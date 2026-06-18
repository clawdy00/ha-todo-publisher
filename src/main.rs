use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    net::SocketAddr,
    sync::Arc,
};

use anyhow::{bail, Context};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
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
    write_token: String,
    read_tokens: BTreeMap<String, ReadToken>,
}

#[derive(Debug, Clone)]
struct ReadToken {
    token: String,
    namespaces: BTreeSet<String>,
}

impl ReadToken {
    fn allows(&self, namespace: &str) -> bool {
        self.namespaces.contains("*") || self.namespaces.contains(namespace)
    }
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let bind = env::var("BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
            .parse()
            .context("BIND_ADDR must be host:port")?;
        let write_token = required_env("WRITE_TOKEN")?;
        if write_token.len() < 24 {
            bail!("WRITE_TOKEN must be at least 24 characters");
        }
        let read_tokens = parse_read_tokens(&required_env("READ_TOKENS")?)?;
        Ok(Self {
            bind,
            write_token,
            read_tokens,
        })
    }
}

#[derive(Debug, Default, Clone, Serialize)]
struct TodoStore {
    updated_at: Option<DateTime<Utc>>,
    namespaces: BTreeMap<String, NamespaceTodos>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NamespaceTodos {
    namespace: String,
    updated_at: DateTime<Utc>,
    lists: Vec<TodoList>,
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
    namespace: String,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    lists: Vec<TodoList>,
}

#[derive(Debug, Serialize)]
struct ApiResponse {
    updated_at: Option<DateTime<Utc>>,
    namespaces: BTreeMap<String, NamespaceTodos>,
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
        .route("/api/todos", get(all_todos))
        .route("/api/todos/:namespace", get(namespace_todos))
        .route("/api/ingest", post(ingest))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request<_>| tracing::info_span!("request", method = %request.method(), uri = %request.uri()))
        )
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
        "namespaces": store.namespaces.len(),
    }))
}

async fn all_todos(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse>, AppError> {
    let scope = read_scope_for_headers(&state.cfg, &headers)
        .ok_or(AppError::unauthorized("missing or invalid read token"))?;
    let store = state.store.read().await;
    let namespaces = store
        .namespaces
        .iter()
        .filter(|(name, _)| scope.allows(name))
        .map(|(name, todos)| (name.clone(), todos.clone()))
        .collect();
    Ok(Json(ApiResponse {
        updated_at: store.updated_at,
        namespaces,
    }))
}

async fn namespace_todos(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Json<NamespaceTodos>, AppError> {
    let scope = read_scope_for_headers(&state.cfg, &headers)
        .ok_or(AppError::unauthorized("missing or invalid read token"))?;
    if !scope.allows(&namespace) {
        return Err(AppError::forbidden(
            "read token is not allowed for namespace",
        ));
    }
    let store = state.store.read().await;
    let todos = store
        .namespaces
        .get(&namespace)
        .cloned()
        .ok_or(AppError::not_found("namespace not found"))?;
    Ok(Json(todos))
}

async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    verify_write_auth(&state.cfg, &headers, &body)?;
    let payload: IngestPayload = serde_json::from_slice(&body).map_err(AppError::bad_request)?;
    validate_namespace(&payload.namespace)?;
    let updated_at = payload.updated_at.unwrap_or_else(Utc::now);
    let namespace = NamespaceTodos {
        namespace: payload.namespace.clone(),
        updated_at,
        lists: payload.lists,
    };
    let mut store = state.store.write().await;
    store.updated_at = Some(Utc::now());
    store
        .namespaces
        .insert(payload.namespace.clone(), namespace);
    Ok(Json(serde_json::json!({
        "ok": true,
        "namespace": payload.namespace,
        "updated_at": updated_at,
    })))
}

async fn auth_middleware(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if path == "/healthz" || path == "/api/ingest" {
        return next.run(req).await;
    }
    match bearer_token(req.headers()) {
        Some(token) if read_scope_for_token(&state.cfg.read_tokens, token).is_some() => {
            next.run(req).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "missing or invalid read token\n",
        )
            .into_response(),
    }
}

fn verify_write_auth(cfg: &Config, headers: &HeaderMap, body: &[u8]) -> Result<(), AppError> {
    if let Some(sig) = headers
        .get("x-ha-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        let Some(hex_sig) = sig.strip_prefix("sha256=") else {
            return Err(AppError::unauthorized("invalid signature format"));
        };
        let expected = hmac_sha256_hex(cfg.write_token.as_bytes(), body);
        if expected.as_bytes().ct_eq(hex_sig.as_bytes()).into() {
            return Ok(());
        }
        return Err(AppError::unauthorized("invalid signature"));
    }

    match bearer_token(headers) {
        Some(token) if constant_time_eq(token.as_bytes(), cfg.write_token.as_bytes()) => Ok(()),
        _ => Err(AppError::unauthorized("missing or invalid write token")),
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

fn read_scope_for_headers<'a>(cfg: &'a Config, headers: &HeaderMap) -> Option<&'a ReadToken> {
    read_scope_for_token(&cfg.read_tokens, bearer_token(headers)?)
}

