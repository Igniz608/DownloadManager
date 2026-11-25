pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    let b = bytes as f64;
    if b >= TB {
        format!("{:.2} TB", b / TB)
    } else if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_speed(bps: u64) -> String {
    if bps == 0 {
        return "0 KB/s".to_string();
    }
    format!("{}/s", format_bytes(bps))
}

pub fn format_eta(secs: Option<u64>) -> String {
    let Some(s) = secs else {
        return "--:--".to_string();
    };
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m:02}:{sec:02}")
    }
}

pub fn format_duration(secs: Option<f64>) -> String {
    let Some(s) = secs else {
        return "--:--".to_string();
    };
    format_eta(Some(s as u64))
}
