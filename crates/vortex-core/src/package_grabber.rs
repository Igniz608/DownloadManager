use crate::http_engine::probe_url;
use crate::model::{FileCategory, GrabbedLink};
use regex::Regex;
use reqwest::Client;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct MultiPartMatch {
    pub package_key: String,
    pub part_number: u32,
    pub is_first_part: bool,
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
    let re_part_rar =
        RE_PART_RAR.get_or_init(|| Regex::new(r"(?i)^(.+?)\.part(\d+)\.rar$").unwrap());
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
        Regex::new(r"(?i)^(.+?(?:\.(?:7z|zip|rar|tar|gz|xz|zst))?)\.(\d{3,4})$").unwrap()
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
            let mut filename = fallback_filename_from_url(&url);
            if url.starts_with("magnet:?") {
                return GrabbedLink {
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
                    headers: h,
                    error: None,
                };
            }
            match probe_url(&c, &url, &h).await {
                Ok(info) => {
                    if let Some(name) = info.filename {
                        filename = name;
                    }
                    let mp = detect_multipart(&filename);
                    let category = FileCategory::from_filename(&filename);
                    GrabbedLink {
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
                        headers: h,
                        error: None,
                    }
                }
                Err(err) => {
                    let mp = detect_multipart(&filename);
                    let category = FileCategory::from_filename(&filename);
                    GrabbedLink {
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
                        headers: h,
                        error: Some(err.to_string()),
                    }
                }
            }
        }));
    }

    let mut links = Vec::with_capacity(handles.len());
    for handle in handles {
        if let Ok(link) = handle.await {
            links.push(link);
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
}
