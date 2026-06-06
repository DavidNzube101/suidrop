use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, RawQuery, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::Mutex;
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

const MAX_BODY: usize = 100 * 1024 * 1024;
const RPC_MIN_INTERVAL: Duration = Duration::from_millis(350);

#[derive(Clone)]
struct NetCfg {
    rpc_url: String,
    publisher: String,
    aggregator: String,
    package_id: String,
}

#[derive(Clone)]
struct Config {
    tatum_api_key: String,
    default_network: String,
    epochs: u32,
    port: u16,
    testnet: NetCfg,
    mainnet: NetCfg,
}

impl Config {
    fn net(&self, network: &str) -> &NetCfg {
        if network == "mainnet" {
            &self.mainnet
        } else {
            &self.testnet
        }
    }

    fn pick(&self, requested: Option<&str>) -> (String, &NetCfg) {
        let name = match requested {
            Some("mainnet") => "mainnet",
            Some("testnet") => "testnet",
            _ if self.default_network == "mainnet" => "mainnet",
            _ => "testnet",
        };
        (name.to_string(), self.net(name))
    }
}

#[derive(Deserialize)]
struct NetQuery {
    network: Option<String>,
}

type ExplorerCache = Arc<Mutex<HashMap<String, (Instant, Value)>>>;

#[derive(Clone)]
struct AppState {
    cfg: Config,
    http: reqwest::Client,
    rpc_gate: Arc<Mutex<Instant>>,
    explorer_cache: ExplorerCache,
    db: Option<PgPool>,
}

fn env_or(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => default.to_string(),
    }
}

fn env_first(keys: &[&str]) -> String {
    for k in keys {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return v;
            }
        }
    }
    String::new()
}

fn net_cfg(network: &str, package_id: String) -> NetCfg {
    let (pub_, agg) = match network {
        "mainnet" => (
            "https://publisher.walrus-mainnet.walrus.space",
            "https://aggregator.walrus-mainnet.walrus.space",
        ),
        _ => (
            "https://publisher.walrus-testnet.walrus.space",
            "https://aggregator.walrus-testnet.walrus.space",
        ),
    };
    NetCfg {
        rpc_url: format!("https://sui-{network}.gateway.tatum.io"),
        publisher: pub_.to_string(),
        aggregator: agg.to_string(),
        package_id,
    }
}

fn resolve_config() -> Config {
    let tatum_api_key = std::env::var("TATUM_API_KEY").unwrap_or_default();
    let default_network = match env_or("SUIDROP_DEFAULT_NETWORK", "mainnet").as_str() {
        "testnet" => "testnet".to_string(),
        _ => "mainnet".to_string(),
    };

    let epochs = std::env::var("WALRUS_EPOCHS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let port = std::env::var("PORT")
        .or_else(|_| std::env::var("SUIDROP_PORT"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);

    let testnet_pkg = env_first(&["SUIDROP_PACKAGE_ID_TESTNET", "SUIDROP_PACKAGE_ID"]);
    let mainnet_pkg = env_first(&["SUIDROP_PACKAGE_ID_MAINNET"]);

    Config {
        tatum_api_key,
        default_network,
        epochs,
        port,
        testnet: net_cfg("testnet", testnet_pkg),
        mainnet: net_cfg("mainnet", mainnet_pkg),
    }
}

async fn rpc_gate(s: &AppState) {
    let mut last = s.rpc_gate.lock().await;
    let elapsed = last.elapsed();
    if elapsed < RPC_MIN_INTERVAL {
        tokio::time::sleep(RPC_MIN_INTERVAL - elapsed).await;
    }
    *last = Instant::now();
}

async fn health_handler(State(s): State<AppState>) -> impl IntoResponse {
    Json(json!({ "status": "ok", "network": s.cfg.default_network }))
}

async fn official_network_handler(State(s): State<AppState>) -> impl IntoResponse {
    Json(json!({ "status": 200, "network": format!("sui-{}", s.cfg.default_network) }))
}

async fn config_handler(State(s): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "defaultNetwork": s.cfg.default_network,
        "epochs": s.cfg.epochs,
        "shorten": s.db.is_some(),
        "networks": {
            "testnet": { "packageId": s.cfg.testnet.package_id, "chain": "sui:testnet" },
            "mainnet": { "packageId": s.cfg.mainnet.package_id, "chain": "sui:mainnet" },
        },
    }))
}

async fn rpc_proxy(State(s): State<AppState>, Query(q): Query<NetQuery>, body: Bytes) -> Response {
    let (_, net) = s.cfg.pick(q.network.as_deref());
    let rpc_url = net.rpc_url.clone();
    rpc_gate(&s).await;

    let resp = s
        .http
        .post(&rpc_url)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .header("x-api-key", &s.cfg.tatum_api_key)
        .body(body)
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status =
                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = r.bytes().await.unwrap_or_default();
            (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("rpc proxy error: {e}")).into_response(),
    }
}

