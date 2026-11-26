use crate::http_engine::{ProbeResult, apply_custom_headers, probe_url};
use crate::model::{FileCategory, GrabbedLink};
use regex::Regex;
use reqwest::Client;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;
use uuid::Uuid;

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

fn is_html_probe(info: &ProbeResult) -> bool {
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

fn is_known_file_hoster(url_str: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    let Some(host) = parsed.host_str().map(|h| h.to_ascii_lowercase()) else {
        return false;
    };
    [
        "fileditch.com",
        "fileditchfiles.me",
        "pixeldrain.com",
        "gofile.io",
        "vikingfile.com",
        "filekeeper.net",
        "mediafire.com",
        "1fichier.com",
        "buzzheavier.com",
        "bzzhr.co",
        "qiwi.gg",
        "krakenfiles.com",
        "mega.nz",
        "akirabox.com",
        "send.cm",
        "uploadhaven.com",
        "datanodes.to",
    ]
    .iter()
    .any(|d| host_matches(&host, d))
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

async fn try_special_hoster_resolve(
    client: &Client,
    url: &str,
    headers: &HashMap<String, String>,
) -> Option<Vec<(String, HashMap<String, String>, Option<String>, Option<u64>)>> {
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
                    title = val.get("name").and_then(|v| v.as_str()).map(str::to_string);
                    size = val.get("size").and_then(|v| v.as_u64());
                }
            }
            return Some(vec![(dl_url, headers.clone(), title, size)]);
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
                                let name = f.get("name").and_then(|v| v.as_str()).map(str::to_string);
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
                            return Some(list);
                        }
                    }
                }
            }
        }
    }

    // Gofile public folder
    if host_matches(&host, "gofile.io") && segs.first() == Some(&"d") && segs.len() >= 2 {
        let folder_id = segs[1];
        if let Ok(acc_resp) = client
            .post("https://api.gofile.io/accounts")
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
                    if let Ok(cont_resp) = client
                        .get(format!(
                            "https://api.gofile.io/contents/{folder_id}?wt=4fd6sg89d7s6"
                        ))
                        .header("Authorization", format!("Bearer {token}"))
                        .timeout(Duration::from_secs(12))
                        .send()
                        .await
                    {
                        if let Ok(cont_json) = cont_resp.json::<serde_json::Value>().await {
                            if let Some(children) = cont_json
                                .get("data")
                                .and_then(|d| d.get("children"))
                                .and_then(|c| c.as_object())
                            {
                                let mut out = Vec::new();
                                for (_k, child) in children {
                                    if child.get("type").and_then(|t| t.as_str()) == Some("file") {
                                        if let Some(link) =
                                            child.get("link").and_then(|l| l.as_str())
                                        {
                                            let mut h = headers.clone();
                                            h.insert(
                                                "Cookie".to_string(),
                                                format!("accountToken={token}"),
                                            );
                                            let name = child
                                                .get("name")
                                                .and_then(|n| n.as_str())
                                                .map(str::to_string);
                                            let size =
                                                child.get("size").and_then(|s| s.as_u64());
                                            out.push((link.to_string(), h, name, size));
                                        }
                                    }
                                }
                                if !out.is_empty() {
                                    return Some(out);
                                }
                            }
                        }
                    }
                }
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
                    return Some(vec![(abs.to_string(), h, None, None)]);
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
    if let Some(specials) = try_special_hoster_resolve(client, url, headers).await {
        if let Some((cand_url, cand_headers, hint_name, hint_size)) = specials.into_iter().next() {
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

    let mut current_url = url.to_string();
    let mut current_headers = headers.clone();

    for _hop in 0..2 {
        let Ok((page_url, html)) = fetch_html_limited(client, &current_url, &current_headers).await
        else {
            break;
        };
        let candidates = extract_html_download_candidates(&page_url, &html);
        if candidates.is_empty() && is_cloudflare_challenge(&html) {
            return Err(anyhow::anyhow!(
                "Protegido por Cloudflare/Captcha — abra no navegador e clique para capturar pela extensão"
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
                selected: true,
                headers,
                error: None,
            }],
            false,
        );
    }

    if let Some(specials) = try_special_hoster_resolve(client, &url, &headers).await {
        let mut out = Vec::new();
        for (cand_url, cand_headers, hint_name, hint_size) in specials {
            let mut filename = hint_name.unwrap_or_else(|| fallback_filename_from_url(&cand_url));
            match probe_url(client, &cand_url, &cand_headers).await {
                Ok(info) => {
                    if let Some(name) = info.filename {
                        filename = name;
                    }
                    filename = sanitize_filename(&filename);
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
                        selected: true,
                        headers: cand_headers,
                        error: None,
                    });
                }
                Err(err) => {
                    let mp = detect_multipart(&filename);
                    let category = FileCategory::from_filename(&filename);
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
                        selected: true,
                        headers: cand_headers,
                        error: Some(err.to_string()),
                    });
                }
            }
        }
        if !out.is_empty() {
            return (out, false);
        }
    }

    let mut filename = fallback_filename_from_url(&url);
    match probe_url(client, &url, &headers).await {
        Ok(info) => {
            if is_html_probe(&info) {
                match resolve_indirect_http_url(client, &url, &headers).await {
                    Ok(Some(resolved)) => {
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
                                selected: true,
                                headers: resolved.headers,
                                error: None,
                            }],
                            false,
                        );
                    }
                    Ok(None) => {
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
                                selected: true,
                                headers,
                                error: None,
                            }],
                            true,
                        );
                    }
                    Err(e) => {
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
                                selected: false,
                                headers,
                                error: Some(e.to_string()),
                            }],
                            false,
                        );
                    }
                }
            }

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
                    selected: true,
                    headers,
                    error: None,
                }],
                false,
            )
        }
        Err(err) => {
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
                    selected: true,
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
}
