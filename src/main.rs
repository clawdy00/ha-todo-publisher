use std::{collections::BTreeMap, env, net::SocketAddr, sync::Arc};

use anyhow::{bail, Context};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    store: Arc<RwLock<TodoStore>>,
}

#[derive(Debug, Clone)]
struct Config {
    bind: SocketAddr,
    write_tokens: BTreeMap<String, String>,
    read_tokens: BTreeMap<String, String>,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let bind = env::var("BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse()
            .context("BIND_ADDR must be host:port")?;
        let write_tokens = parse_scoped_tokens("WRITE_TOKENS", &required_env("WRITE_TOKENS")?)?;
        let read_tokens = parse_scoped_tokens("READ_TOKENS", &required_env("READ_TOKENS")?)?;
        Ok(Self {
            bind,
            write_tokens,
            read_tokens,
        })
    }
}

#[derive(Debug, Default, Clone, Serialize)]
struct TodoStore {
    updated_at: Option<DateTime<Utc>>,
    lists: BTreeMap<String, PublishedTodoList>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PublishedTodoList {
    slug: String,
    updated_at: DateTime<Utc>,
    list: TodoList,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TodoList {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    items: Vec<TodoItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TodoItem {
    id: String,
    summary: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IngestPayload {
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    #[serde(flatten)]
    list: TodoList,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg = Arc::new(Config::from_env()?);
    let state = AppState {
        cfg: cfg.clone(),
        store: Arc::new(RwLock::new(TodoStore::default())),
    };
    let app = app(state);
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    info!(addr = %cfg.bind, "starting ha-todo-publisher");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/todos/:slug", get(read_todo).post(write_todo))
        .layer(TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
            tracing::info_span!("request", method = %request.method(), uri = %request.uri())
        }))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state)
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        warn!(%err, "failed to install ctrl+c handler");
    }
}

async fn healthz(State(state): State<AppState>) -> Json<serde_json::Value> {
    let store = state.store.read().await;
    Json(serde_json::json!({
        "ok": true,
        "updated_at": store.updated_at,
        "lists": store.lists.len(),
    }))
}

async fn read_todo(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Result<Json<PublishedTodoList>, AppError> {
    validate_slug(&slug)?;
    verify_scoped_bearer(&state.cfg.read_tokens, &slug, &headers, "read")?;
    let store = state.store.read().await;
    let list = store
        .lists
        .get(&slug)
        .cloned()
        .ok_or(AppError::not_found("todo list not found"))?;
    Ok(Json(list))
}

async fn write_todo(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_slug(&slug)?;
    verify_write_auth(&state.cfg, &slug, &headers, &body)?;
    let payload: IngestPayload = serde_json::from_slice(&body).map_err(AppError::bad_request)?;
    let updated_at = payload.updated_at.unwrap_or_else(Utc::now);
    let published = PublishedTodoList {
        slug: slug.clone(),
        updated_at,
        list: payload.list,
    };
    let mut store = state.store.write().await;
    store.updated_at = Some(Utc::now());
    store.lists.insert(slug.clone(), published);
    Ok(Json(serde_json::json!({
        "ok": true,
        "slug": slug,
        "updated_at": updated_at,
    })))
}

async fn auth_middleware(
    State(_state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if path == "/healthz" || path.starts_with("/api/todos/") {
        return next.run(req).await;
    }
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

fn verify_write_auth(
    cfg: &Config,
    slug: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), AppError> {
    if let Some(sig) = headers
        .get("x-ha-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        let Some(hex_sig) = sig.strip_prefix("sha256=") else {
            return Err(AppError::unauthorized("invalid signature format"));
        };
        let Some(token) = cfg.write_tokens.get(slug) else {
            return Err(AppError::forbidden("no write token configured for slug"));
        };
        let expected = hmac_sha256_hex(token.as_bytes(), body);
        if expected.as_bytes().ct_eq(hex_sig.as_bytes()).into() {
            return Ok(());
        }
        return Err(AppError::unauthorized("invalid signature"));
    }

    verify_scoped_bearer(&cfg.write_tokens, slug, headers, "write")
}

fn verify_scoped_bearer(
    tokens: &BTreeMap<String, String>,
    slug: &str,
    headers: &HeaderMap,
    kind: &str,
) -> Result<(), AppError> {
    let supplied = bearer_token(headers)
        .ok_or_else(|| AppError::unauthorized(format!("missing or invalid {kind} token")))?;
    let expected = tokens
        .get(slug)
        .ok_or_else(|| AppError::forbidden(format!("no {kind} token configured for slug")))?;
    if constant_time_eq(supplied.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(AppError::unauthorized(format!(
            "missing or invalid {kind} token"
        )))
    }
}

fn hmac_sha256_hex(key: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.ct_eq(b).into()
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn parse_scoped_tokens(env_name: &str, raw: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (slug, token) = part
            .split_once(':')
            .with_context(|| format!("{env_name} entries must be slug:token"))?;
        validate_slug(slug)
            .map_err(|e| anyhow::anyhow!("invalid {env_name} slug {slug}: {}", e.msg))?;
        if token.len() < 24 {
            bail!("{env_name} token for {slug} must be at least 24 characters");
        }
        if out.insert(slug.to_string(), token.to_string()).is_some() {
            bail!("{env_name} contains duplicate slug {slug}");
        }
    }
    if out.is_empty() {
        bail!("{env_name} must contain at least one slug:token entry");
    }
    Ok(out)
}

fn validate_slug(slug: &str) -> Result<(), AppError> {
    let ok = !slug.is_empty()
        && slug.len() <= 64
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(AppError::bad_request_msg(
            "slug must be 1-64 chars of lowercase letters, digits, '-' or '_'",
        ))
    }
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    msg: String,
}

impl AppError {
    fn bad_request(err: serde_json::Error) -> Self {
        Self::bad_request_msg(format!("invalid JSON: {err}"))
    }
    fn bad_request_msg(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            msg: msg.into(),
        }
    }
    fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            msg: msg.into(),
        }
    }
    fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            msg: msg.into(),
        }
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            msg: msg.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.msg).into_response()
    }
}

fn required_env(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| format!("{name} is required"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scoped_tokens() {
        let tokens = parse_scoped_tokens(
            "READ_TOKENS",
            "cars:abcdefghijklmnopqrstuvwxyz,shopping:123456789012345678901234",
        )
        .unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens["cars"], "abcdefghijklmnopqrstuvwxyz");
    }

    #[test]
    fn rejects_duplicate_slugs() {
        assert!(parse_scoped_tokens(
            "READ_TOKENS",
            "cars:abcdefghijklmnopqrstuvwxyz,cars:123456789012345678901234",
        )
        .is_err());
    }

    #[test]
    fn rejects_bad_slug() {
        assert!(validate_slug("cars").is_ok());
        assert!(validate_slug("car_tasks").is_ok());
        assert!(validate_slug("Cars").is_err());
        assert!(validate_slug("../../x").is_err());
    }

    #[test]
    fn hmac_is_stable() {
        let sig = hmac_sha256_hex(b"secret", br#"{"x":1}"#);
        assert_eq!(sig.len(), 64);
    }
}
