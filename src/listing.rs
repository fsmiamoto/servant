//! Minimal HTML directory listing for folder mounts without an
//! `index.html`. Dotfiles are omitted to match the dotfile guard.

use std::path::Path;
use std::time::SystemTime;

use chrono_dummy::format_time;

mod chrono_dummy {
    use std::time::{SystemTime, UNIX_EPOCH};
    pub fn format_time(t: SystemTime) -> String {
        let secs = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Render as UTC YYYY-MM-DD HH:MM via plain arithmetic; no chrono dep.
        let (year, month, day, hour, minute) = epoch_to_ymdhm(secs);
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
    }
    fn epoch_to_ymdhm(s: u64) -> (u32, u32, u32, u32, u32) {
        let days = s / 86_400;
        let rem = s % 86_400;
        let hour = (rem / 3600) as u32;
        let minute = ((rem / 60) % 60) as u32;
        let (year, month, day) = days_to_ymd(days as i64 + 719_468 /* shift to civil */);
        (year as u32, month, day, hour, minute)
    }
    // Howard Hinnant's date algorithm.
    fn days_to_ymd(z: i64) -> (i64, u32, u32) {
        let z = z - 719_468;
        let era = if z >= 0 {
            z / 146_097
        } else {
            (z - 146_096) / 146_097
        };
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = (yoe as i64) + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let y = if m <= 2 { y + 1 } else { y };
        (y, m, d)
    }
}

pub fn render(dir: &Path, url_prefix: &str) -> std::io::Result<String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    entries.sort_by_key(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        let is_file = e.file_type().map(|t| t.is_file()).unwrap_or(true);
        (is_file, name)
    });

    let mut html = String::new();
    html.push_str("<!doctype html><html><head><meta charset=\"utf-8\"><title>Index of ");
    html.push_str(&html_escape(url_prefix));
    html.push_str("</title><style>body{font:14px/1.4 monospace;padding:1em}");
    html.push_str("a{text-decoration:none}td{padding:2px 1em}</style></head><body>");
    html.push_str("<h1>Index of ");
    html.push_str(&html_escape(url_prefix));
    html.push_str("</h1><table>");
    html.push_str("<tr><td><a href=\"../\">../</a></td><td></td><td></td></tr>");

    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let md = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = md.is_dir();
        let display = if is_dir {
            format!("{name}/")
        } else {
            name.clone()
        };
        let size = if is_dir {
            "-".into()
        } else {
            format_size(md.len())
        };
        let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        html.push_str(&format!(
            "<tr><td><a href=\"{href}\">{label}</a></td><td>{size}</td><td>{when}</td></tr>",
            href = html_escape(&display),
            label = html_escape(&display),
            size = html_escape(&size),
            when = format_time(mtime),
        ));
    }
    html.push_str("</table></body></html>");
    Ok(html)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn format_size(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}
