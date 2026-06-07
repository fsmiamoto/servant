//! Public hostname resolver used only client-side for printing URLs.
//!
//! Order: `$SERVANT_PUBLIC_HOST` → `config.public_host` → `hostname -f` →
//! `hostname` → `"localhost"`.

use std::sync::OnceLock;

use crate::config::Config;

static CACHE: OnceLock<String> = OnceLock::new();

pub fn resolve_public_host(cfg: &Config) -> String {
    CACHE
        .get_or_init(|| {
            if let Ok(v) = std::env::var("SERVANT_PUBLIC_HOST") {
                let v = v.trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
            if let Some(h) = cfg.public_host.as_ref() {
                let h = h.trim();
                if !h.is_empty() {
                    return h.to_string();
                }
            }
            // Try `hostname -f` (FQDN); fall back to `hostname` short name.
            if let Some(fqdn) = run_hostname(&["-f"]) {
                if fqdn.contains('.') {
                    return fqdn;
                }
            }
            if let Ok(h) = hostname::get() {
                let s = h.to_string_lossy().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
            "localhost".to_string()
        })
        .clone()
}

fn run_hostname(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("hostname")
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
