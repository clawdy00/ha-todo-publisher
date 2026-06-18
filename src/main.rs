use std::{collections::BTreeMap, env, fs, net::SocketAddr, path::PathBuf, sync::Arc};

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

#[derive(Debug, Deserialize)]
struct FileConfig {
    #[serde(default = "default_bind_addr")]
    bind_addr: String,
    todos: BTreeMap<String, TodoConfig>,
}

#[derive(Debug, Deserialize)]
struct TodoConfig {
    write_token: String,
    read_token: String,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let path = config_path();
        Self::from_path(&path)
            .with_context(|| format!("failed to load config from {}", path.display()))
    }

    fn from_path(path: &PathBuf) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path).with_context(|| "failed to read config file")?;
        let file: FileConfig =
            toml::from_str(&raw).with_context(|| "failed to parse TOML config")?;
        Self::from_file(file)
    }

    fn from_file(file: FileConfig) -> anyhow::Result<Self> {
        if file.todos.is_empty() {
            bail!("config must contain at least one [todos.<slug>] entry");
        }
        let bind = file
            .bind_addr
            .parse()
            .context("bind_addr must be host:port")?;
        let mut write_tokens = BTreeMap::new();
        let mut read_tokens = BTreeMap::new();
        for (slug, todo) in file.todos {
            validate_slug(&slug)
                .map_err(|e| anyhow::anyhow!("invalid todo slug {slug}: {}", e.msg))?;
            validate_token("write_token", &slug, &todo.write_token)?;
            validate_token("read_token", &slug, &todo.read_token)?;
            write_tokens.insert(slug.clone(), todo.write_token);
            read_tokens.insert(slug, todo.read_token);
        }
        Ok(Self {
            bind,
            write_tokens,
            read_tokens,
        })
    }
}

fn config_path() -> PathBuf {
    env::var_os("CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

fn default_bind_addr() -> String {
    "0.0.0.0:8080".to_string()
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

fn validate_token(field: &str, slug: &str, token: &str) -> anyhow::Result<()> {
    if token.len() < 24 {
        bail!("{field} for {slug} must be at least 24 characters");
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_file_config() -> FileConfig {
        FileConfig {
            bind_addr: "127.0.0.1:8080".to_string(),
            todos: BTreeMap::from([
                (
                    "cars".to_string(),
                    TodoConfig {
                        write_token: "w".repeat(32),
                        read_token: "r".repeat(32),
                    },
                ),
                (
                    "shopping".to_string(),
                    TodoConfig {
                        write_token: "x".repeat(32),
                        read_token: "y".repeat(32),
                    },
                ),
            ]),
        }
    }

    #[test]
    fn parses_file_config() {
        let cfg = Config::from_file(test_file_config()).unwrap();
        assert_eq!(cfg.write_tokens.len(), 2);
        assert_eq!(cfg.read_tokens["cars"], "r".repeat(32));
    }

    #[test]
    fn parses_toml_config() {
        let raw = r#"
bind_addr = "127.0.0.1:8080"

[todos.cars]
write_token = "wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww"
read_token = "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr"
"#;
        let file: FileConfig = toml::from_str(raw).unwrap();
        let cfg = Config::from_file(file).unwrap();
        assert!(cfg.write_tokens.contains_key("cars"));
    }

    #[test]
    fn rejects_short_tokens() {
        let mut file = test_file_config();
        file.todos.get_mut("cars").unwrap().read_token = "short".to_string();
        assert!(Config::from_file(file).is_err());
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
