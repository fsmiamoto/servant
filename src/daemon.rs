//! Daemon entry point: single-instance lock, dual listeners, prune,
//! reaper, graceful shutdown.

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use tokio::net::{TcpListener, UnixListener};
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use tower::Service;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::control::{router as control_router, ControlState};
use crate::db::{self, SharedDb};
use crate::serve::{router as serve_router, ServeState};
use crate::ttl::TouchDebouncer;

const REAPER_INTERVAL: Duration = Duration::from_secs(300);

pub fn run_daemon(cfg: Config) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_daemon_async(cfg))
}

pub async fn run_daemon_async(cfg: Config) -> Result<()> {
    run_daemon_async_with_shutdown(cfg, CancellationToken::new()).await
}

/// Like `run_daemon_async`, but also exits cleanly when `external_shutdown`
/// is cancelled. SIGINT/SIGTERM still cause a graceful shutdown. Used by
/// integration tests that need to simulate a SIGTERM + restart without
/// hijacking the process-wide signal handlers.
pub async fn run_daemon_async_with_shutdown(
    cfg: Config,
    external_shutdown: CancellationToken,
) -> Result<()> {
    init_tracing();
    cfg.ensure_state_dir().context("ensure_state_dir")?;

    // Single-instance lock.
    let lock_path = cfg.daemon_lock_path();
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening lock file {}", lock_path.display()))?;
    if lock_file.try_lock_exclusive().is_err() {
        eprintln!("servant: another servant daemon is already running");
        std::process::exit(2);
    }
    // Keep lock_file alive until end of function.

    // DB open + migrate + prune.
    let mut conn = db::open(&cfg.registry_db_path())?;
    db::migrate(&mut conn)?;
    let stats = db::prune_on_load(&conn)?;
    if stats.expired > 0 || stats.missing_source > 0 {
        tracing::info!(
            target: "servant",
            "prune-on-load: removed {} expired, {} missing-source",
            stats.expired, stats.missing_source
        );
    }
    let db: SharedDb = Arc::new(Mutex::new(conn));

    let touch = TouchDebouncer::spawn(db.clone());
    let shutdown = CancellationToken::new();
    let reaper = crate::reaper::spawn_reaper(db.clone(), REAPER_INTERVAL, shutdown.clone());

    let started_at = Instant::now();
    let cfg_arc = Arc::new(cfg.clone());
    let control_state = ControlState {
        db: db.clone(),
        config: cfg_arc.clone(),
        started_at,
    };
    let serve_state = ServeState {
        db: db.clone(),
        touch: touch.clone(),
    };

    let control = control_router(control_state);
    let serve = serve_router(serve_state);

    // UDS bind.
    let sock_path = cfg.control_socket_path();
    if sock_path.exists() {
        std::fs::remove_file(&sock_path).ok();
    }
    let uds = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(target: "servant", "control socket bound at {}", sock_path.display());

    // TCP bind.
    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let tcp = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let local_addr = tcp.local_addr().ok();
    tracing::info!(target: "servant", "serving plane bound at {addr}{}",
        local_addr.map(|a| format!(" ({a})")).unwrap_or_default());

    // Signal handlers.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let uds_shutdown = shutdown.clone();
    let tcp_shutdown = shutdown.clone();
    let uds_task = tokio::spawn(serve_uds(uds, control, uds_shutdown));
    let tcp_task = tokio::spawn(serve_tcp(tcp, serve, tcp_shutdown));

    tokio::select! {
        _ = sigterm.recv() => tracing::info!(target: "servant", "SIGTERM received"),
        _ = sigint.recv() => tracing::info!(target: "servant", "SIGINT received"),
        _ = external_shutdown.cancelled() => tracing::info!(target: "servant", "external shutdown signal"),
    }

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = uds_task.await;
        let _ = tcp_task.await;
        let _ = reaper.await;
    })
    .await;
    // Drain the TouchDebouncer before we drop the connection. With the
    // expired-row guard in `db::touch`, draining is safe even when the
    // queued touches reference rows that have since expired — those
    // touches become no-ops instead of resurrecting dead rows.
    touch.flush().await;
    // Run one more prune so any rows that crossed their deadline while
    // the daemon was draining are gone before we exit. Without this a
    // restart could briefly serve a row that was already past its TTL
    // when SIGTERM arrived (the next prune_on_load would still catch
    // it, but doing it here keeps the persisted state authoritative).
    {
        let conn = db.lock().unwrap();
        let _ = db::reap_expired(&conn);
    }

    // Cleanup.
    std::fs::remove_file(&sock_path).ok();
    let _ = <std::fs::File as FileExt>::unlock(&lock_file);
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("servant=info,tower_http=info"));
    // Try journald, fall back to stderr only.
    let journald = tracing_journald::layer().ok();
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(journald)
        .with(stderr_layer)
        .try_init();
}

async fn serve_uds(listener: UnixListener, router: axum::Router, shutdown: CancellationToken) {
    let svc = router.into_make_service();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accept = listener.accept() => {
                let (stream, _addr) = match accept {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(target: "servant", "UDS accept error: {e}");
                        continue;
                    }
                };
                let mut make = svc.clone();
                let tower_svc = make.call(&stream).await.unwrap();
                let hyper_svc = hyper_util::service::TowerToHyperService::new(tower_svc);
                let io = TokioIo::new(stream);
                let conn_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let builder = ConnBuilder::new(TokioExecutor::new());
                    tokio::select! {
                        _ = conn_shutdown.cancelled() => {}
                        res = builder.serve_connection(io, hyper_svc) => {
                            if let Err(e) = res {
                                tracing::debug!(target: "servant", "UDS conn error: {e}");
                            }
                        }
                    }
                });
            }
        }
    }
}

async fn serve_tcp(listener: TcpListener, router: axum::Router, shutdown: CancellationToken) {
    let svc = router.into_make_service();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accept = listener.accept() => {
                let (stream, _addr) = match accept {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(target: "servant", "TCP accept error: {e}");
                        continue;
                    }
                };
                let mut make = svc.clone();
                let tower_svc = make.call(&stream).await.unwrap();
                let hyper_svc = hyper_util::service::TowerToHyperService::new(tower_svc);
                let io = TokioIo::new(stream);
                let conn_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let builder = ConnBuilder::new(TokioExecutor::new());
                    tokio::select! {
                        _ = conn_shutdown.cancelled() => {}
                        res = builder.serve_connection(io, hyper_svc) => {
                            if let Err(e) = res {
                                tracing::debug!(target: "servant", "TCP conn error: {e}");
                            }
                        }
                    }
                });
            }
        }
    }
}

// Convenience shim — Incoming used here so tower-related glue compiles
// against hyper-util's auto Builder.
#[allow(dead_code)]
fn _typetag(_i: Incoming) {}
