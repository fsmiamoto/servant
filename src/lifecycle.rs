//! Thin wrappers around systemd lifecycle commands. Defaults to the per-user
//! service (`systemctl --user ...`); `--system` manages the explicit system
//! service installed by `servant service install --system`.

use std::process::Command;

use anyhow::{anyhow, bail, Result};
use nix::unistd::Uid;

use crate::cli::{LifecycleArgs, LogsArgs};
use crate::config::systemd_unit_path;
use crate::install::{
    resolve_system_service_target, run_systemctl, system_unit_path, which, SystemServiceTarget,
};
use crate::output::OutputMode;

fn ensure_systemctl() -> Result<()> {
    if which("systemctl").is_none() {
        return Err(anyhow!(
            "systemd not available; servant requires Linux with systemd"
        ));
    }
    Ok(())
}

pub fn start(args: LifecycleArgs, _mode: OutputMode) -> Result<i32> {
    ensure_systemctl()?;
    if args.system {
        let target = resolve_lifecycle_system_target(args.system_user.as_deref())?;
        let st = Command::new("systemctl")
            .args(["start", &target.service_name()])
            .status()?;
        Ok(st.code().unwrap_or(1))
    } else {
        let st = Command::new("systemctl")
            .args(["--user", "start", "servant"])
            .status()?;
        Ok(st.code().unwrap_or(1))
    }
}

pub fn stop(args: LifecycleArgs, _mode: OutputMode) -> Result<i32> {
    ensure_systemctl()?;
    if args.system {
        let target = resolve_lifecycle_system_target(args.system_user.as_deref())?;
        let st = Command::new("systemctl")
            .args(["stop", &target.service_name()])
            .status()?;
        Ok(st.code().unwrap_or(1))
    } else {
        let st = Command::new("systemctl")
            .args(["--user", "stop", "servant"])
            .status()?;
        Ok(st.code().unwrap_or(1))
    }
}

pub fn restart(args: LifecycleArgs, _mode: OutputMode) -> Result<i32> {
    ensure_systemctl()?;
    if args.system {
        let target = resolve_lifecycle_system_target(args.system_user.as_deref())?;
        let st = Command::new("systemctl")
            .args(["restart", &target.service_name()])
            .status()?;
        Ok(st.code().unwrap_or(1))
    } else {
        let st = Command::new("systemctl")
            .args(["--user", "restart", "servant"])
            .status()?;
        Ok(st.code().unwrap_or(1))
    }
}

pub fn uninstall(args: LifecycleArgs, _mode: OutputMode) -> Result<i32> {
    ensure_systemctl()?;
    if args.system {
        let target = resolve_lifecycle_system_target(args.system_user.as_deref())?;
        // System uninstall writes /etc and normally requires root. We let
        // systemctl surface polkit/sudo failures, but removing the unit file as
        // non-root would be confusing, so fail early there.
        if !Uid::effective().is_root() {
            bail!(
                "system uninstall requires root. Try:\n  sudo servant service uninstall --system{}",
                args.system_user
                    .as_deref()
                    .map(|u| format!(" --system-user {u}"))
                    .unwrap_or_default()
            );
        }
        let service = target.service_name();
        let _ = Command::new("systemctl")
            .args(["disable", "--now", &service])
            .status();
        let unit = system_unit_path(&target);
        let _ = std::fs::remove_file(&unit);
        let _ = run_systemctl(&["daemon-reload"]);
        println!(
            "✓ servant system service uninstalled (kept ~/.servant/registry.db and ~/.config/servant/config.toml for user {})",
            target.user
        );
        Ok(0)
    } else {
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
}

pub fn logs(args: LogsArgs) -> Result<i32> {
    if which("journalctl").is_none() {
        return Err(anyhow!("journalctl not available"));
    }
    let mut cmd = Command::new("journalctl");
    if args.system {
        let target = resolve_lifecycle_system_target(args.system_user.as_deref())?;
        cmd.args(["-u", &target.service_name()]);
    } else {
        cmd.args(["--user-unit", "servant"]);
    }
    if args.follow {
        cmd.arg("-f");
    }
    if let Some(n) = args.lines {
        cmd.arg("-n").arg(n.to_string());
    }
    let st = cmd.status()?;
    Ok(st.code().unwrap_or(1))
}

fn resolve_lifecycle_system_target(system_user: Option<&str>) -> Result<SystemServiceTarget> {
    if system_user.is_some() || Uid::effective().is_root() {
        resolve_system_service_target(system_user)
    } else {
        bail!(
            "--system lifecycle commands need the target user. Try:\n  sudo servant service <command> --system\nor:\n  servant service <command> --system --system-user $USER"
        )
    }
}