fn read_scope_for_token<'a>(
    tokens: &'a BTreeMap<String, ReadToken>,
    supplied: &str,
) -> Option<&'a ReadToken> {
    let mut matched = None;
    for entry in tokens.values() {
        if constant_time_eq(entry.token.as_bytes(), supplied.as_bytes()) {
            matched = Some(entry);
        }
    }
    matched
}

fn parse_read_tokens(raw: &str) -> anyhow::Result<BTreeMap<String, ReadToken>> {
    let mut out = BTreeMap::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let mut fields = part.splitn(3, ':');
        let name = fields
            .next()
            .context("READ_TOKENS entries must be name:token:namespace[+namespace]")?;
        let token = fields
            .next()
            .context("READ_TOKENS entries must be name:token:namespace[+namespace]")?;
        let namespaces_raw = fields
            .next()
            .context("READ_TOKENS entries must be name:token:namespace[+namespace]")?;
        if token.len() < 24 {
            bail!("READ_TOKENS token for {name} must be at least 24 characters");
        }
        validate_token_name(name)
            .map_err(|e| anyhow::anyhow!("invalid READ_TOKENS name {name}: {}", e.msg))?;
        let namespaces = parse_allowed_namespaces(name, namespaces_raw)?;
        out.insert(
            name.to_string(),
            ReadToken {
                token: token.to_string(),
                namespaces,
            },
        );
    }
    if out.is_empty() {
        bail!("READ_TOKENS must contain at least one name:token:namespace entry");
    }
    Ok(out)
}

fn parse_allowed_namespaces(name: &str, raw: &str) -> anyhow::Result<BTreeSet<String>> {
    let mut namespaces = BTreeSet::new();
    for namespace in raw.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        if namespace == "*" {
            namespaces.insert(namespace.to_string());
        } else {
            validate_namespace(namespace).map_err(|e| {
                anyhow::anyhow!(
                    "invalid namespace {namespace} for READ_TOKENS name {name}: {}",
                    e.msg
                )
            })?;
            namespaces.insert(namespace.to_string());
        }
    }
    if namespaces.is_empty() {
        bail!("READ_TOKENS entry {name} must allow at least one namespace");
    }
    Ok(namespaces)
}

fn validate_namespace(ns: &str) -> Result<(), AppError> {
    let ok = !ns.is_empty()
        && ns.len() <= 64
        && ns
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(AppError::bad_request_msg(
            "namespace must be 1-64 chars of lowercase letters, digits, '-' or '_'",
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

fn validate_token_name(name: &str) -> Result<(), AppError> {
    validate_namespace(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_read_tokens() {
        let tokens = parse_read_tokens(
            "trmnl:abcdefghijklmnopqrstuvwxyz:shopping,wall:123456789012345678901234:home+shopping",
        )
        .unwrap();
        assert_eq!(tokens.len(), 2);
        assert!(tokens["trmnl"].allows("shopping"));
        assert!(!tokens["trmnl"].allows("home"));
        assert!(tokens["wall"].allows("home"));
        assert!(tokens["wall"].allows("shopping"));
    }

    #[test]
    fn rejects_read_tokens_without_scope() {
        assert!(parse_read_tokens("trmnl:abcdefghijklmnopqrstuvwxyz").is_err());
    }

    #[test]
    fn rejects_bad_namespace() {
        assert!(validate_namespace("home").is_ok());
        assert!(validate_namespace("Home").is_err());
        assert!(validate_namespace("../../x").is_err());
    }

    #[test]
    fn hmac_is_stable() {
        let sig = hmac_sha256_hex(b"secret", br#"{"x":1}"#);
        assert_eq!(sig.len(), 64);
    }
}
