//! `servant install` — write the systemd --user unit, daemon-reload,
//! enable --now, best-effort linger. Pure unit-file rendering is unit
//! tested below; the side-effectful steps are guarded behind the binary
//! and exercised by the HITL runbook in `SMOKE.md`.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::cli::InstallArgs;
use crate::config::{config_file_path, systemd_unit_path, Config};
use crate::output::OutputMode;

pub fn run_install(mut cfg: Config, args: InstallArgs, mode: OutputMode) -> Result<i32> {
    if which("systemctl").is_none() {
        return Err(anyhow!(
            "systemd not available; servant requires Linux with systemd user services"
        ));
    }
    // Merge CLI overrides into config and persist them.
    if let Some(p) = args.port {
        cfg.port = p;
    }
    if let Some(b) = args.bind.clone() {
        cfg.bind = b;
    }
    cfg.ensure_config_dir().ok();
    cfg.save(&config_file_path()?)?;

    // Resolve current binary path; prefer the real path (resolve symlinks).
    let exe = std::env::current_exe().context("current_exe")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);

    // Render and write the unit file atomically.
    let unit = render_unit(&exe);
    let unit_path = systemd_unit_path()?;
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = unit_path.with_extension("service.tmp");
    std::fs::write(&tmp, &unit)?;
    std::fs::rename(&tmp, &unit_path)?;

    run_systemctl(&["--user", "daemon-reload"])?;
    run_systemctl(&["--user", "enable", "--now", "servant"])?;

    // Best-effort linger.
    let user = std::env::var("USER").unwrap_or_default();
    if !user.is_empty() {
        let st = Command::new("loginctl")
            .args(["enable-linger", &user])
            .status();
        if !matches!(st, Ok(s) if s.success()) {
            eprintln!(
                "note: could not enable linger (loginctl enable-linger {user} failed).\n\
                 the daemon will exit on logout. run as root:\n  sudo loginctl enable-linger {user}"
            );
        }
    }

    // Health-poll for up to 5s.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ok = false;
    while Instant::now() < deadline {
        if poll_health(&cfg) {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if !ok {
        return Err(anyhow!(
            "service started but /health did not respond; check `journalctl --user -u servant`"
        ));
    }

    match mode {
        OutputMode::Json => {
            let v = serde_json::json!({
                "ok": true,
                "unit": unit_path.display().to_string(),
                "binary": exe.display().to_string(),
                "bind": cfg.bind,
                "port": cfg.port,
            });
            println!("{}", serde_json::to_string(&v)?);
        }
        OutputMode::Human => {
            println!("✓ servant installed");
            println!("  unit:    {}", unit_path.display());
            println!("  binary:  {}", exe.display());
            println!("  bind:    {}:{}", cfg.bind, cfg.port);
            println!("  status:  running");
        }
    }
    Ok(0)
}

pub fn render_unit(exe_path: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Servant — local static file server\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} daemon\n\
         Restart=on-failure\n\
         RestartSec=2s\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe_path.display()
    )
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let st = Command::new("systemctl").args(args).status()?;
    if !st.success() {
        anyhow::bail!("systemctl {} exited {}", args.join(" "), st);
    }
    Ok(())
}

fn poll_health(cfg: &Config) -> bool {
    // Synchronous: connect via Unix socket and read the response.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(_) => return false,
    };
    rt.block_on(async {
        let client = crate::client::ControlClient::new(cfg);
        client.health().await.is_ok()
    })
}

pub fn which(prog: &str) -> Option<std::path::PathBuf> {
    let p = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&p) {
        let candidate = dir.join(prog);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_file_renders() {
        let out = render_unit(Path::new("/usr/local/bin/servant"));
        assert!(out.contains("ExecStart=/usr/local/bin/servant daemon"));
        assert!(out.contains("WantedBy=default.target"));
        assert!(out.contains("Restart=on-failure"));
        assert!(out.contains("After=network.target"));
    }
}