async fn walrus_upload(
    State(s): State<AppState>,
    Query(q): Query<NetQuery>,
    body: Bytes,
) -> Response {
    let (_, net) = s.cfg.pick(q.network.as_deref());
    let url = format!(
        "{}/v1/blobs?epochs={}",
        net.publisher.trim_end_matches('/'),
        s.cfg.epochs
    );

    let resp = s
        .http
        .put(&url)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(body)
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status =
                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = r.bytes().await.unwrap_or_default();
            (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("walrus upload error: {e}")).into_response(),
    }
}

async fn walrus_download(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<NetQuery>,
) -> Response {
    let (_, net) = s.cfg.pick(q.network.as_deref());
    let url = format!("{}/v1/blobs/{}", net.aggregator.trim_end_matches('/'), id);

    match s.http.get(&url).send().await {
        Ok(r) => {
            let status =
                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = r.bytes().await.unwrap_or_default();
            (
                status,
                [(header::CONTENT_TYPE, "application/octet-stream")],
                bytes,
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("walrus download error: {e}"),
        )
            .into_response(),
    }
}

fn rewrite_esm(src: &str) -> String {
    src.replace("from\"/", "from\"/esm/")
        .replace("from \"/", "from \"/esm/")
        .replace("import\"/", "import\"/esm/")
        .replace("import \"/", "import \"/esm/")
        .replace("import(\"/", "import(\"/esm/")
}

async fn esm_proxy(
    State(s): State<AppState>,
    Path(path): Path<String>,
    RawQuery(q): RawQuery,
) -> Response {
    let mut url = format!("https://esm.sh/{path}");
    if let Some(q) = q.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(&q);
    }

    match s.http.get(&url).send().await {
        Ok(r) => {
            let status =
                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let ct = r
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/javascript")
                .to_string();
            let body = r.bytes().await.unwrap_or_default();

            if ct.contains("javascript") || ct.contains("typescript") {
                let rewritten = rewrite_esm(&String::from_utf8_lossy(&body));
                (
                    status,
                    [
                        (
                            header::CONTENT_TYPE,
                            "application/javascript; charset=utf-8",
                        ),
                        (header::CACHE_CONTROL, "public, max-age=86400"),
                    ],
                    rewritten,
                )
                    .into_response()
            } else {
                (status, [(header::CONTENT_TYPE, ct)], body).into_response()
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("esm proxy error: {e}")).into_response(),
    }
}

async fn tatum_call(s: &AppState, rpc_url: &str, method: &str, params: Value) -> Option<Value> {
    rpc_gate(s).await;
    let body = json!({ "id": 1, "jsonrpc": "2.0", "method": method, "params": params });
    let resp = s
        .http
        .post(rpc_url)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .header("x-api-key", &s.cfg.tatum_api_key)
        .json(&body)
        .send()
        .await
        .ok()?;
    resp.json().await.ok()
}

async fn explorer_handler(
    State(s): State<AppState>,
    Query(q): Query<NetQuery>,
) -> impl IntoResponse {
    let (name, net) = s.cfg.pick(q.network.as_deref());
    if net.package_id.is_empty() {
        return Json(
            json!({ "network": name, "drops": 0, "files": 0, "totalSize": 0, "recent": [] }),
        );
    }

    {
        let cache = s.explorer_cache.lock().await;
        if let Some((at, value)) = cache.get(&name) {
            if at.elapsed() < Duration::from_secs(60) {
                return Json(value.clone());
            }
        }
    }

    let rpc_url = net.rpc_url.clone();
    let event_type = format!("{}::receipt::DropCreated", net.package_id);
    let mut cursor = Value::Null;
    let mut drops: u64 = 0;
    let mut total: u64 = 0;
    let mut recent: Vec<Value> = Vec::new();
    let mut pages = 0;

    loop {
        let params = json!([{ "MoveEventType": event_type }, cursor, 50, true]);
        let res = match tatum_call(&s, &rpc_url, "suix_queryEvents", params).await {
            Some(r) => r,
            None => break,
        };
        let result = &res["result"];
        let data = result["data"].as_array().cloned().unwrap_or_default();

        for e in &data {
            let pj = &e["parsedJson"];
            let size = pj["size"]
                .as_str()
                .and_then(|x| x.parse::<u64>().ok())
                .unwrap_or(0);
            drops += 1;
            total = total.saturating_add(size);
            if recent.len() < 30 {
                recent.push(json!({
                    "sender": pj["sender"],
                    "recipient": pj["recipient"],
                    "blobId": pj["blob_id"],
                    "receiptId": pj["receipt_id"],
                    "size": size,
                    "createdAtMs": pj["created_at_ms"],
                    "txDigest": e["id"]["txDigest"],
                }));
            }
        }

        pages += 1;
        let has_next = result["hasNextPage"].as_bool().unwrap_or(false);
        if !has_next || pages >= 20 {
            break;
        }
        cursor = result["nextCursor"].clone();
    }

    let out = json!({ "network": name, "drops": drops, "files": drops, "totalSize": total, "recent": recent });
    {
        let mut cache = s.explorer_cache.lock().await;
        cache.insert(name, (Instant::now(), out.clone()));
    }
    Json(out)
}

#[derive(Deserialize)]
struct ShortenReq {
    path: String,
}

fn valid_target(p: &str) -> bool {
    p.starts_with("/app?")
        && p.len() < 1024
        && !p.contains("://")
        && !p.chars().any(|c| c.is_whitespace())
}

fn gen_code() -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..7)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

