use std::{collections::BTreeMap, env, net::SocketAddr, sync::Arc};

use anyhow::{bail, Context};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use maud::{html, Markup, DOCTYPE};
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
    read_tokens: BTreeMap<String, String>,
    public_html: bool,
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
        let public_html = env_bool("PUBLIC_HTML", false)?;
        Ok(Self {
            bind,
            write_token,
            read_tokens,
            public_html,
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
        .route("/", get(index))
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

async fn index(State(state): State<AppState>) -> Html<String> {
    let store = state.store.read().await;
    Html(render_index(&store).into_string())
}

async fn all_todos(State(state): State<AppState>) -> Json<ApiResponse> {
    let store = state.store.read().await;
    Json(ApiResponse {
        updated_at: store.updated_at,
        namespaces: store.namespaces.clone(),
    })
}

async fn namespace_todos(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Result<Json<NamespaceTodos>, AppError> {
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
    if path == "/healthz" || path == "/api/ingest" || (path == "/" && state.cfg.public_html) {
        return next.run(req).await;
    }
    match bearer_token(req.headers()) {
        Some(token) if token_allowed(&state.cfg.read_tokens, token) => next.run(req).await,
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

fn token_allowed(tokens: &BTreeMap<String, String>, supplied: &str) -> bool {
    tokens
        .values()
        .any(|token| constant_time_eq(token.as_bytes(), supplied.as_bytes()))
}

fn parse_read_tokens(raw: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, token) = part
            .split_once(':')
            .context("READ_TOKENS entries must be name:token")?;
        if token.len() < 24 {
            bail!("READ_TOKENS token for {name} must be at least 24 characters");
        }
        validate_namespace(name)
            .map_err(|e| anyhow::anyhow!("invalid READ_TOKENS name {name}: {}", e.msg))?;
        out.insert(name.to_string(), token.to_string());
    }
    if out.is_empty() {
        bail!("READ_TOKENS must contain at least one name:token entry");
    }
    Ok(out)
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

fn render_index(store: &TodoStore) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Home Assistant Todos" }
                style { (STYLE) }
            }
            body {
                main {
                    header {
                        h1 { "Todos" }
                        @if let Some(ts) = store.updated_at {
                            p class="muted" { "Updated " (ts.to_rfc3339()) }
                        } @else {
                            p class="muted" { "No data received yet." }
                        }
                    }
                    @for (name, ns) in &store.namespaces {
                        section class="namespace" {
                            h2 { (name) }
                            p class="muted" { "Namespace updated " (ns.updated_at.to_rfc3339()) }
                            @for list in &ns.lists {
                                article class="list" {
                                    h3 { (list.name.as_deref().unwrap_or(&list.id)) }
                                    @if list.items.is_empty() {
                                        p class="muted" { "No items" }
                                    } @else {
                                        ul {
                                            @for item in &list.items {
                                                li {
                                                    span class="summary" { (item.summary) }
                                                    @if let Some(due) = &item.due { span class="due" { (due) } }
                                                    @if let Some(status) = &item.status { span class="status" { (status) } }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

const STYLE: &str = r#"
:root { color-scheme: light dark; font-family: system-ui, -apple-system, BlinkMacSystemFont, sans-serif; }
body { margin: 0; background: #101318; color: #eef2f7; }
main { max-width: 760px; margin: 0 auto; padding: 24px; }
h1 { margin: 0 0 4px; font-size: 2rem; }
h2 { margin-top: 32px; border-bottom: 1px solid #303642; padding-bottom: 8px; }
h3 { margin: 0 0 12px; }
.muted { color: #9aa4b2; }
.list { background: #171c24; border: 1px solid #2b3340; border-radius: 14px; padding: 16px; margin: 12px 0; }
ul { list-style: none; margin: 0; padding: 0; }
li { display: flex; gap: 8px; align-items: baseline; padding: 9px 0; border-top: 1px solid #252b35; }
li:first-child { border-top: 0; }
.summary { flex: 1; }
.due, .status { color: #a7b3c4; font-size: .85rem; border: 1px solid #374151; border-radius: 999px; padding: 2px 8px; }
"#;

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

fn env_bool(name: &str, default: bool) -> anyhow::Result<bool> {
    match env::var(name) {
        Ok(v) => match v.as_str() {
            "1" | "true" | "TRUE" | "yes" | "YES" => Ok(true),
            "0" | "false" | "FALSE" | "no" | "NO" => Ok(false),
            _ => bail!("{name} must be boolean"),
        },
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_read_tokens() {
        let tokens =
            parse_read_tokens("trmnl:abcdefghijklmnopqrstuvwxyz,wall:123456789012345678901234")
                .unwrap();
        assert_eq!(tokens.len(), 2);
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
