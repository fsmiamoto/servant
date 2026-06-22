//! HTTP-over-Unix-Socket client for the CLI. Wraps hyper directly
//! because the daemon's control endpoints don't need anything fancier.

use std::io;
use std::path::Path;

use anyhow::{anyhow, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use tokio::net::UnixStream;

use crate::cli::{InstallArgs, RmArgs, ServeArgs};
use crate::config::Config;
use crate::control::{
    HealthResponse, RegisterRequest, RegisterResponse, RegistrationView, TtlRequest,
};
use crate::output::OutputMode;
use crate::ttl::{format_expires_in, Ttl};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("daemon not running ({0})")]
    Unreachable(String),
}

pub struct ControlClient {
    socket: std::path::PathBuf,
}

impl ControlClient {
    pub fn new(cfg: &Config) -> Self {
        Self {
            socket: cfg.control_socket_path(),
        }
    }

    async fn request<B: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&B>,
    ) -> Result<R> {
        let stream = match UnixStream::connect(&self.socket).await {
            Ok(s) => s,
            Err(e)
                if e.kind() == io::ErrorKind::ConnectionRefused
                    || e.kind() == io::ErrorKind::NotFound =>
            {
                return Err(ClientError::Unreachable(format!(
                    "no socket at {}\nhint: run `servant service start` or `servant service install`",
                    self.socket.display()
                ))
                .into());
            }
            Err(e) => return Err(e.into()),
        };

        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let body_bytes = match body {
            Some(b) => Bytes::from(serde_json::to_vec(b)?),
            None => Bytes::new(),
        };
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("content-length", body_bytes.len().to_string())
            .body(Full::new(body_bytes))?;

        let resp = sender.send_request(req).await?;
        let status = resp.status();
        let collected = resp.into_body().collect().await?.to_bytes();
        if !status.is_success() {
            let err_msg = parse_error_msg(&collected)
                .unwrap_or_else(|| format!("daemon returned HTTP {status}"));
            return Err(anyhow!(err_msg));
        }
        if collected.is_empty() {
            // R should be deserializable from "null".
            return Ok(serde_json::from_slice(b"null")?);
        }
        Ok(serde_json::from_slice(&collected)?)
    }

    pub async fn register(&self, req: RegisterRequest) -> Result<RegisterResponse> {
        self.request::<_, _>("POST", "/register", Some(&req)).await
    }
    pub async fn list(&self) -> Result<Vec<RegistrationView>> {
        self.request::<(), _>("GET", "/list", None).await
    }
    pub async fn rm_by_id(&self, id: i64) -> Result<serde_json::Value> {
        self.request::<(), _>("DELETE", &format!("/registrations/{id}"), None)
            .await
    }
    pub async fn rm_by_url(&self, url_path: &str) -> Result<serde_json::Value> {
        self.request::<_, _>(
            "DELETE",
            "/by-url",
            Some(&serde_json::json!({"url_path": url_path})),
        )
        .await
    }
    pub async fn rm_by_source(&self, source_path: &str) -> Result<serde_json::Value> {
        self.request::<_, _>(
            "DELETE",
            "/by-source",
            Some(&serde_json::json!({"source_path": source_path})),
        )
        .await
    }
    pub async fn health(&self) -> Result<HealthResponse> {
        self.request::<(), _>("GET", "/health", None).await
    }
}

fn parse_error_msg(bytes: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    v.get("error")?.as_str().map(|s| s.to_string())
}

// ----- handlers used from cli.rs --------------------------------------

