//! `servant service install` — write a systemd unit, daemon-reload, enable --now,
//! then health-check the daemon.
//!
//! The default install path uses a per-user systemd manager. `--system` is an
//! explicit fallback for hosts where `systemctl --user` is unavailable or
//! broken: it writes a root-owned system unit, but still runs the daemon as the
//! target user so servant reads that user's config/state/files rather than
//! running as root.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use nix::unistd::{chown, Gid, Group, Uid, User};

use crate::cli::InstallArgs;
use crate::config::{config_file_path, systemd_unit_path, Config};
use crate::output::OutputMode;

#[derive(Debug, Clone)]
pub struct SystemServiceTarget {
    pub user: String,
    pub uid: Uid,
    pub gid: Gid,
    pub group: String,
    pub home: PathBuf,
}

impl SystemServiceTarget {
    pub fn service_name(&self) -> String {
        system_service_name(self.uid)
    }
}

pub fn run_install(args: InstallArgs, mode: OutputMode) -> Result<i32> {
    if args.system {
        run_system_install(args, mode)
    } else {
        run_user_install(args, mode)
    }
}

fn run_user_install(args: InstallArgs, mode: OutputMode) -> Result<i32> {
    if which("systemctl").is_none() {
        return Err(anyhow!(
            "systemd not available: systemctl was not found in PATH. \
             servant service install requires Linux with systemd."
        ));
    }

    let mut cfg = Config::load()?;
    apply_install_overrides(&mut cfg, &args);
    cfg.ensure_config_dir().ok();
    cfg.save(&config_file_path()?)?;

    let exe = current_binary()?;
    let unit = render_unit(&exe);
    let unit_path = systemd_unit_path()?;
    write_atomic(&unit_path, &unit)?;

    run_user_systemctl(&["daemon-reload"])?;
    run_user_systemctl(&["enable", "--now", "servant"])?;

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

    health_check(&cfg, "journalctl --user -u servant")?;
    print_install_success(mode, "user", &unit_path, &exe, None, &cfg)
}

fn run_system_install(args: InstallArgs, mode: OutputMode) -> Result<i32> {
    if which("systemctl").is_none() {
        return Err(anyhow!(
            "systemd not available: systemctl was not found in PATH"
        ));
    }
    if !Uid::effective().is_root() {
        bail!(
            "system install requires root. Try:\n  sudo servant service install --system{}",
            args.system_user
                .as_deref()
                .map(|u| format!(" --system-user {u}"))
                .unwrap_or_default()
        );
    }

    let target = resolve_system_service_target(args.system_user.as_deref())?;

    // Config/state paths are deliberately per target user. Under `sudo`, HOME
    // may point at /root, so switch it before loading/saving config and before
    // health-polling the target user's control socket.
    std::env::set_var("HOME", &target.home);
    std::env::set_var("USER", &target.user);

    let mut cfg = Config::load()?;
    apply_install_overrides(&mut cfg, &args);
    cfg.ensure_config_dir().ok();
    cfg.save(&config_file_path()?)?;
    chown_target_config(&target)?;

    let exe = current_binary()?;
    let unit = render_system_unit(&exe, &target);
    let unit_path = system_unit_path(&target);
    write_atomic(&unit_path, &unit)?;

    run_systemctl(&["daemon-reload"])?;
    let service = target.service_name();
    run_systemctl(&["enable", "--now", &service])?;

    health_check(&cfg, &format!("journalctl -u {service}"))?;
    print_install_success(mode, "system", &unit_path, &exe, Some(&target), &cfg)
}

fn apply_install_overrides(cfg: &mut Config, args: &InstallArgs) {
    if let Some(p) = args.port {
        cfg.port = p;
    }
    if let Some(b) = args.bind.clone() {
        cfg.bind = b;
    }
}

