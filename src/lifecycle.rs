//! Thin wrappers around `systemctl --user` / `journalctl --user-unit
//! servant`. Inherits child stdio so users see real tool output.

use std::process::Command;

use anyhow::{anyhow, Result};

use crate::cli::LogsArgs;
use crate::config::systemd_unit_path;
use crate::install::which;
use crate::output::OutputMode;

fn ensure_systemctl() -> Result<()> {
    if which("systemctl").is_none() {
        return Err(anyhow!(
            "systemd not available; servant requires Linux with systemd user services"
        ));
    }
    Ok(())
}

pub fn start(_mode: OutputMode) -> Result<i32> {
    ensure_systemctl()?;
    let st = Command::new("systemctl")
        .args(["--user", "start", "servant"])
        .status()?;
    Ok(st.code().unwrap_or(1))
}

pub fn stop(_mode: OutputMode) -> Result<i32> {
    ensure_systemctl()?;
    let st = Command::new("systemctl")
        .args(["--user", "stop", "servant"])
        .status()?;
    Ok(st.code().unwrap_or(1))
}

pub fn restart(_mode: OutputMode) -> Result<i32> {
    ensure_systemctl()?;
    let st = Command::new("systemctl")
        .args(["--user", "restart", "servant"])
        .status()?;
    Ok(st.code().unwrap_or(1))
}

pub fn uninstall(_mode: OutputMode) -> Result<i32> {
    ensure_systemctl()?;
    // disable --now (ignore failures).
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "servant"])
        .status();
    let unit = systemd_unit_path()?;
    let _ = std::fs::remove_file(&unit);
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    println!(
        "✓ servant uninstalled (kept ~/.servant/registry.db and ~/.config/servant/config.toml)"
    );
    Ok(0)
}

pub fn logs(args: LogsArgs) -> Result<i32> {
    if which("journalctl").is_none() {
        return Err(anyhow!("journalctl not available"));
    }
    let mut cmd = Command::new("journalctl");
    cmd.args(["--user-unit", "servant"]);
    if args.follow {
        cmd.arg("-f");
    }
    if let Some(n) = args.lines {
        cmd.arg("-n").arg(n.to_string());
    }
    let st = cmd.status()?;
    Ok(st.code().unwrap_or(1))
}
