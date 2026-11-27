use crate::http_engine::{ProbeResult, apply_custom_headers, probe_url};
use crate::model::{FileCategory, GrabbedLink};
use regex::Regex;
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const GOFILE_WT_LANG: &str = "en-US";
const GOFILE_WT_SALT_DEFAULT: &str = "12af056dacea0b";
const GOFILE_WT_WINDOW_SECS: u64 = 14_400;

#[derive(Debug, Clone)]
pub struct MultiPartMatch {
    pub package_key: String,
    pub part_number: u32,
    pub is_first_part: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedDirectDownload {
    pub url: String,
    pub probe: ProbeResult,
    pub headers: HashMap<String, String>,
}

static RE_PART_RAR: OnceLock<Regex> = OnceLock::new();
static RE_DOT_NUM: OnceLock<Regex> = OnceLock::new();
static RE_ZIP_Z: OnceLock<Regex> = OnceLock::new();
static RE_RAR_R: OnceLock<Regex> = OnceLock::new();
static RE_URL: OnceLock<Regex> = OnceLock::new();

pub fn extract_urls_from_text(input: &str) -> Vec<String> {
    let re = RE_URL.get_or_init(|| {
        Regex::new(r#"(?i)(https?://[^\s"'<>\]\)]+|magnet:\?[^\s"'<>]+)"#).unwrap()
    });
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in re.find_iter(input) {
        let cleaned = m
            .as_str()
            .trim_end_matches(|c: char| matches!(c, ',' | ';' | '.' | ')' | ']' | '"'));
        if seen.insert(cleaned.to_string()) {
            out.push(cleaned.to_string());
        }
    }
    out
}

/// Detects JDownloader-2-style multi-part archive filenames:
/// - `archive.part01.rar`, `archive.part1.rar`
/// - `archive.7z.001`, `archive.zip.001`, `archive.tar.gz.001`, `archive.001`
/// - `archive.z01` .. `archive.z99` + `archive.zip`
/// - `archive.r00` .. `archive.r99` + `archive.rar`
pub fn detect_multipart(filename: &str) -> Option<MultiPartMatch> {
    let re_part_rar = RE_PART_RAR.get_or_init(|| {
        Regex::new(r"(?i)^(.+?)\.part(\d+)(?:\s*\(\d+\))?\.rar$").unwrap()
    });
    if let Some(caps) = re_part_rar.captures(filename) {
        let base = caps.get(1)?.as_str().to_string();
        let num: u32 = caps.get(2)?.as_str().parse().ok()?;
        return Some(MultiPartMatch {
            package_key: format!("{base}.rar"),
            part_number: num,
            is_first_part: num == 1,
        });
    }

    let re_dot_num = RE_DOT_NUM.get_or_init(|| {
        Regex::new(r"(?i)^(.+?(?:\.(?:7z|zip|rar|tar|gz|xz|zst))?)\.(\d{3,4})(?:\s*\(\d+\))?$")
            .unwrap()
    });
    if let Some(caps) = re_dot_num.captures(filename) {
        let base = caps.get(1)?.as_str().to_string();
        let num: u32 = caps.get(2)?.as_str().parse().ok()?;
        return Some(MultiPartMatch {
            package_key: base,
            part_number: num,
            is_first_part: num == 1,
        });
    }

    let re_zip_z = RE_ZIP_Z.get_or_init(|| Regex::new(r"(?i)^(.+?)\.z(\d{2})$").unwrap());
    if let Some(caps) = re_zip_z.captures(filename) {
        let base = caps.get(1)?.as_str().to_string();
        let num: u32 = caps.get(2)?.as_str().parse().ok()?;
        return Some(MultiPartMatch {
            package_key: format!("{base}.zip"),
            part_number: num,
            is_first_part: false, // .zip is the main/final entry for split zip
        });
    }

    let re_rar_r = RE_RAR_R.get_or_init(|| Regex::new(r"(?i)^(.+?)\.r(\d{2})$").unwrap());
    if let Some(caps) = re_rar_r.captures(filename) {
        let base = caps.get(1)?.as_str().to_string();
        let num: u32 = caps.get(2)?.as_str().parse().ok()?;
        return Some(MultiPartMatch {
            package_key: format!("{base}.rar"),
            part_number: num + 2, // .rar is part 1, .r00 is part 2
            is_first_part: false,
        });
    }

    None
}

pub fn fallback_filename_from_url(url_str: &str) -> String {
    if url_str.starts_with("magnet:?") {
        if let Ok(parsed) = url::Url::parse(url_str) {
            for (k, v) in parsed.query_pairs() {
                if k == "dn" && !v.trim().is_empty() {
                    return sanitize_filename(&v);
                }
            }
        }
        return "torrent_magnet.torrent".to_string();
    }
    if let Ok(parsed) = url::Url::parse(url_str) {
        if let Some(mut segs) = parsed.path_segments() {
            if let Some(last) = segs.next_back() {
                let decoded = url_decode_lossy(last);
                let clean = sanitize_filename(&decoded);
                if !clean.is_empty() {
                    return clean;
                }
            }
        }
        if let Some(host) = parsed.host_str() {
            return format!("{}.bin", sanitize_filename(host));
        }
    }
    "download.bin".to_string()
}

pub fn url_decode_lossy(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let raw = input.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let Ok(val) = u8::from_str_radix(
                std::str::from_utf8(&raw[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                bytes.push(val);
                i += 3;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn sanitize_filename(name: &str) -> String {
    let trimmed = name.trim().trim_matches('"');
    let replaced: String = trimmed
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0'..='\x1f' => '_',
            _ => c,
        })
        .collect();
    let trimmed_dots = replaced.trim_matches(|c| c == '.' || c == ' ');
    if trimmed_dots.is_empty() {
        "download.bin".to_string()
    } else {
        trimmed_dots.to_string()
    }
}

pub fn is_html_probe(info: &ProbeResult) -> bool {
    if info.has_content_disposition {
        return false;
    }
    matches!(
        info.content_type.as_deref(),
        Some("text/html") | Some("application/xhtml+xml")
    )
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

pub fn is_vikingfile_host(host: &str) -> bool {
    let low = host.trim().to_ascii_lowercase();
    let stripped = low.trim_end_matches('.');
    stripped.split('.').any(|label| {
        matches!(
            label,
            "vikingfile" | "vik1ngfile" | "vikingf1le" | "vik1ngf1le"
        )
    })
}

pub fn is_fileditch_host(host: &str) -> bool {
    [
        "fileditch.com",
        "fileditchfiles.me",
        "fileditchfiles.st",
        "fileditchvids.st",
        "itsfileditchfiles.st",
        "fileditchhot.st",
        "theditch.st",
        "embedditch.st",
    ]
    .iter()
    .any(|d| host_matches(host, d))
}

pub fn classify_source_site(url_str: &str) -> Option<String> {
    if url_str.starts_with("magnet:?") {
        return Some("Magnet".to_string());
    }
    let parsed = url::Url::parse(url_str).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if is_fileditch_host(&host) {
        return Some("FileDitch".to_string());
    }
    let known: &[(&[&str], &str)] = &[
        (&["filekeeper.net"], "FileKeeper"),
        (&["pixeldrain.com"], "Pixeldrain"),
        (&["gofile.io"], "Gofile"),
        (&["vikingfile.com", "vik1ngfile.site", "vik1ngf1le.online"], "VikingFile"),
        (&["mediafire.com"], "MediaFire"),
        (&["1fichier.com"], "1fichier"),
        (&["buzzheavier.com", "bzzhr.co"], "Buzzheavier"),
        (&["qiwi.gg"], "Qiwi"),
        (&["krakenfiles.com"], "Krakenfiles"),
        (&["mega.nz", "mega.co.nz"], "MEGA"),
        (&["akirabox.com"], "Akirabox"),
        (&["send.cm"], "Send.cm"),
        (&["uploadhaven.com"], "UploadHaven"),
        (&["datanodes.to"], "Datanodes"),
        (&["ranoz.gg"], "Ranoz"),
        (&["bowfile.com"], "Bowfile"),
        (&["fuckingfast.co"], "FuckingFast"),
        (&["rootz.so"], "Rootz"),
        (&["github.com", "githubusercontent.com"], "GitHub"),
        (&["drive.google.com"], "Google Drive"),
        (&["dropbox.com"], "Dropbox"),
        (&["1drv.ms", "onedrive.live.com"], "OneDrive"),
        (&["huggingface.co"], "HuggingFace"),
        (&["archive.org"], "Archive.org"),
    ];
    if is_vikingfile_host(&host) {
        return Some("VikingFile".to_string());
    }
    for (domains, label) in known {
        if domains.iter().any(|d| host_matches(&host, d)) {
            return Some((*label).to_string());
        }
    }
    let stripped = host.strip_prefix("www.").unwrap_or(&host);
    let parts: Vec<&str> = stripped.split('.').collect();
    if parts.len() >= 2 {
        Some(parts[parts.len() - 2..].join("."))
    } else {
        Some(stripped.to_string())
    }
}

fn is_known_file_hoster_host(host: &str) -> bool {
    if is_vikingfile_host(host) || is_fileditch_host(host) {
        return true;
    }
    [
        "pixeldrain.com",
        "gofile.io",
        "vikingfile.com",
        "vik1ngfile.site",
        "filekeeper.net",
        "mediafire.com",
        "1fichier.com",
        "buzzheavier.com",
        "bzzhr.co",
        "qiwi.gg",
        "krakenfiles.com",
        "mega.nz",
        "mega.co.nz",
        "akirabox.com",
        "send.cm",
        "uploadhaven.com",
        "datanodes.to",
        "ranoz.gg",
        "bowfile.com",
        "fuckingfast.co",
        "rootz.so",
    ]
    .iter()
    .any(|d| host_matches(host, d))
}

fn is_known_file_hoster(url_str: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    let Some(host) = parsed.host_str().map(|h| h.to_ascii_lowercase()) else {
        return false;
    };
    is_known_file_hoster_host(&host)
}

fn pick_source_site(original_url: &str, resolved_url: &str) -> Option<String> {
    if is_known_file_hoster(original_url) {
        return classify_source_site(original_url);
    }
    if is_known_file_hoster(resolved_url) {
        return classify_source_site(resolved_url);
    }
    classify_source_site(original_url).or_else(|| classify_source_site(resolved_url))
}

pub fn parse_human_size(text: &str) -> Option<u64> {
    static RE_SIZE: OnceLock<Regex> = OnceLock::new();
    let re = RE_SIZE.get_or_init(|| {
        Regex::new(r"(?i)(\d+(?:[.,]\d+)?)\s*(tib|tb|gib|gb|mib|mb|kib|kb|bytes|b)\b").unwrap()
    });
    let caps = re.captures(text)?;
    let num_str = caps.get(1)?.as_str().replace(',', ".");
    let value: f64 = num_str.parse().ok()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let unit = caps.get(2)?.as_str().to_ascii_lowercase();
    let mult: f64 = match unit.as_str() {
        "b" | "bytes" => 1.0,
        "kb" => 1_000.0,
        "kib" => 1024.0,
        "mb" => 1_000_000.0,
        "mib" => 1024.0 * 1024.0,
        "gb" => 1_000_000_000.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" => 1_000_000_000_000.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * mult).round() as u64)
}

pub fn compute_gofile_website_token(
    user_agent: &str,
    lang: &str,
    account_token: &str,
    unix_secs: u64,
    salt: &str,
) -> String {
    let window = unix_secs / GOFILE_WT_WINDOW_SECS;
    let payload = format!("{user_agent}::{lang}::{account_token}::{window}::{salt}");
    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    hex::encode(hasher.finalize())
}

fn has_downloadable_file_extension(url_str: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    let path = parsed.path().to_ascii_lowercase();
    let last = path.rsplit('/').next().unwrap_or("");
    let exts = [
        ".rar", ".zip", ".7z", ".tar", ".gz", ".tgz", ".xz", ".bz2", ".zst", ".iso", ".img",
        ".exe", ".msi", ".apk", ".deb", ".rpm", ".dmg", ".mp4", ".mkv", ".webm", ".avi", ".mov",
        ".mp3", ".flac", ".wav", ".pdf", ".epub", ".bin", ".torrent",
    ];
    if exts.iter().any(|e| last.ends_with(e)) {
        return true;
    }
    if let Some((_, ext)) = last.rsplit_once('.') {
        return ext.len() == 3 && ext.chars().all(|c| c.is_ascii_digit());
    }
    false
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn strip_html_tags(input: &str) -> String {
    static RE_TAG: OnceLock<Regex> = OnceLock::new();
    let re = RE_TAG.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    decode_html_entities(&re.replace_all(input, " "))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_cloudflare_challenge(html: &str) -> bool {
    let low = html.to_ascii_lowercase();
    (low.contains("just a moment...") || low.contains("cf-browser-verification"))
        && (low.contains("cloudflare") || low.contains("turnstile"))
}

fn is_fileditch_pow_challenge(html: &str) -> bool {
    let low = html.to_ascii_lowercase();
    low.contains("powwasm.js")
        || low.contains("verifying-overlay")
        || low.contains("verification can&#039;t run")
        || low.contains("verification can't run")
        || low.contains("webassembly is disabled")
}

fn extract_html_download_candidates(base_url: &str, html: &str) -> Vec<(i32, String)> {
    let Ok(base) = url::Url::parse(base_url) else {
        return Vec::new();
    };

    static RE_META_REFRESH: OnceLock<Regex> = OnceLock::new();
    static RE_ANCHOR: OnceLock<Regex> = OnceLock::new();
    static RE_ATTR: OnceLock<Regex> = OnceLock::new();
    static RE_JS_URL: OnceLock<Regex> = OnceLock::new();

    let re_meta = RE_META_REFRESH.get_or_init(|| {
        Regex::new(r#"(?is)<meta[^>]+http-equiv\s*=\s*["']?refresh["']?[^>]+content\s*=\s*["'][^"']*url\s*=\s*([^"'>\s]+)"#).unwrap()
    });
    let re_anchor = RE_ANCHOR.get_or_init(|| {
        Regex::new(r#"(?is)<(?:a|button)\b([^>]*)>(.*?)</(?:a|button)>"#).unwrap()
    });
    let re_attr = RE_ATTR.get_or_init(|| {
        Regex::new(r#"(?is)\b(href|data-url|data-href|data-link|data-clipboard-text|onclick)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap()
    });
    let re_js_url = RE_JS_URL.get_or_init(|| {
        Regex::new(r#"(?i)(?:downloadUrl|download_url|fileUrl|directUrl|window\.location(?:\.href)?\s*=|location\.replace\(|writeText\()\s*[:=]?\s*["'](https?://[^"'\s]+)["']"#).unwrap()
    });

    let mut scored: HashMap<String, i32> = HashMap::new();
    let mut push_candidate = |raw: &str, score: i32| {
        let cleaned = decode_html_entities(raw.trim());
        if cleaned.is_empty()
            || cleaned.starts_with('#')
            || cleaned.starts_with("javascript:")
            || cleaned.starts_with("mailto:")
        {
            return;
        }
        let Ok(resolved) = base.join(&cleaned) else {
            return;
        };
        if resolved.scheme() != "http" && resolved.scheme() != "https" {
            return;
        }
        let s = resolved.to_string();
        if s == base_url {
            return;
        }
        let low = s.to_ascii_lowercase();
        let path_low = resolved.path().to_ascii_lowercase();
        if [
            ".css", ".js", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".ico", ".woff", ".woff2",
        ]
        .iter()
        .any(|ext| path_low.ends_with(ext))
        {
            return;
        }
        let noise = [
            "/plans",
            "/pricing",
            "/login",
            "/register",
            "/signup",
            "/report",
            "/dmca",
            "/terms",
            "/privacy",
            "/tos",
            "discord.gg",
            "discord.com/invite",
            "t.me/",
        ];
        if noise.iter().any(|n| low.contains(n)) {
            return;
        }
        let entry = scored.entry(s).or_insert(i32::MIN);
        if score > *entry {
            *entry = score;
        }
    };

    for cap in re_meta.captures_iter(html) {
        if let Some(m) = cap.get(1) {
            push_candidate(m.as_str(), 90);
        }
    }

    for cap in re_js_url.captures_iter(html) {
        if let Some(m) = cap.get(1) {
            push_candidate(m.as_str(), 85);
        }
    }

    for cap in re_anchor.captures_iter(html) {
        let attrs = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let inner_text = strip_html_tags(cap.get(2).map(|m| m.as_str()).unwrap_or(""));
        let attrs_low = attrs.to_ascii_lowercase();
        let text_low = inner_text.to_ascii_lowercase();

        let has_download_attr = regex::Regex::new(r#"(?i)\bdownload(?:\s*=|\s|$)"#)
            .map(|r| r.is_match(&attrs_low))
            .unwrap_or(false);

        let mut base_score = 0i32;
        if has_download_attr {
            base_score += 100;
        }
        if text_low.contains("jdownloader")
            || text_low.contains("fdm")
            || text_low.contains("link de download")
            || text_low.contains("direct download")
        {
            base_score += 95;
        } else if text_low.contains("baixar")
            || text_low.contains("download")
            || text_low.contains("get file")
            || text_low.contains("click here to download")
        {
            base_score += 75;
        }
        if attrs_low.contains("download") || attrs_low.contains("dl-btn") {
            base_score += 35;
        }
        if text_low.contains("planos")
            || text_low.contains("pricing")
            || text_low.contains("denunciar")
            || text_low.contains("report")
            || text_low.contains("premium")
        {
            base_score -= 150;
        }

        let mut urls_in_tag: Vec<String> = Vec::new();
        for ac in re_attr.captures_iter(attrs) {
            let attr_name = ac.get(1).map(|m| m.as_str().to_ascii_lowercase()).unwrap_or_default();
            let val = ac
                .get(2)
                .or_else(|| ac.get(3))
                .map(|m| m.as_str())
                .unwrap_or("");
            if attr_name == "onclick" {
                for u in extract_urls_from_text(val) {
                    urls_in_tag.push(u);
                }
            } else {
                urls_in_tag.push(val.to_string());
            }
        }

        for u in urls_in_tag {
            let mut s = base_score;
            let u_low = u.to_ascii_lowercase();
            if has_downloadable_file_extension(&u_low) {
                s += 70;
            }
            if u_low.contains("/dl/")
                || u_low.contains("/download")
                || u_low.contains("?download")
                || u_low.contains("/api/file/")
            {
                s += 45;
            }
            if let Ok(parsed_cand) = base.join(&u) {
                if parsed_cand.host_str() != base.host_str() && parsed_cand.path().len() > 24 {
                    s += 40;
                }
            }
            if s >= 50 {
                push_candidate(&u, s);
            }
        }
    }

    let mut out: Vec<(i32, String)> = scored.into_iter().map(|(u, s)| (s, u)).collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

fn extract_html_index_links(base_url: &str, html: &str) -> Vec<String> {
    let Ok(base) = url::Url::parse(base_url) else {
        return Vec::new();
    };
    static RE_ANCHOR_INDEX: OnceLock<Regex> = OnceLock::new();
    static RE_HREF: OnceLock<Regex> = OnceLock::new();
    let re_anchor = RE_ANCHOR_INDEX
        .get_or_init(|| Regex::new(r#"(?is)<a\b([^>]*)>(.*?)</a>"#).unwrap());
    let re_href = RE_HREF
        .get_or_init(|| Regex::new(r#"(?is)\bhref\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for cap in re_anchor.captures_iter(html) {
        let attrs = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let text = strip_html_tags(cap.get(2).map(|m| m.as_str()).unwrap_or(""))
            .to_ascii_lowercase();
        let Some(hcap) = re_href.captures(attrs) else {
            continue;
        };
        let raw_href = hcap
            .get(1)
            .or_else(|| hcap.get(2))
            .map(|m| decode_html_entities(m.as_str().trim()))
            .unwrap_or_default();
        if raw_href.is_empty() || raw_href.starts_with('#') || raw_href.starts_with("javascript:") {
            continue;
        }
        let Ok(resolved) = base.join(&raw_href) else {
            continue;
        };
        if resolved.scheme() != "http" && resolved.scheme() != "https" {
            continue;
        }
        let s = resolved.to_string();
        if s == base_url {
            continue;
        }
        let looks_like_download_target = is_known_file_hoster(&s)
            || has_downloadable_file_extension(&s)
            || text == "download"
            || text == "baixar"
            || text.contains("part")
            || text.contains(".rar")
            || text.contains(".zip")
            || text.contains(".7z");
        if looks_like_download_target && seen.insert(s.clone()) {
            out.push(s);
            if out.len() >= 40 {
                break;
            }
        }
    }
    out
}

async fn fetch_html_limited(
    client: &Client,
    url: &str,
    headers: &HashMap<String, String>,
) -> anyhow::Result<(String, String)> {
    let resp = apply_custom_headers(client.get(url), headers)
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    let final_url = resp.url().to_string();
    let bytes = resp.bytes().await?;
    let max_len = bytes.len().min(2 * 1024 * 1024);
    Ok((
        final_url,
        String::from_utf8_lossy(&bytes[..max_len]).into_owned(),
    ))
}

enum SpecialResolveOutcome {
    DirectCandidates(Vec<(String, HashMap<String, String>, Option<String>, Option<u64>)>),
    StaticLinks(Vec<GrabbedLink>),
}

fn no_redirect_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(12))
            .timeout(Duration::from_secs(20))
            .build()
            .expect("failed to build no-redirect HTTP client")
    })
}

fn extract_html_attr(tag: &str, attr_name: &str) -> Option<String> {
    let pattern = format!(
        r#"(?is)\b{}\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
        regex::escape(attr_name)
    );
    let re = Regex::new(&pattern).ok()?;
    let caps = re.captures(tag)?;
    caps.get(1)
        .or_else(|| caps.get(2))
        .or_else(|| caps.get(3))
        .map(|m| decode_html_entities(m.as_str().trim()))
}

fn extract_tag_inner_by_id(html: &str, id_val: &str) -> Option<String> {
    let pattern = format!(
        r#"(?is)<[a-z0-9]+\b[^>]*\bid\s*=\s*["']{}["'][^>]*>(.*?)</[a-z0-9]+>"#,
        regex::escape(id_val)
    );
    let re = Regex::new(&pattern).ok()?;
    let caps = re.captures(html)?;
    let text = strip_html_tags(caps.get(1)?.as_str());
    if text.is_empty() { None } else { Some(text) }
}

struct FileKeeperFormData {
    countdown: u64,
    code: String,
    rand: String,
    referer: String,
    method: String,
    has_captcha: bool,
    has_password: bool,
    post_action: Option<String>,
    hint_name: Option<String>,
    hint_size: Option<u64>,
}

fn parse_filekeeper_html(url: &str, html: &str) -> Option<FileKeeperFormData> {
    static RE_COUNTDOWN_TAG: OnceLock<Regex> = OnceLock::new();
    static RE_FORM_TAG: OnceLock<Regex> = OnceLock::new();
    static RE_INPUT_TAG: OnceLock<Regex> = OnceLock::new();
    static RE_H1: OnceLock<Regex> = OnceLock::new();
    static RE_TITLE: OnceLock<Regex> = OnceLock::new();

    let re_cd = RE_COUNTDOWN_TAG
        .get_or_init(|| Regex::new(r#"(?is)<[^>]*\bid\s*=\s*["']download-countdown["'][^>]*>"#).unwrap());
    let re_form = RE_FORM_TAG.get_or_init(|| Regex::new(r#"(?is)<form\b[^>]*>"#).unwrap());
    let re_input = RE_INPUT_TAG.get_or_init(|| Regex::new(r#"(?is)<input\b[^>]*>"#).unwrap());
    let re_h1 = RE_H1.get_or_init(|| Regex::new(r#"(?is)<h1\b[^>]*>(.*?)</h1>"#).unwrap());
    let re_title = RE_TITLE.get_or_init(|| Regex::new(r#"(?is)<title\b[^>]*>(.*?)</title>"#).unwrap());

    let mut countdown = 0u64;
    let mut code = String::new();
    let mut rand = String::new();
    let mut referer = url.to_string();
    let mut method = String::new();
    let mut has_captcha = false;
    let mut has_password = false;

    if let Some(m) = re_cd.find(html) {
        let tag = m.as_str();
        if let Some(v) = extract_html_attr(tag, "data-countdown") {
            countdown = v.parse::<u64>().unwrap_or(0);
        }
        if let Some(v) = extract_html_attr(tag, "data-code") {
            code = v;
        }
        if let Some(v) = extract_html_attr(tag, "data-rand") {
            rand = v;
        }
        if let Some(v) = extract_html_attr(tag, "data-referer") {
            if !v.is_empty() {
                referer = v;
            }
        }
        if let Some(v) = extract_html_attr(tag, "data-method") {
            method = v;
        }
        if let Some(v) = extract_html_attr(tag, "data-has-captcha") {
            has_captcha = v.eq_ignore_ascii_case("true") || v == "1";
        }
        if let Some(v) = extract_html_attr(tag, "data-has-password") {
            has_password = v.eq_ignore_ascii_case("true") || v == "1";
        }
    }

    for m in re_input.find_iter(html) {
        let tag = m.as_str();
        let Some(name) = extract_html_attr(tag, "name") else {
            continue;
        };
        let val = extract_html_attr(tag, "value").unwrap_or_default();
        if name.eq_ignore_ascii_case("id") && code.is_empty() && !val.is_empty() {
            code = val;
        } else if name.eq_ignore_ascii_case("rand") && rand.is_empty() && !val.is_empty() {
            rand = val;
        } else if name.eq_ignore_ascii_case("method_free") && method.is_empty() && !val.is_empty() {
            method = val;
        } else if name.eq_ignore_ascii_case("referer") && referer == url && !val.is_empty() {
            referer = val;
        }
    }

    if code.is_empty() || rand.is_empty() {
        return None;
    }

    let mut post_action = None;
    for m in re_form.find_iter(html) {
        let tag = m.as_str();
        if let Some(act) = extract_html_attr(tag, "action") {
            if !act.is_empty() {
                post_action = Some(act);
                break;
            }
        }
    }

    let hint_name = re_h1
        .captures(html)
        .and_then(|c| {
            let t = strip_html_tags(c.get(1)?.as_str());
            if t.is_empty() || t.to_ascii_lowercase().contains("filekeeper") {
                None
            } else {
                Some(t)
            }
        })
        .or_else(|| {
            re_title.captures(html).and_then(|c| {
                let t = strip_html_tags(c.get(1)?.as_str());
                let trimmed = t
                    .trim_start_matches("Download ")
                    .split(" - FileKeeper")
                    .next()
                    .unwrap_or(&t)
                    .trim();
                if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("FileKeeper") {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
        });

    let hint_size = parse_human_size(html);

    Some(FileKeeperFormData {
        countdown,
        code,
        rand,
        referer,
        method,
        has_captcha,
        has_password,
        post_action,
        hint_name,
        hint_size,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct VikingFileMeta {
    filename: Option<String>,
    size_bytes: Option<u64>,
    has_turnstile: bool,
    direct_link: Option<String>,
}

fn parse_vikingfile_html(base_url: &str, html: &str) -> VikingFileMeta {
    let filename = extract_tag_inner_by_id(html, "filename");
    let size_bytes = extract_tag_inner_by_id(html, "size").and_then(|s| parse_human_size(&s));
    let low = html.to_ascii_lowercase();
    let has_turnstile = low.contains("captcha-download")
        || low.contains("challenges.cloudflare.com/turnstile")
        || low.contains("cf-turnstile");

    let mut direct_link = None;
    if let Ok(base) = url::Url::parse(base_url) {
        static RE_DL_LINK: OnceLock<Regex> = OnceLock::new();
        let re = RE_DL_LINK.get_or_init(|| {
            Regex::new(r#"(?is)<a\b[^>]*\bid\s*=\s*["']download-link["'][^>]*>"#).unwrap()
        });
        if let Some(m) = re.find(html) {
            if let Some(href) = extract_html_attr(m.as_str(), "href") {
                if !href.is_empty() && href != "#" && !href.starts_with("javascript:") {
                    if let Ok(abs) = base.join(&href) {
                        direct_link = Some(abs.to_string());
                    }
                }
            }
        }
    }

    VikingFileMeta {
        filename,
        size_bytes,
        has_turnstile,
        direct_link,
    }
}

fn collect_gofile_children_urls(
    children: &serde_json::Map<String, serde_json::Value>,
    token: &str,
    base_headers: &HashMap<String, String>,
    out: &mut Vec<(String, HashMap<String, String>, Option<String>, Option<u64>)>,
) {
    for (_k, child) in children {
        let kind = child.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if kind == "file" {
            if let Some(link) = child.get("link").and_then(|l| l.as_str()) {
                let mut h = base_headers.clone();
                h.insert("Cookie".to_string(), format!("accountToken={token}"));
                h.insert("User-Agent".to_string(), BROWSER_UA.to_string());
                h.entry("Referer".to_string())
                    .or_insert_with(|| "https://gofile.io/".to_string());
                let name = child
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(str::to_string);
                let size = child.get("size").and_then(|s| s.as_u64());
                out.push((link.to_string(), h, name, size));
            }
        } else if kind == "folder" {
            if let Some(sub_children) = child.get("children").and_then(|c| c.as_object()) {
                collect_gofile_children_urls(sub_children, token, base_headers, out);
            }
        }
    }
}

fn build_static_error_link(
    url: String,
    filename: String,
    size_bytes: Option<u64>,
    source_site: Option<String>,
    headers: HashMap<String, String>,
    error: String,
) -> GrabbedLink {
    let clean = sanitize_filename(&filename);
    let mp = detect_multipart(&clean);
    let category = FileCategory::from_filename(&clean);
    GrabbedLink {
        id: Uuid::new_v4(),
        url,
        filename: clean,
        size_bytes,
        online: Some(false),
        probing: false,
        supports_ranges: false,
        category,
        package_key: mp.as_ref().map(|m| m.package_key.clone()),
        part_number: mp.as_ref().map(|m| m.part_number),
        part_total: None,
        source_site,
        selected: false,
        headers,
        error: Some(error),
    }
}

async fn try_special_hoster_resolve(
    client: &Client,
    url: &str,
    headers: &HashMap<String, String>,
) -> Option<SpecialResolveOutcome> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let segs: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();

    // Pixeldrain single file or list
    if host_matches(&host, "pixeldrain.com") {
        if segs.first() == Some(&"u") && segs.len() >= 2 {
            let id = segs[1];
            let dl_url = format!("https://pixeldrain.com/api/file/{id}?download");
            let mut title = None;
            let mut size = None;
            if let Ok(resp) = client
                .get(format!("https://pixeldrain.com/api/file/{id}/info"))
                .timeout(Duration::from_secs(10))
                .send()
                .await
            {
                if let Ok(val) = resp.json::<serde_json::Value>().await {
                    if val.get("success").and_then(|v| v.as_bool()) == Some(false) {
                        let msg = val
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Arquivo não encontrado no Pixeldrain");
                        return Some(SpecialResolveOutcome::StaticLinks(vec![
                            build_static_error_link(
                                url.to_string(),
                                fallback_filename_from_url(url),
                                None,
                                Some("Pixeldrain".to_string()),
                                headers.clone(),
                                msg.to_string(),
                            ),
                        ]));
                    }
                    title = val.get("name").and_then(|v| v.as_str()).map(str::to_string);
                    size = val.get("size").and_then(|v| v.as_u64());
                }
            }
            return Some(SpecialResolveOutcome::DirectCandidates(vec![(
                dl_url,
                headers.clone(),
                title,
                size,
            )]));
        }
        if segs.first() == Some(&"l") && segs.len() >= 2 {
            let id = segs[1];
            if let Ok(resp) = client
                .get(format!("https://pixeldrain.com/api/list/{id}"))
                .timeout(Duration::from_secs(12))
                .send()
                .await
            {
                if let Ok(val) = resp.json::<serde_json::Value>().await {
                    if let Some(files) = val.get("files").and_then(|v| v.as_array()) {
                        let mut list = Vec::new();
                        for f in files {
                            if let Some(fid) = f.get("id").and_then(|v| v.as_str()) {
                                let name =
                                    f.get("name").and_then(|v| v.as_str()).map(str::to_string);
                                let size = f.get("size").and_then(|v| v.as_u64());
                                list.push((
                                    format!("https://pixeldrain.com/api/file/{fid}?download"),
                                    headers.clone(),
                                    name,
                                    size,
                                ));
                            }
                        }
                        if !list.is_empty() {
                            return Some(SpecialResolveOutcome::DirectCandidates(list));
                        }
                    }
                }
            }
        }
    }

    // Gofile public folder (dynamic X-Website-Token)
    if host_matches(&host, "gofile.io") && segs.first() == Some(&"d") && segs.len() >= 2 {
        let folder_id = segs[1];
        if let Ok(acc_resp) = client
            .post("https://api.gofile.io/accounts")
            .header("User-Agent", BROWSER_UA)
            .header("Origin", "https://gofile.io")
            .header("Referer", "https://gofile.io/")
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            if let Ok(acc_json) = acc_resp.json::<serde_json::Value>().await {
                if let Some(token) = acc_json
                    .get("data")
                    .and_then(|d| d.get("token"))
                    .and_then(|t| t.as_str())
                {
                    let salt = std::env::var("GOFILE_WT_SALT")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| GOFILE_WT_SALT_DEFAULT.to_string());
                    let now_secs = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let api_url = format!(
                        "https://api.gofile.io/contents/{folder_id}?contentFilter=&page=1&pageSize=1000&sortField=createTime&sortDirection=-1"
                    );

                    let mut offset_window = 0u64;
                    for attempt in 0..3 {
                        let wt = compute_gofile_website_token(
                            BROWSER_UA,
                            GOFILE_WT_LANG,
                            token,
                            now_secs.saturating_sub(offset_window * GOFILE_WT_WINDOW_SECS),
                            &salt,
                        );
                        if let Ok(cont_resp) = client
                            .get(&api_url)
                            .header("User-Agent", BROWSER_UA)
                            .header("Authorization", format!("Bearer {token}"))
                            .header("X-Website-Token", wt)
                            .header("X-BL", GOFILE_WT_LANG)
                            .header("Origin", "https://gofile.io")
                            .header("Referer", "https://gofile.io/")
                            .timeout(Duration::from_secs(12))
                            .send()
                            .await
                        {
                            if let Ok(cont_json) = cont_resp.json::<serde_json::Value>().await {
                                let status = cont_json
                                    .get("status")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("");
                                if status == "ok" {
                                    if let Some(children) = cont_json
                                        .get("data")
                                        .and_then(|d| d.get("children"))
                                        .and_then(|c| c.as_object())
                                    {
                                        let mut out = Vec::new();
                                        collect_gofile_children_urls(
                                            children,
                                            token,
                                            headers,
                                            &mut out,
                                        );
                                        if !out.is_empty() {
                                            return Some(SpecialResolveOutcome::DirectCandidates(
                                                out,
                                            ));
                                        }
                                        return Some(SpecialResolveOutcome::StaticLinks(vec![
                                            build_static_error_link(
                                                url.to_string(),
                                                format!("gofile_{folder_id}"),
                                                None,
                                                Some("Gofile".to_string()),
                                                headers.clone(),
                                                "Pasta Gofile vazia".to_string(),
                                            ),
                                        ]));
                                    }
                                } else if status == "error-notPremium" && offset_window == 0 {
                                    offset_window = 1;
                                    continue;
                                } else if status == "error-rateLimit" && attempt < 2 {
                                    tokio::time::sleep(Duration::from_millis(1500)).await;
                                    continue;
                                } else if status == "error-notFound"
                                    || status == "error-notPublic"
                                    || status == "error-passwordRequired"
                                {
                                    let msg = match status {
                                        "error-notFound" => {
                                            "Pasta/arquivo Gofile não encontrado ou expirado"
                                        }
                                        "error-notPublic" => "Pasta Gofile privada",
                                        "error-passwordRequired" => {
                                            "Pasta Gofile protegida por senha"
                                        }
                                        _ => status,
                                    };
                                    return Some(SpecialResolveOutcome::StaticLinks(vec![
                                        build_static_error_link(
                                            url.to_string(),
                                            format!("gofile_{folder_id}"),
                                            None,
                                            Some("Gofile".to_string()),
                                            headers.clone(),
                                            msg.to_string(),
                                        ),
                                    ]));
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    // FileKeeper landing page -> countdown + POST download2 -> 302 Location
    if host_matches(&host, "filekeeper.net") {
        let mut get_headers = headers.clone();
        get_headers
            .entry("User-Agent".to_string())
            .or_insert_with(|| BROWSER_UA.to_string());
        if let Some(code_guess) = segs.first().filter(|&&s| s != "download") {
            let ck = format!("lang=english; file_code={code_guess}");
            get_headers.entry("Cookie".to_string()).or_insert(ck);
        }
        if let Ok((page_url, html)) = fetch_html_limited(client, url, &get_headers).await {
            let low = html.to_ascii_lowercase();
            if low.contains("file not found")
                || low.contains("file was deleted")
                || low.contains("could not be found")
                || low.contains("has been removed")
            {
                return Some(SpecialResolveOutcome::StaticLinks(vec![
                    build_static_error_link(
                        url.to_string(),
                        fallback_filename_from_url(url),
                        None,
                        Some("FileKeeper".to_string()),
                        headers.clone(),
                        "Arquivo não encontrado ou expirado no FileKeeper".to_string(),
                    ),
                ]));
            }
            if let Some(form) = parse_filekeeper_html(&page_url, &html) {
                let fallback_name = form
                    .hint_name
                    .clone()
                    .unwrap_or_else(|| fallback_filename_from_url(url));
                if form.has_captcha {
                    return Some(SpecialResolveOutcome::StaticLinks(vec![
                        build_static_error_link(
                            url.to_string(),
                            fallback_name,
                            form.hint_size,
                            Some("FileKeeper".to_string()),
                            headers.clone(),
                            "FileKeeper exigiu captcha — clique em Download no navegador"
                                .to_string(),
                        ),
                    ]));
                }
                if form.has_password {
                    return Some(SpecialResolveOutcome::StaticLinks(vec![
                        build_static_error_link(
                            url.to_string(),
                            fallback_name,
                            form.hint_size,
                            Some("FileKeeper".to_string()),
                            headers.clone(),
                            "Arquivo protegido por senha no FileKeeper".to_string(),
                        ),
                    ]));
                }
                if form.countdown > 0 {
                    tokio::time::sleep(Duration::from_secs((form.countdown + 1).min(16))).await;
                }
                let post_url = form
                    .post_action
                    .as_deref()
                    .and_then(|act| {
                        url::Url::parse(&page_url)
                            .ok()
                            .and_then(|b| b.join(act).ok())
                    })
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| page_url.clone());

                let encoded_body = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("op", "download2")
                    .append_pair("id", form.code.as_str())
                    .append_pair("rand", form.rand.as_str())
                    .append_pair("referer", form.referer.as_str())
                    .append_pair("method_free", form.method.as_str())
                    .append_pair("down_direct", "1")
                    .finish();

                let cookie_val = format!("lang=english; file_code={}", form.code);
                let mut post_req = no_redirect_client()
                    .post(&post_url)
                    .header("User-Agent", BROWSER_UA)
                    .header("Referer", &page_url)
                    .header("Origin", "https://filekeeper.net")
                    .header("Cookie", &cookie_val)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(encoded_body);
                if let Some(ua) = headers.get("User-Agent") {
                    post_req = post_req.header("User-Agent", ua);
                }

                if let Ok(post_resp) = post_req.send().await {
                    if let Some(loc) = post_resp
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|v| v.to_str().ok())
                    {
                        if let Ok(base_p) = url::Url::parse(&post_url) {
                            if let Ok(direct) = base_p.join(loc) {
                                let mut dl_headers = headers.clone();
                                dl_headers.insert("Referer".to_string(), page_url);
                                dl_headers.insert("User-Agent".to_string(), BROWSER_UA.to_string());
                                return Some(SpecialResolveOutcome::DirectCandidates(vec![(
                                    direct.to_string(),
                                    dl_headers,
                                    form.hint_name,
                                    form.hint_size,
                                )]));
                            }
                        }
                    } else if let Ok(post_html) = post_resp.text().await {
                        let cands = extract_html_download_candidates(&post_url, &post_html);
                        if let Some((_, best)) = cands.into_iter().next() {
                            let mut dl_headers = headers.clone();
                            dl_headers.insert("Referer".to_string(), page_url);
                            return Some(SpecialResolveOutcome::DirectCandidates(vec![(
                                best,
                                dl_headers,
                                form.hint_name,
                                form.hint_size,
                            )]));
                        }
                    }
                }
            }
        }
    }

    // FileDitch Status API for direct path-style links (/<folder>/<id>/<filename>)
    if is_fileditch_host(&host)
        && segs.len() >= 3
        && !parsed.path().contains("file.php")
    {
        let api_host = if host_matches(&host, "fileditchfiles.me") {
            "fileditchfiles.me"
        } else {
            "fileditchfiles.st"
        };
        let api_url = format!("https://{api_host}/api{}", parsed.path());
        let hint_name = segs.last().map(|s| {
            url::form_urlencoded::parse(s.as_bytes())
                .next()
                .map(|(k, _)| k.into_owned())
                .unwrap_or_else(|| (*s).to_string())
        });
        let mut hint_size = None;
        let mut api_status_true = false;
        if let Ok(resp) = client
            .get(&api_url)
            .header("User-Agent", BROWSER_UA)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            if let Ok(val) = resp.json::<serde_json::Value>().await {
                if val.get("status").and_then(|s| s.as_bool()) == Some(false) {
                    return Some(SpecialResolveOutcome::StaticLinks(vec![
                        build_static_error_link(
                            url.to_string(),
                            hint_name.unwrap_or_else(|| fallback_filename_from_url(url)),
                            val.get("size").and_then(|s| s.as_u64()),
                            Some("FileDitch".to_string()),
                            headers.clone(),
                            "Arquivo removido ou expirado no FileDitch".to_string(),
                        ),
                    ]));
                }
                if val.get("status").and_then(|s| s.as_bool()) == Some(true) {
                    api_status_true = true;
                }
                hint_size = val.get("size").and_then(|s| s.as_u64());
            }
        }
        let mut dl_headers = headers.clone();
        dl_headers
            .entry("User-Agent".to_string())
            .or_insert_with(|| BROWSER_UA.to_string());
        if let Ok(info) = probe_url(client, url, &dl_headers).await {
            if !is_html_probe(&info) {
                return Some(SpecialResolveOutcome::DirectCandidates(vec![(
                    url.to_string(),
                    dl_headers,
                    info.filename.or(hint_name),
                    info.total_bytes.or(hint_size),
                )]));
            }
        }
        match fetch_html_limited(client, url, &dl_headers).await {
            Ok((page_url, html)) => {
                let cands = extract_html_download_candidates(&page_url, &html);
                if let Some((_, best)) = cands.into_iter().next() {
                    dl_headers.insert("Referer".to_string(), page_url);
                    return Some(SpecialResolveOutcome::DirectCandidates(vec![(
                        best,
                        dl_headers,
                        hint_name,
                        hint_size,
                    )]));
                }
                if is_fileditch_pow_challenge(&html) {
                    return Some(SpecialResolveOutcome::StaticLinks(vec![
                        build_static_error_link(
                            url.to_string(),
                            hint_name.unwrap_or_else(|| fallback_filename_from_url(url)),
                            hint_size,
                            Some("FileDitch".to_string()),
                            headers.clone(),
                            "FileDitch exige verificação automática no navegador — clique em 'Resolver no Vortex' ou 'Resolver no Navegador'"
                                .to_string(),
                        ),
                    ]));
                }
                if is_cloudflare_challenge(&html) {
                    return Some(SpecialResolveOutcome::StaticLinks(vec![
                        build_static_error_link(
                            url.to_string(),
                            hint_name.unwrap_or_else(|| fallback_filename_from_url(url)),
                            hint_size,
                            Some("FileDitch".to_string()),
                            headers.clone(),
                            "FileDitch protegido por Cloudflare — clique em 'Resolver no Vortex' ou 'Resolver no Navegador'"
                                .to_string(),
                        ),
                    ]));
                }
                if api_status_true || hint_size.is_some() {
                    return Some(SpecialResolveOutcome::StaticLinks(vec![
                        build_static_error_link(
                            url.to_string(),
                            hint_name.unwrap_or_else(|| fallback_filename_from_url(url)),
                            hint_size,
                            Some("FileDitch".to_string()),
                            headers.clone(),
                            "FileDitch exige verificação no navegador — clique em 'Resolver no Vortex' ou 'Resolver no Navegador'"
                                .to_string(),
                        ),
                    ]));
                }
            }
            Err(_) if api_status_true || hint_size.is_some() => {
                return Some(SpecialResolveOutcome::StaticLinks(vec![
                    build_static_error_link(
                        url.to_string(),
                        hint_name.unwrap_or_else(|| fallback_filename_from_url(url)),
                        hint_size,
                        Some("FileDitch".to_string()),
                        headers.clone(),
                        "FileDitch bloqueou a leitura direta — clique em 'Resolver no Vortex' ou 'Resolver no Navegador'"
                            .to_string(),
                    ),
                ]));
            }
            Err(_) => {}
        }
    }

    // VikingFile metadata + Turnstile / direct link extraction
    if is_vikingfile_host(&host) {
        let mut get_headers = headers.clone();
        get_headers
            .entry("User-Agent".to_string())
            .or_insert_with(|| BROWSER_UA.to_string());
        if let Ok((page_url, html)) = fetch_html_limited(client, url, &get_headers).await {
            let meta = parse_vikingfile_html(&page_url, &html);
            if let Some(direct) = meta.direct_link {
                let mut dl_headers = headers.clone();
                dl_headers.insert("Referer".to_string(), page_url);
                return Some(SpecialResolveOutcome::DirectCandidates(vec![(
                    direct,
                    dl_headers,
                    meta.filename,
                    meta.size_bytes,
                )]));
            }
            if meta.has_turnstile || meta.filename.is_some() {
                let name = meta
                    .filename
                    .unwrap_or_else(|| fallback_filename_from_url(url));
                let msg = if meta.has_turnstile {
                    "VikingFile exige Turnstile no navegador — clique em 'Resolver no Navegador' para capturar via extensão"
                } else {
                    "Não foi possível obter link direto do VikingFile"
                };
                return Some(SpecialResolveOutcome::StaticLinks(vec![
                    build_static_error_link(
                        url.to_string(),
                        name,
                        meta.size_bytes,
                        Some("VikingFile".to_string()),
                        headers.clone(),
                        msg.to_string(),
                    ),
                ]));
            }
        }
    }

    // Buzzheavier HTMX redirect endpoint
    if host_matches(&host, "buzzheavier.com") || host_matches(&host, "bzzhr.co") {
        let dl_endpoint = format!("{}/download", url.trim_end_matches('/'));
        if let Ok(resp) = apply_custom_headers(client.get(&dl_endpoint), headers)
            .header("HX-Request", "true")
            .header("Referer", url)
            .timeout(Duration::from_secs(12))
            .send()
            .await
        {
            if let Some(hx) = resp
                .headers()
                .get("hx-redirect")
                .or_else(|| resp.headers().get("location"))
                .and_then(|v| v.to_str().ok())
            {
                if let Ok(abs) = parsed.join(hx) {
                    let mut h = headers.clone();
                    h.entry("Referer".to_string())
                        .or_insert_with(|| url.to_string());
                    return Some(SpecialResolveOutcome::DirectCandidates(vec![(
                        abs.to_string(),
                        h,
                        None,
                        None,
                    )]));
                }
            }
        }
    }

    None
}

pub async fn resolve_indirect_http_url(
    client: &Client,
    url: &str,
    headers: &HashMap<String, String>,
) -> anyhow::Result<Option<ResolvedDirectDownload>> {
    if let Some(outcome) = try_special_hoster_resolve(client, url, headers).await {
        match outcome {
            SpecialResolveOutcome::DirectCandidates(specials) => {
                if let Some((cand_url, cand_headers, hint_name, hint_size)) =
                    specials.into_iter().next()
                {
                    if let Ok(mut info) = probe_url(client, &cand_url, &cand_headers).await {
                        if !is_html_probe(&info) {
                            if info.filename.is_none() {
                                info.filename = hint_name;
                            }
                            if info.total_bytes.is_none() {
                                info.total_bytes = hint_size;
                            }
                            return Ok(Some(ResolvedDirectDownload {
                                url: cand_url,
                                probe: info,
                                headers: cand_headers,
                            }));
                        }
                    }
                }
            }
            SpecialResolveOutcome::StaticLinks(links) => {
                if let Some(err) = links.into_iter().next().and_then(|l| l.error) {
                    return Err(anyhow::anyhow!(err));
                }
            }
        }
    }

    let mut current_url = url.to_string();
    let mut current_headers = headers.clone();

    for _hop in 0..2 {
        let Ok((page_url, html)) = fetch_html_limited(client, &current_url, &current_headers).await
        else {
            break;
        };
        let candidates = extract_html_download_candidates(&page_url, &html);
        if candidates.is_empty()
            && (is_cloudflare_challenge(&html) || is_fileditch_pow_challenge(&html))
        {
            return Err(anyhow::anyhow!(
                "Protegido por verificação/Captcha no navegador — clique em 'Resolver no Vortex' ou 'Resolver no Navegador'"
            ));
        }

        let mut next_hop_html: Option<String> = None;
        for (_, cand) in candidates.into_iter().take(4) {
            let mut cand_headers = current_headers.clone();
            cand_headers
                .entry("Referer".to_string())
                .or_insert_with(|| page_url.clone());

            if let Ok(info) = probe_url(client, &cand, &cand_headers).await {
                if !is_html_probe(&info) {
                    return Ok(Some(ResolvedDirectDownload {
                        url: cand,
                        probe: info,
                        headers: cand_headers,
                    }));
                } else if next_hop_html.is_none() {
                    next_hop_html = Some(cand);
                }
            }
        }

        if let Some(hop_url) = next_hop_html {
            current_headers.insert("Referer".to_string(), page_url);
            current_url = hop_url;
        } else {
            break;
        }
    }

    Ok(None)
}

async fn probe_single_leaf(
    client: &Client,
    url: String,
    headers: HashMap<String, String>,
) -> (Vec<GrabbedLink>, bool) {
    if url.starts_with("magnet:?") {
        let filename = fallback_filename_from_url(&url);
        return (
            vec![GrabbedLink {
                id: Uuid::new_v4(),
                url,
                filename,
                size_bytes: None,
                online: Some(true),
                probing: false,
                supports_ranges: false,
                category: FileCategory::Torrent,
                package_key: None,
                part_number: None,
                part_total: None,
                source_site: Some("Magnet".to_string()),
                selected: false,
                headers,
                error: None,
            }],
            false,
        );
    }

    if let Some(outcome) = try_special_hoster_resolve(client, &url, &headers).await {
        match outcome {
            SpecialResolveOutcome::StaticLinks(links) => {
                if !links.is_empty() {
                    return (links, false);
                }
            }
            SpecialResolveOutcome::DirectCandidates(specials) => {
                let mut out = Vec::new();
                for (cand_url, cand_headers, hint_name, hint_size) in specials {
                    let source_site = pick_source_site(&url, &cand_url);
                    let fallback_name = hint_name
                        .clone()
                        .unwrap_or_else(|| fallback_filename_from_url(&cand_url));
                    match probe_url(client, &cand_url, &cand_headers).await {
                        Ok(info) => {
                            let filename = sanitize_filename(
                                info.filename.as_deref().unwrap_or(&fallback_name),
                            );
                            let mp = detect_multipart(&filename);
                            let category = FileCategory::from_filename(&filename);
                            out.push(GrabbedLink {
                                id: Uuid::new_v4(),
                                url: cand_url,
                                filename,
                                size_bytes: info.total_bytes.or(hint_size),
                                online: Some(true),
                                probing: false,
                                supports_ranges: info.supports_ranges,
                                category,
                                package_key: mp.as_ref().map(|m| m.package_key.clone()),
                                part_number: mp.as_ref().map(|m| m.part_number),
                                part_total: None,
                                source_site,
                                selected: false,
                                headers: cand_headers,
                                error: None,
                            });
                        }
                        Err(err) => {
                            let is_rate_limited = err.to_string().contains("429");
                            let filename = sanitize_filename(&fallback_name);
                            let mp = detect_multipart(&filename);
                            let category = FileCategory::from_filename(&filename);
                            if is_rate_limited && hint_name.is_some() && hint_size.is_some() {
                                out.push(GrabbedLink {
                                    id: Uuid::new_v4(),
                                    url: cand_url,
                                    filename,
                                    size_bytes: hint_size,
                                    online: Some(true),
                                    probing: false,
                                    supports_ranges: true,
                                    category,
                                    package_key: mp.as_ref().map(|m| m.package_key.clone()),
                                    part_number: mp.as_ref().map(|m| m.part_number),
                                    part_total: None,
                                    source_site,
                                    selected: false,
                                    headers: cand_headers,
                                    error: None,
                                });
                            } else {
                                out.push(GrabbedLink {
                                    id: Uuid::new_v4(),
                                    url: cand_url,
                                    filename,
                                    size_bytes: hint_size,
                                    online: Some(false),
                                    probing: false,
                                    supports_ranges: false,
                                    category,
                                    package_key: mp.as_ref().map(|m| m.package_key.clone()),
                                    part_number: mp.as_ref().map(|m| m.part_number),
                                    part_total: None,
                                    source_site,
                                    selected: false,
                                    headers: cand_headers,
                                    error: Some(err.to_string()),
                                });
                            }
                        }
                    }
                }
                if !out.is_empty() {
                    return (out, false);
                }
            }
        }
    }

    let mut filename = fallback_filename_from_url(&url);
    match probe_url(client, &url, &headers).await {
        Ok(info) => {
            if is_html_probe(&info) {
                match resolve_indirect_http_url(client, &url, &headers).await {
                    Ok(Some(resolved)) => {
                        let source_site = pick_source_site(&url, &resolved.url);
                        let resolved_name = resolved
                            .probe
                            .filename
                            .unwrap_or_else(|| fallback_filename_from_url(&resolved.url));
                        let clean = sanitize_filename(&resolved_name);
                        let mp = detect_multipart(&clean);
                        let category = FileCategory::from_filename(&clean);
                        return (
                            vec![GrabbedLink {
                                id: Uuid::new_v4(),
                                url: resolved.url,
                                filename: clean,
                                size_bytes: resolved.probe.total_bytes,
                                online: Some(true),
                                probing: false,
                                supports_ranges: resolved.probe.supports_ranges,
                                category,
                                package_key: mp.as_ref().map(|m| m.package_key.clone()),
                                part_number: mp.as_ref().map(|m| m.part_number),
                                part_total: None,
                                source_site,
                                selected: false,
                                headers: resolved.headers,
                                error: None,
                            }],
                            false,
                        );
                    }
                    Ok(None) => {
                        let source_site = classify_source_site(&url);
                        if let Some(name) = info.filename {
                            filename = name;
                        }
                        filename = sanitize_filename(&filename);
                        let mp = detect_multipart(&filename);
                        let category = FileCategory::from_filename(&filename);
                        return (
                            vec![GrabbedLink {
                                id: Uuid::new_v4(),
                                url,
                                filename,
                                size_bytes: info.total_bytes,
                                online: Some(true),
                                probing: false,
                                supports_ranges: info.supports_ranges,
                                category,
                                package_key: mp.as_ref().map(|m| m.package_key.clone()),
                                part_number: mp.as_ref().map(|m| m.part_number),
                                part_total: None,
                                source_site,
                                selected: false,
                                headers,
                                error: None,
                            }],
                            true,
                        );
                    }
                    Err(e) => {
                        let source_site = classify_source_site(&url);
                        return (
                            vec![GrabbedLink {
                                id: Uuid::new_v4(),
                                url,
                                filename,
                                size_bytes: None,
                                online: Some(false),
                                probing: false,
                                supports_ranges: false,
                                category: FileCategory::Other,
                                package_key: None,
                                part_number: None,
                                part_total: None,
                                source_site,
                                selected: false,
                                headers,
                                error: Some(e.to_string()),
                            }],
                            false,
                        );
                    }
                }
            }

            let source_site = classify_source_site(&url);
            if let Some(name) = info.filename {
                filename = name;
            }
            filename = sanitize_filename(&filename);
            let mp = detect_multipart(&filename);
            let category = FileCategory::from_filename(&filename);
            (
                vec![GrabbedLink {
                    id: Uuid::new_v4(),
                    url,
                    filename,
                    size_bytes: info.total_bytes,
                    online: Some(true),
                    probing: false,
                    supports_ranges: info.supports_ranges,
                    category,
                    package_key: mp.as_ref().map(|m| m.package_key.clone()),
                    part_number: mp.as_ref().map(|m| m.part_number),
                    part_total: None,
                    source_site,
                    selected: false,
                    headers,
                    error: None,
                }],
                false,
            )
        }
        Err(err) => {
            let source_site = classify_source_site(&url);
            let mp = detect_multipart(&filename);
            let category = FileCategory::from_filename(&filename);
            (
                vec![GrabbedLink {
                    id: Uuid::new_v4(),
                    url,
                    filename,
                    size_bytes: None,
                    online: Some(false),
                    probing: false,
                    supports_ranges: false,
                    category,
                    package_key: mp.as_ref().map(|m| m.package_key.clone()),
                    part_number: mp.as_ref().map(|m| m.part_number),
                    part_total: None,
                    source_site,
                    selected: false,
                    headers,
                    error: Some(err.to_string()),
                }],
                false,
            )
        }
    }
}

async fn probe_single_url_smart(
    client: &Client,
    url: String,
    headers: HashMap<String, String>,
) -> Vec<GrabbedLink> {
    let (links, is_unresolved_html) = probe_single_leaf(client, url.clone(), headers.clone()).await;
    if is_unresolved_html {
        if let Ok((page_url, html)) = fetch_html_limited(client, &url, &headers).await {
            let child_urls = extract_html_index_links(&page_url, &html);
            if child_urls.len() >= 2 {
                let mut child_headers = headers;
                child_headers
                    .entry("Referer".to_string())
                    .or_insert(page_url);
                let mut expanded = Vec::new();
                for child in child_urls {
                    let c = client.clone();
                    let h = child_headers.clone();
                    expanded.push(tokio::spawn(async move {
                        probe_single_leaf(&c, child, h).await.0
                    }));
                }
                let mut out = Vec::new();
                for task in expanded {
                    if let Ok(mut items) = task.await {
                        out.append(&mut items);
                    }
                }
                if !out.is_empty() {
                    return out;
                }
            }
        }
    }
    links
}

pub async fn probe_grabbed_links(
    client: &Client,
    urls: Vec<String>,
    headers: HashMap<String, String>,
) -> Vec<GrabbedLink> {
    let mut handles = Vec::with_capacity(urls.len());
    for url in urls {
        let c = client.clone();
        let h = headers.clone();
        handles.push(tokio::spawn(async move {
            probe_single_url_smart(&c, url, h).await
        }));
    }

    let mut links = Vec::new();
    let mut seen_urls = HashSet::new();
    for handle in handles {
        if let Ok(items) = handle.await {
            for link in items {
                if seen_urls.insert(link.url.clone()) {
                    links.push(link);
                }
            }
        }
    }
    assign_package_totals(&mut links);
    links
}

pub fn assign_package_totals(links: &mut [GrabbedLink]) {
    // Also link `.zip` to `.z01` packages and `.rar` to `.r00` packages if present
    let existing_keys: HashSet<String> = links
        .iter()
        .filter_map(|l| l.package_key.clone())
        .collect();
    for link in links.iter_mut() {
        if link.package_key.is_none() {
            let lower = link.filename.to_lowercase();
            if lower.ends_with(".zip") && existing_keys.contains(&link.filename) {
                link.package_key = Some(link.filename.clone());
                link.part_number = Some(999);
            } else if lower.ends_with(".rar") && existing_keys.contains(&link.filename) {
                link.package_key = Some(link.filename.clone());
                link.part_number = Some(1);
            }
        }
    }

    let mut counts: HashMap<String, u32> = HashMap::new();
    for link in links.iter() {
        if let Some(ref key) = link.package_key {
            *counts.entry(key.clone()).or_insert(0) += 1;
        }
    }

    for link in links.iter_mut() {
        if let Some(ref key) = link.package_key {
            if let Some(&total) = counts.get(key) {
                link.part_total = Some(total);
            }
        }
    }
}

pub fn find_missing_parts(part_numbers: &[u32]) -> Vec<u32> {
    if part_numbers.is_empty() {
        return Vec::new();
    }
    let set: BTreeSet<u32> = part_numbers.iter().copied().collect();
    let max_part = *set.iter().next_back().unwrap_or(&0);
    let min_part = *set.iter().next().unwrap_or(&1);
    let start = if min_part == 0 { 0 } else { 1 };
    let mut missing = Vec::new();
    for p in start..=max_part {
        if !set.contains(&p) {
            missing.push(p);
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GrabbedLink;

    fn make_link(name: &str) -> GrabbedLink {
        let mp = detect_multipart(name);
        GrabbedLink {
            id: uuid::Uuid::new_v4(),
            url: format!("https://example.com/{name}"),
            filename: name.to_string(),
            size_bytes: Some(1024),
            online: Some(true),
            probing: false,
            supports_ranges: true,
            category: crate::model::FileCategory::from_filename(name),
            headers: HashMap::new(),
            package_key: mp.as_ref().map(|m| m.package_key.clone()),
            part_number: mp.as_ref().map(|m| m.part_number),
            part_total: None,
            source_site: None,
            selected: true,
            error: None,
        }
    }

    #[test]
    fn groups_rar_multipart_and_detects_missing() {
        let mut links = vec![
            make_link("Ubuntu.part01.rar"),
            make_link("Ubuntu.part03.rar"),
            make_link("single_file.iso"),
        ];
        assign_package_totals(&mut links);
        assert_eq!(links[0].package_key.as_deref(), Some("Ubuntu.rar"));
        assert_eq!(links[0].part_number, Some(1));
        assert_eq!(links[1].package_key.as_deref(), Some("Ubuntu.rar"));
        assert_eq!(links[1].part_number, Some(3));
        assert_eq!(links[2].package_key, None);
        assert_eq!(find_missing_parts(&[1, 3]), vec![2]);
    }

    #[test]
    fn groups_7z_and_zip_splits() {
        let mut links = vec![
            make_link("Archive.7z.001"),
            make_link("Archive.7z.002"),
            make_link("Backup.z01"),
            make_link("Backup.zip"),
        ];
        assign_package_totals(&mut links);
        assert_eq!(links[0].package_key.as_deref(), Some("Archive.7z"));
        assert_eq!(links[1].part_total, Some(2));
        assert_eq!(links[2].package_key.as_deref(), Some("Backup.zip"));
        assert_eq!(links[3].package_key.as_deref(), Some("Backup.zip"));
    }

    #[test]
    fn extracts_urls_and_sanitizes_filenames() {
        let text = "Check https://cdn.test/a.part1.rar and https://cdn.test/a.part2.rar! Also https://cdn.test/a.part1.rar";
        let urls = extract_urls_from_text(text);
        assert_eq!(urls.len(), 2);
        assert_eq!(sanitize_filename("my:bad*file?.zip"), "my_bad_file_.zip");
    }

    #[test]
    fn extracts_fileditch_style_download_link_and_index_links() {
        let html = r#"
            <html><body>
                <a href="/plans">veja os planos</a>
                <a class="btn" href="https://beta.staysharpliftsheavydaily.xyz/MTc5NTY4">Baixar</a>
                <a href="https://beta.staysharpliftsheavydaily.xyz/MTc5NTY4" download>Copiar link de download para JDownloader / FDM</a>
                <a href="/report">Denunciar</a>
            </body></html>
        "#;
        let cands = extract_html_download_candidates("https://fileditchfiles.me/file.php?id=123", html);
        assert!(!cands.is_empty());
        assert_eq!(cands[0].1, "https://beta.staysharpliftsheavydaily.xyz/MTc5NTY4");

        let index_html = r#"
            <div>
                <a href="https://fileditchfiles.me/file.php?id=p1">DOWNLOAD</a>
                <a href="https://fileditchfiles.me/file.php?id=p2">DOWNLOAD</a>
                <a href="https://example.com/about">About</a>
            </div>
        "#;
        let links = extract_html_index_links("https://games.example.org/hot-wheels", index_html);
        assert_eq!(links.len(), 2);

        let mp = detect_multipart("HOT.WHEELS.Infinite.Rush.Build.03112026-OFME.part1 (1).rar")
            .unwrap();
        assert_eq!(mp.part_number, 1);
        assert!(mp.is_first_part);
    }

    #[test]
    fn classifies_hoster_sites_and_domains() {
        assert_eq!(
            classify_source_site("https://itsfileditchfiles.st/f/abc/game.part1.rar").as_deref(),
            Some("FileDitch")
        );
        assert_eq!(
            classify_source_site("https://fileditchfiles.me/file.php?id=123").as_deref(),
            Some("FileDitch")
        );
        assert_eq!(
            classify_source_site("https://filekeeper.net/abc123").as_deref(),
            Some("FileKeeper")
        );
        assert_eq!(
            classify_source_site("https://pixeldrain.com/u/xyz").as_deref(),
            Some("Pixeldrain")
        );
        assert_eq!(
            classify_source_site("https://gofile.io/d/folder").as_deref(),
            Some("Gofile")
        );
        assert_eq!(
            classify_source_site("https://vikingfile.com/f/8obvHhvQET").as_deref(),
            Some("VikingFile")
        );
        assert_eq!(
            classify_source_site("https://www.example.org/files/test.zip").as_deref(),
            Some("example.org")
        );
        assert_eq!(
            pick_source_site(
                "https://fileditchfiles.me/file.php?id=123",
                "https://beta.staysharpliftsheavydaily.xyz/MTc5NTY4"
            )
            .as_deref(),
            Some("FileDitch")
        );
    }

    #[test]
    fn parses_human_sizes_and_hoster_html() {
        assert_eq!(parse_human_size("373.35 MB"), Some(373_350_000));
        assert_eq!(parse_human_size("1.5 GB"), Some(1_500_000_000));
        assert_eq!(parse_human_size("2 MiB"), Some(2 * 1024 * 1024));

        let fk_html = r#"
            <html><head><title>Download Game.part1.rar - FileKeeper</title></head>
            <body>
              <form method="POST" action="/download">
                <div id="download-countdown" data-countdown="5" data-code="xyz99" data-rand="rnd123" data-referer="https://filekeeper.net/xyz99" data-method="Free" data-has-captcha="false" data-has-password="false"></div>
              </form>
              <span>Size: 1.25 GB</span>
            </body></html>
        "#;
        let fk = parse_filekeeper_html("https://filekeeper.net/xyz99", fk_html).unwrap();
        assert_eq!(fk.countdown, 5);
        assert_eq!(fk.code, "xyz99");
        assert_eq!(fk.rand, "rnd123");
        assert_eq!(fk.method, "Free");
        assert!(!fk.has_captcha);
        assert_eq!(fk.post_action.as_deref(), Some("/download"));
        assert_eq!(fk.hint_name.as_deref(), Some("Game.part1.rar"));
        assert_eq!(fk.hint_size, Some(1_250_000_000));

        let vf_html = r#"
            <html><body>
              <h2 id="filename">HOT.WHEELS.part5.rar</h2>
              <p id="size">373.35 MB</p>
              <div id="captcha-download" class="cf-turnstile" data-sitekey="0x4AAAAAAAgbsMNBuk2d3Qp6"></div>
            </body></html>
        "#;
        let vf = parse_vikingfile_html("https://vikingfile.com/f/8obvHhvQET", vf_html);
        assert_eq!(vf.filename.as_deref(), Some("HOT.WHEELS.part5.rar"));
        assert_eq!(vf.size_bytes, Some(373_350_000));
        assert!(vf.has_turnstile);
        assert!(vf.direct_link.is_none());
    }

    #[test]
    fn computes_deterministic_gofile_website_token() {
        let wt1 = compute_gofile_website_token(
            "UA",
            "en-US",
            "tok123",
            1_700_000_000,
            GOFILE_WT_SALT_DEFAULT,
        );
        let wt2 = compute_gofile_website_token(
            "UA",
            "en-US",
            "tok123",
            1_700_000_000 + 100,
            GOFILE_WT_SALT_DEFAULT,
        );
        assert_eq!(wt1.len(), 64);
        assert_eq!(wt1, wt2);
    }

    #[test]
    fn detects_fileditch_pow_gate_and_purple_download_button() {
        let pow_html = r#"
            <html><body>
              <div class="verifying-overlay"><div class="spinner"></div><p>Verifying your browser…</p></div>
              <form method="POST"><input type="hidden" name="x" value="1"></form>
              <script type="module">import { run } from "https://fileditchassets.b-cdn.net/pow/0e2e2529da/powwasm.js";</script>
            </body></html>
        "#;
        assert!(is_fileditch_pow_challenge(pow_html));
        assert!(
            extract_html_download_candidates(
                "https://itsfileditchfiles.st/alpha43/a163/game.part1.rar?fdg=1",
                pow_html
            )
            .is_empty()
        );

        let unlocked_html = r#"
            <html><body>
              <div class="controls">
                <a class="btn btn-main" href="https://lumiskipssugardaily.xyz/MTc5NTc3MDgwMTphYmM=/game.part1.rar">↓ Baixar</a>
                <a class="btn btn-alt" href="/report">Denunciar</a>
                <a class="btn btn-dlcopy" href="https://lumiskipssugardaily.xyz/MTc5NTc3MDgwMTphYmM=/game.part1.rar">Copiar link de download para JDownloader / FDM</a>
              </div>
            </body></html>
        "#;
        assert!(!is_fileditch_pow_challenge(unlocked_html));
        let cands = extract_html_download_candidates(
            "https://itsfileditchfiles.st/alpha43/a163/game.part1.rar",
            unlocked_html,
        );
        assert!(!cands.is_empty());
        assert_eq!(
            cands[0].1,
            "https://lumiskipssugardaily.xyz/MTc5NTc3MDgwMTphYmM=/game.part1.rar"
        );
    }
}
