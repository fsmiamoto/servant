//! Control plane router — bound only on the Unix socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::db::{self, Kind, NewRegistration, Registration, SharedDb};
use crate::ttl::Ttl;
use crate::url_alloc::{allocate_url_path, UrlAllocation};

#[derive(Clone)]
pub struct ControlState {
    pub db: SharedDb,
    pub config: Arc<crate::config::Config>,
    pub started_at: Instant,
}

pub fn router(state: ControlState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/list", get(list))
        .route("/register", post(register))
        .route("/registrations/{id}", delete(rm_by_id))
        .route("/by-source", delete(rm_by_source))
        .route("/by-url", delete(rm_by_url))
        .with_state(state)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub uptime_secs: u64,
    pub registrations: i64,
    pub port: u16,
    pub bind: String,
}

async fn health(State(s): State<ControlState>) -> Response {
    let conn = s.db.lock().unwrap();
    let count = db::count(&conn).unwrap_or(0);
    drop(conn);
    let body = HealthResponse {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        uptime_secs: s.started_at.elapsed().as_secs(),
        registrations: count,
        port: s.config.port,
        bind: s.config.bind.clone(),
    };
    (StatusCode::OK, Json(body)).into_response()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RegisterRequest {
    pub path: String,
    pub kind: String,
    pub ttl: TtlRequest,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", content = "value")]
pub enum TtlRequest {
    Seconds(i64),
    Never,
    Default,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterResponse {
    #[serde(flatten)]
    pub registration: RegistrationView,
    pub reused: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RegistrationView {
    pub id: i64,
    pub url_path: String,
    pub source_path: String,
    pub kind: String,
    pub name_slug: Option<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub ttl_seconds: Option<i64>,
    pub last_hit_at: Option<i64>,
    pub missing_since: Option<i64>,
    pub missing: bool,
    pub expires_in_secs: Option<i64>,
}

impl RegistrationView {
    pub fn from(r: Registration, now: i64) -> Self {
        let expires_in = r.expires_at.map(|e| e - now);
        Self {
            id: r.id,
            url_path: r.url_path,
            source_path: r.source_path,
            kind: r.kind.as_str().to_string(),
            name_slug: r.name_slug,
            created_at: r.created_at,
            expires_at: r.expires_at,
            ttl_seconds: r.ttl_seconds,
            last_hit_at: r.last_hit_at,
            missing_since: r.missing_since,
            missing: r.missing_since.is_some(),
            expires_in_secs: expires_in,
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    code: &'static str,
}

fn err(status: StatusCode, code: &'static str, msg: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: msg.into(),
            code,
        }),
    )
        .into_response()
}

async fn register(State(s): State<ControlState>, Json(req): Json<RegisterRequest>) -> Response {
    let path = PathBuf::from(&req.path);
    if !path.is_absolute() {
        return err(
            StatusCode::BAD_REQUEST,
            "non_absolute_path",
            "path must be absolute",
        );
    }
    let kind = match Kind::parse(&req.kind) {
        Ok(k) => k,
        Err(e) => return err(StatusCode::BAD_REQUEST, "bad_kind", e.to_string()),
    };
    let now = db::now_unix();
    let conn = s.db.lock().unwrap();
    let alloc = match allocate_url_path(&conn, &path, kind.clone(), req.name.as_deref()) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::BAD_REQUEST, "alloc", e.to_string()),
    };
    let (registration, reused) = match alloc {
        UrlAllocation::Existing(existing) => {
            if let Err(e) = db::touch(&conn, existing.id, now, existing.ttl_seconds) {
                return err(StatusCode::INTERNAL_SERVER_ERROR, "touch", e.to_string());
            }
            let refreshed = db::find_by_id(&conn, existing.id)
                .ok()
                .flatten()
                .unwrap_or(existing);
            (refreshed, true)
        }
        UrlAllocation::New {
            url_path,
            name_slug,
        } => {
            let (expires_at, ttl_seconds) = resolve_ttl(&req.ttl, now, &s.config.default_ttl);
            let new = NewRegistration {
                url_path,
                source_path: path.to_string_lossy().into(),
                kind,
                name_slug,
                created_at: now,
                expires_at,
                ttl_seconds,
            };
            match db::insert(&conn, &new) {
                Ok(r) => (r, false),
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "insert", e.to_string()),
            }
        }
    };
    drop(conn);
    let body = RegisterResponse {
        registration: RegistrationView::from(registration, now),
        reused,
    };
    (StatusCode::OK, Json(body)).into_response()
}

fn resolve_ttl(req: &TtlRequest, now: i64, default: &Ttl) -> (Option<i64>, Option<i64>) {
    match req {
        TtlRequest::Seconds(n) => (Some(now + n), Some(*n)),
        TtlRequest::Never => (None, None),
        TtlRequest::Default => match default {
            Ttl::Never => (None, None),
            Ttl::Duration(d) => {
                let s = d.as_secs() as i64;
                (Some(now + s), Some(s))
            }
        },
    }
}

async fn list(State(s): State<ControlState>) -> Response {
    let now = db::now_unix();
    let conn = s.db.lock().unwrap();
    let rows = match db::list(&conn) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "list", e.to_string()),
    };
    drop(conn);
    let view: Vec<RegistrationView> = rows
        .into_iter()
        .map(|r| RegistrationView::from(r, now))
        .collect();
    Json(view).into_response()
}

async fn rm_by_id(State(s): State<ControlState>, AxPath(id): AxPath<i64>) -> Response {
    let now = db::now_unix();
    let conn = s.db.lock().unwrap();
    match db::find_by_id(&conn, id) {
        Ok(Some(r)) => {
            if let Err(e) = db::delete(&conn, id) {
                return err(StatusCode::INTERNAL_SERVER_ERROR, "delete", e.to_string());
            }
            Json(serde_json::json!({"removed": RegistrationView::from(r, now)})).into_response()
        }
        Ok(None) => err(
            StatusCode::NOT_FOUND,
            "not_found",
            "no registration with that id",
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "lookup", e.to_string()),
    }
}

#[derive(Deserialize)]
struct BySource {
    source_path: String,
}
#[derive(Deserialize)]
struct ByUrl {
    url_path: String,
}

async fn rm_by_source(State(s): State<ControlState>, Json(b): Json<BySource>) -> Response {
    let now = db::now_unix();
    let conn = s.db.lock().unwrap();
    match db::find_by_source(&conn, Path::new(&b.source_path)) {
        Ok(Some(r)) => {
            let _ = db::delete(&conn, r.id);
            Json(serde_json::json!({"removed": RegistrationView::from(r, now)})).into_response()
        }
        Ok(None) => err(
            StatusCode::NOT_FOUND,
            "not_found",
            "no registration for that source",
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "lookup", e.to_string()),
    }
}

async fn rm_by_url(State(s): State<ControlState>, Json(b): Json<ByUrl>) -> Response {
    let now = db::now_unix();
    let conn = s.db.lock().unwrap();
    match db::find_by_url(&conn, &b.url_path) {
        Ok(Some(r)) => {
            let _ = db::delete(&conn, r.id);
            Json(serde_json::json!({"removed": RegistrationView::from(r, now)})).into_response()
        }
        Ok(None) => err(
            StatusCode::NOT_FOUND,
            "not_found",
            "no registration for that url",
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "lookup", e.to_string()),
    }
}