fn current_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("service.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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

pub fn render_system_unit(exe_path: &Path, target: &SystemServiceTarget) -> String {
    format!(
        "[Unit]\n\
         Description=Servant — local static file server for {user}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User={user}\n\
         Group={group}\n\
         Environment=HOME={home}\n\
         Environment=USER={user}\n\
         WorkingDirectory={home}\n\
         ExecStart={exe} daemon\n\
         Restart=on-failure\n\
         RestartSec=2s\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        user = target.user,
        group = target.group,
        home = target.home.display(),
        exe = exe_path.display(),
    )
}

pub fn resolve_system_service_target(system_user: Option<&str>) -> Result<SystemServiceTarget> {
    let user = match system_user {
        Some(name) => name.to_string(),
        None => std::env::var("SUDO_USER")
            .ok()
            .filter(|u| !u.is_empty() && u != "root")
            .ok_or_else(|| {
                anyhow!(
                    "could not determine which user the system service should run as. \
                     Re-run via sudo from your user account, or pass --system-user NAME"
                )
            })?,
    };

    let passwd =
        User::from_name(&user)?.ok_or_else(|| anyhow!("target user {user:?} does not exist"))?;
    if passwd.uid.is_root() {
        bail!("refusing to install servant system service that runs as root");
    }
    let group = Group::from_gid(passwd.gid)?
        .map(|g| g.name)
        .unwrap_or_else(|| passwd.gid.as_raw().to_string());

    Ok(SystemServiceTarget {
        user: passwd.name,
        uid: passwd.uid,
        gid: passwd.gid,
        group,
        home: passwd.dir,
    })
}

pub fn system_service_name(uid: Uid) -> String {
    format!("servant-user-{}.service", uid.as_raw())
}

pub fn system_unit_path(target: &SystemServiceTarget) -> PathBuf {
    PathBuf::from("/etc/systemd/system").join(target.service_name())
}

fn chown_target_config(target: &SystemServiceTarget) -> Result<()> {
    // `sudo servant service install --system` writes config before the daemon starts.
    // Make sure the user's config remains user-owned rather than root-owned.
    let xdg_config = target.home.join(".config");
    if xdg_config.exists() {
        chown(&xdg_config, Some(target.uid), Some(target.gid))?;
    }
    let servant_config_dir = crate::config::config_dir()?;
    if servant_config_dir.exists() {
        chown(&servant_config_dir, Some(target.uid), Some(target.gid))?;
    }
    let servant_config_file = config_file_path()?;
    if servant_config_file.exists() {
        chown(&servant_config_file, Some(target.uid), Some(target.gid))?;
    }
    Ok(())
}

fn run_user_systemctl(args: &[&str]) -> Result<()> {
    let mut all_args = vec!["--user"];
    all_args.extend_from_slice(args);
    let output = Command::new("systemctl").args(&all_args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = format!(
            "systemctl {} exited {}\n{}{}",
            all_args.join(" "),
            output.status,
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!("stdout:\n{}\n", stdout.trim())
            },
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!("stderr:\n{}\n", stderr.trim())
            },
        );
        return Err(user_systemd_unavailable_error(&detail, Some(&all_args)));
    }
    Ok(())
}

pub fn run_systemctl(args: &[&str]) -> Result<()> {
    let st = Command::new("systemctl").args(args).status()?;
    if !st.success() {
        anyhow::bail!("systemctl {} exited {}", args.join(" "), st);
    }
    Ok(())
}

fn user_systemd_unavailable_error(reason: &str, _args: Option<&[&str]>) -> anyhow::Error {
    anyhow!(
        "systemd user services are unavailable or failed on this host.\n\n{reason}\n\n\
         Try the explicit system-service fallback instead:\n  sudo servant service install --system\n\n\
         This writes a root-owned systemd unit, but runs the servant daemon as your user."
    )
}

fn health_check(cfg: &Config, log_hint: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ok = false;
    while Instant::now() < deadline {
        if poll_health(cfg) {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if !ok {
        return Err(anyhow!(
            "service started but /health did not respond; check `{log_hint}`"
        ));
    }
    Ok(())
}

fn print_install_success(
    mode: OutputMode,
    install_mode: &str,
    unit_path: &Path,
    exe: &Path,
    target: Option<&SystemServiceTarget>,
    cfg: &Config,
) -> Result<i32> {
    match mode {
        OutputMode::Json => {
            let mut v = serde_json::json!({
                "ok": true,
                "mode": install_mode,
                "unit": unit_path.display().to_string(),
                "binary": exe.display().to_string(),
                "bind": cfg.bind,
                "port": cfg.port,
            });
            if let Some(target) = target {
                v["service"] = serde_json::json!(target.service_name());
                v["user"] = serde_json::json!(target.user);
                v["home"] = serde_json::json!(target.home.display().to_string());
            }
            println!("{}", serde_json::to_string(&v)?);
        }
        OutputMode::Human => {
            println!("✓ servant installed");
            println!("  mode:    {install_mode}");
            if let Some(target) = target {
                println!("  service: {}", target.service_name());
                println!("  user:    {}", target.user);
                println!("  home:    {}", target.home.display());
            }
            println!("  unit:    {}", unit_path.display());
            println!("  binary:  {}", exe.display());
            println!("  bind:    {}:{}", cfg.bind, cfg.port);
            println!("  status:  running");
        }
    }
    Ok(0)
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

    #[test]
    fn system_unit_file_runs_as_target_user() {
        let target = SystemServiceTarget {
            user: "alice".to_string(),
            uid: Uid::from_raw(1001),
            gid: Gid::from_raw(1001),
            group: "alice".to_string(),
            home: PathBuf::from("/home/alice"),
        };
        let out = render_system_unit(Path::new("/usr/local/bin/servant"), &target);
        assert!(out.contains("Description=Servant — local static file server for alice"));
        assert!(out.contains("User=alice"));
        assert!(out.contains("Group=alice"));
        assert!(out.contains("Environment=HOME=/home/alice"));
        assert!(out.contains("ExecStart=/usr/local/bin/servant daemon"));
        assert!(out.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn system_service_name_is_uid_scoped() {
        assert_eq!(
            system_service_name(Uid::from_raw(1001)),
            "servant-user-1001.service"
        );
    }
}