pub async fn handle_serve(cfg: Config, args: ServeArgs, mode: OutputMode) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let (abs, kind) = crate::paths::canonicalize_user_path(&args.path.to_string_lossy(), &cwd)?;

    let ttl_req = match args.ttl {
        Some(Ttl::Never) => TtlRequest::Never,
        Some(Ttl::Duration(d)) => TtlRequest::Seconds(d.as_secs() as i64),
        None => TtlRequest::Default,
    };
    let req = RegisterRequest {
        path: abs.to_string_lossy().into(),
        kind: kind.as_str().into(),
        ttl: ttl_req,
        name: args.name.clone(),
    };
    let client = ControlClient::new(&cfg);
    let resp = client.register(req).await?;
    let host = crate::host::resolve_public_host(&cfg);
    let url = format!("http://{}:{}{}", host, cfg.port, resp.registration.url_path);

    match mode {
        OutputMode::Json => {
            let mut v = serde_json::to_value(&resp)?;
            if let serde_json::Value::Object(ref mut map) = v {
                map.insert("url".into(), serde_json::Value::String(url));
            }
            println!("{}", serde_json::to_string(&v)?);
        }
        OutputMode::Human => {
            let ttl_label = format_expires_in(resp.registration.expires_in_secs);
            let reused = if resp.reused { "  (reused)" } else { "" };
            println!(
                "Serving {url}  (id={}, expires in {ttl_label}){reused}",
                resp.registration.id
            );
        }
    }
    Ok(0)
}

pub async fn handle_ls(cfg: Config, mode: OutputMode) -> Result<i32> {
    let client = ControlClient::new(&cfg);
    let rows = client.list().await?;
    let host = crate::host::resolve_public_host(&cfg);
    match mode {
        OutputMode::Json => println!("{}", serde_json::to_string(&rows)?),
        OutputMode::Human => {
            if rows.is_empty() {
                println!("(no registrations)");
                return Ok(0);
            }
            println!("{:<5}  {:<40}  {:<14}  SOURCE", "ID", "URL", "EXPIRES");
            for r in rows {
                let url = format!("http://{}:{}{}", host, cfg.port, r.url_path);
                let expires = format_expires_in(r.expires_in_secs);
                let miss = if r.missing { "  [MISSING]" } else { "" };
                println!(
                    "{:<5}  {:<40}  {:<14}  {}{miss}",
                    r.id, url, expires, r.source_path
                );
            }
        }
    }
    Ok(0)
}

pub async fn handle_rm(cfg: Config, args: RmArgs, mode: OutputMode) -> Result<i32> {
    let client = ControlClient::new(&cfg);
    let removed = if args.target.chars().all(|c| c.is_ascii_digit()) {
        client.rm_by_id(args.target.parse()?).await?
    } else if args.target.starts_with('/') && !std::path::Path::new(&args.target).exists() {
        client.rm_by_url(&args.target).await?
    } else if args.target.starts_with("http://") {
        let p = args
            .target
            .splitn(4, '/')
            .nth(3)
            .map(|s| format!("/{s}"))
            .unwrap_or(args.target.clone());
        client.rm_by_url(&p).await?
    } else {
        let cwd = std::env::current_dir()?;
        let (abs, _) = crate::paths::canonicalize_user_path(&args.target, &cwd)?;
        client.rm_by_source(&abs.to_string_lossy()).await?
    };
    match mode {
        OutputMode::Json => println!("{}", serde_json::to_string(&removed)?),
        OutputMode::Human => {
            let url = removed
                .get("removed")
                .and_then(|r| r.get("url_path"))
                .and_then(|u| u.as_str())
                .unwrap_or("(unknown)");
            println!("Removed {url}");
        }
    }
    Ok(0)
}

pub async fn handle_status(cfg: Config, mode: OutputMode) -> Result<i32> {
    let client = ControlClient::new(&cfg);
    let h = client.health().await?;
    match mode {
        OutputMode::Json => println!("{}", serde_json::to_string(&h)?),
        OutputMode::Human => {
            println!(
                "daemon: {} (v{}) bind={}:{} registrations={} uptime={}s",
                h.status, h.version, h.bind, h.port, h.registrations, h.uptime_secs
            );
        }
    }
    Ok(0)
}

#[allow(dead_code)]
fn _silence_unused_imports(_p: &Path) {}

#[allow(dead_code)]
pub fn _placeholder_install(_a: InstallArgs) {}
