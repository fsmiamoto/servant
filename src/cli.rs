//! CLI surface using clap derive. Subcommands route to handlers that
//! return an `i32` exit code (`0` ok, `1` user/config error, `2` daemon
//! unreachable).
//!
//! `--json` works as both a top-level flag and a per-subcommand flag;
//! `SERVANT_JSON=1` is equivalent.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{Args, Parser, Subcommand};

use crate::client::ClientError;
use crate::config::Config;
use crate::output::OutputMode;
use crate::ttl::Ttl;

pub const EXIT_OK: i32 = 0;
pub const EXIT_USER_ERROR: i32 = 1;
pub const EXIT_DAEMON_UNREACHABLE: i32 = 2;

#[derive(Parser, Debug)]
#[command(
    name = "servant",
    version,
    about = "Per-user always-running static file server",
    long_about = None
)]
pub struct Cli {
    /// Emit JSON (also via SERVANT_JSON=1).
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Register a file or directory and print its public URL.
    Serve(ServeArgs),
    /// List active registrations.
    Ls(JsonOnly),
    /// Remove a registration by id, URL, or source path.
    Rm(RmArgs),
    /// Manage the daemon/systemd service.
    #[command(subcommand)]
    Service(ServiceCommand),
    /// Internal: run the daemon in the foreground.
    #[command(hide = true)]
    Daemon,
}

#[derive(Subcommand, Debug)]
pub enum ServiceCommand {
    /// Install the systemd service.
    Install(InstallArgs),
    /// Stop and remove the systemd service.
    Uninstall(LifecycleArgs),
    /// Show daemon health/status.
    Status(JsonOnly),
    /// Tail daemon logs.
    Logs(LogsArgs),
    /// Start the daemon via systemd.
    Start(LifecycleArgs),
    /// Stop the daemon via systemd.
    Stop(LifecycleArgs),
    /// Restart the daemon via systemd.
    Restart(LifecycleArgs),
}

#[derive(Args, Debug)]
pub struct ServeArgs {
    pub path: PathBuf,
    /// TTL like `30s`, `5m`, `2h`, `24h`, `7d`, or `never`.
    #[arg(long, value_parser = parse_ttl)]
    pub ttl: Option<Ttl>,
    /// Override the URL slug.
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JsonOnly {
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct RmArgs {
    /// Numeric id, URL path (`/foo.html`), full URL, or source path.
    pub target: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct InstallArgs {
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub bind: Option<String>,
    /// Install a root-owned system service that runs as the target user.
    #[arg(long)]
    pub system: bool,
    /// Target user for --system installs; defaults to SUDO_USER.
    #[arg(long, requires = "system")]
    pub system_user: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct LifecycleArgs {
    /// Manage the root-owned system service instead of the systemd --user service.
    #[arg(long)]
    pub system: bool,
    /// Target user for --system lifecycle commands; defaults to SUDO_USER.
    #[arg(long, requires = "system")]
    pub system_user: Option<String>,
}

#[derive(Args, Debug)]
pub struct LogsArgs {
    #[arg(short = 'f', long)]
    pub follow: bool,
    #[arg(short = 'n', long)]
    pub lines: Option<u32>,
    /// Show logs for the root-owned system service.
    #[arg(long)]
    pub system: bool,
    /// Target user for --system logs; defaults to SUDO_USER.
    #[arg(long, requires = "system")]
    pub system_user: Option<String>,
}

fn parse_ttl(s: &str) -> Result<Ttl, String> {
    Ttl::from_str(s)
}

fn env_json() -> bool {
    matches!(std::env::var("SERVANT_JSON").ok().as_deref(), Some(v) if !v.is_empty() && v != "0")
}

/// Top-level entry. Parses argv and dispatches.
pub fn run() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            // Use clap's renderer (handles --help, --version, usage errors).
            let _ = e.print();
            // clap maps help/version to its own kinds — exit 0 for those.
            return match e.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => EXIT_USER_ERROR,
            };
        }
    };

    let cmd_json = match &cli.command {
        Command::Serve(a) => a.json,
        Command::Ls(a) => a.json,
        Command::Rm(a) => a.json,
        Command::Service(ServiceCommand::Install(a)) => a.json,
        Command::Service(ServiceCommand::Status(a)) => a.json,
        _ => false,
    };
    let mode = if cli.json || cmd_json || env_json() {
        OutputMode::Json
    } else {
        OutputMode::Human
    };

    let result = match cli.command {
        Command::Service(ServiceCommand::Install(args)) => crate::install::run_install(args, mode),
        command => {
            let cfg = match Config::load() {
                Ok(c) => c,
                Err(e) => {
                    crate::output::print_error(mode, &e.to_string(), EXIT_USER_ERROR);
                    return EXIT_USER_ERROR;
                }
            };
            match command {
                Command::Daemon => crate::daemon::run_daemon(cfg).map(|_| EXIT_OK),
                Command::Service(service) => match service {
                    ServiceCommand::Install(_) => {
                        unreachable!("service install handled before config load")
                    }
                    ServiceCommand::Uninstall(a) => crate::lifecycle::uninstall(a, mode),
                    ServiceCommand::Start(a) => crate::lifecycle::start(a, mode),
                    ServiceCommand::Stop(a) => crate::lifecycle::stop(a, mode),
                    ServiceCommand::Restart(a) => crate::lifecycle::restart(a, mode),
                    ServiceCommand::Logs(a) => crate::lifecycle::logs(a),
                    ServiceCommand::Status(_) => run_status_cli(cfg, mode),
                },
                other => run_async_cli(other, cfg, mode),
            }
        }
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            // Distinguish daemon-unreachable for nice exit codes.
            let code = if let Some(ClientError::Unreachable(_)) = e.downcast_ref::<ClientError>() {
                EXIT_DAEMON_UNREACHABLE
            } else {
                EXIT_USER_ERROR
            };
            crate::output::print_error(mode, &e.to_string(), code);
            code
        }
    }
}

fn run_async_cli(cmd: Command, cfg: Config, mode: OutputMode) -> anyhow::Result<i32> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        match cmd {
            Command::Serve(a) => crate::client::handle_serve(cfg, a, mode).await,
            Command::Ls(_) => crate::client::handle_ls(cfg, mode).await,
            Command::Rm(a) => crate::client::handle_rm(cfg, a, mode).await,
            _ => unreachable!("dispatched synchronously"),
        }
    })
}

fn run_status_cli(cfg: Config, mode: OutputMode) -> anyhow::Result<i32> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(crate::client::handle_status(cfg, mode))
}