async fn shorten_handler(State(s): State<AppState>, Json(req): Json<ShortenReq>) -> Response {
    let pool = match &s.db {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, "shortening disabled").into_response(),
    };
    if !valid_target(&req.path) {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }

    for _ in 0..5 {
        let code = gen_code();
        let res = sqlx::query("INSERT INTO links (code, target) VALUES ($1, $2)")
            .bind(&code)
            .bind(&req.path)
            .persistent(false)
            .execute(pool)
            .await;
        match res {
            Ok(_) => {
                return Json(json!({ "code": code, "short": format!("/s/{code}") })).into_response()
            }
            Err(e) => {
                let dup = e
                    .as_database_error()
                    .map(|d| d.is_unique_violation())
                    .unwrap_or(false);
                if !dup {
                    return (StatusCode::BAD_GATEWAY, "could not store link").into_response();
                }
            }
        }
    }
    (StatusCode::INTERNAL_SERVER_ERROR, "could not allocate code").into_response()
}

async fn short_redirect(State(s): State<AppState>, Path(code): Path<String>) -> Response {
    let pool = match &s.db {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let row = sqlx::query_scalar::<_, String>("SELECT target FROM links WHERE code = $1")
        .bind(&code)
        .persistent(false)
        .fetch_optional(pool)
        .await;
    match row {
        Ok(Some(target)) if target.starts_with("/app") => Redirect::to(&target).into_response(),
        Ok(_) => (StatusCode::NOT_FOUND, "link not found").into_response(),
        Err(_) => (StatusCode::BAD_GATEWAY, "lookup failed").into_response(),
    }
}

const MIGRATIONS: &[(&str, &str)] =
    &[("0001_init", include_str!("../db/migrations/0001_init.sql"))];

async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version text PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())",
    )
    .execute(pool)
    .await?;

    for (version, sql) in MIGRATIONS {
        let applied: Option<(String,)> =
            sqlx::query_as("SELECT version FROM schema_migrations WHERE version = $1")
                .bind(version)
                .persistent(false)
                .fetch_optional(pool)
                .await?;
        if applied.is_some() {
            continue;
        }
        sqlx::raw_sql(sql).execute(pool).await?;
        sqlx::query("INSERT INTO schema_migrations (version) VALUES ($1)")
            .bind(version)
            .persistent(false)
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn connect_db() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())?;
    match PgPoolOptions::new().max_connections(5).connect(&url).await {
        Ok(pool) => {
            if let Err(e) = run_migrations(&pool).await {
                tracing::warn!("migrations failed: {e}. Shortening disabled.");
                return None;
            }
            tracing::info!("link shortening enabled (Postgres connected, migrations applied)");
            Some(pool)
        }
        Err(e) => {
            tracing::warn!("DATABASE_URL set but connection failed: {e}. Shortening disabled.");
            None
        }
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "suidrop=info,tower_http=info".into()),
        )
        .init();

    let cfg = resolve_config();
    if cfg.tatum_api_key.is_empty() {
        tracing::warn!("TATUM_API_KEY is empty. RPC proxy calls will fail. Set it in .env");
    }

    let db = connect_db().await;

    let state = AppState {
        http: reqwest::Client::new(),
        rpc_gate: Arc::new(Mutex::new(Instant::now() - RPC_MIN_INTERVAL)),
        explorer_cache: Arc::new(Mutex::new(HashMap::new())),
        db,
        cfg: cfg.clone(),
    };

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/api/official-network", get(official_network_handler))
        .route("/api/config", get(config_handler))
        .route("/api/rpc", post(rpc_proxy))
        .route("/api/walrus/upload", post(walrus_upload))
        .route("/api/walrus/blob/:id", get(walrus_download))
        .route("/api/explorer", get(explorer_handler))
        .route("/api/shorten", post(shorten_handler))
        .route("/s/:code", get(short_redirect))
        .route("/esm/*path", get(esm_proxy))
        .route_service("/", ServeFile::new("frontend/landing.html"))
        .route_service("/app", ServeFile::new("frontend/app.html"))
        .nest_service("/media", ServeDir::new("media"))
        .fallback_service(ServeDir::new("frontend"))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], cfg.port));
    tracing::info!(
        "SuiDrop listening on http://{addr} (default network: {})",
        cfg.default_network
    );

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind port");
    axum::serve(listener, app).await.expect("server crashed");
}
