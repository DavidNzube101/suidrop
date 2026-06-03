use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, RawQuery, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

const MAX_BODY: usize = 100 * 1024 * 1024;
const RPC_MIN_INTERVAL: Duration = Duration::from_millis(350);

#[derive(Clone)]
struct Config {
    network: String,
    tatum_api_key: String,
    rpc_url: String,
    walrus_publisher: String,
    walrus_aggregator: String,
    epochs: u32,
    port: u16,
    package_id: String,
}

type ExplorerCache = Arc<Mutex<Option<(Instant, Value)>>>;

#[derive(Clone)]
struct AppState {
    cfg: Config,
    http: reqwest::Client,
    rpc_gate: Arc<Mutex<Instant>>,
    explorer_cache: ExplorerCache,
}

fn env_or(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => default.to_string(),
    }
}

fn resolve_config() -> Config {
    let network = env_or("SUIDROP_NETWORK", "testnet");
    let tatum_api_key = std::env::var("TATUM_API_KEY").unwrap_or_default();

    let rpc_url = format!("https://sui-{network}.gateway.tatum.io");

    let (def_pub, def_agg) = match network.as_str() {
        "mainnet" => (
            "https://publisher.walrus-mainnet.walrus.space",
            "https://aggregator.walrus-mainnet.walrus.space",
        ),
        _ => (
            "https://publisher.walrus-testnet.walrus.space",
            "https://aggregator.walrus-testnet.walrus.space",
        ),
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

    Config {
        network,
        tatum_api_key,
        rpc_url,
        walrus_publisher: env_or("WALRUS_PUBLISHER", def_pub),
        walrus_aggregator: env_or("WALRUS_AGGREGATOR", def_agg),
        epochs,
        port,
        package_id: std::env::var("SUIDROP_PACKAGE_ID").unwrap_or_default(),
    }
}

async fn health_handler(State(s): State<AppState>) -> impl IntoResponse {
    Json(json!({ "status": "ok", "network": s.cfg.network }))
}

async fn config_handler(State(s): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "network": s.cfg.network,
        "epochs": s.cfg.epochs,
        "packageId": s.cfg.package_id,
        "chain": format!("sui:{}", s.cfg.network),
    }))
}

async fn rpc_proxy(State(s): State<AppState>, body: Bytes) -> Response {
    {
        let mut last = s.rpc_gate.lock().await;
        let elapsed = last.elapsed();
        if elapsed < RPC_MIN_INTERVAL {
            tokio::time::sleep(RPC_MIN_INTERVAL - elapsed).await;
        }
        *last = Instant::now();
    }

    let resp = s
        .http
        .post(&s.cfg.rpc_url)
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

async fn walrus_upload(State(s): State<AppState>, body: Bytes) -> Response {
    let url = format!(
        "{}/v1/blobs?epochs={}",
        s.cfg.walrus_publisher.trim_end_matches('/'),
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

async fn walrus_download(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let url = format!(
        "{}/v1/blobs/{}",
        s.cfg.walrus_aggregator.trim_end_matches('/'),
        id
    );

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

async fn tatum_call(s: &AppState, method: &str, params: Value) -> Option<Value> {
    {
        let mut last = s.rpc_gate.lock().await;
        let elapsed = last.elapsed();
        if elapsed < RPC_MIN_INTERVAL {
            tokio::time::sleep(RPC_MIN_INTERVAL - elapsed).await;
        }
        *last = Instant::now();
    }
    let body = json!({ "id": 1, "jsonrpc": "2.0", "method": method, "params": params });
    let resp = s
        .http
        .post(&s.cfg.rpc_url)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .header("x-api-key", &s.cfg.tatum_api_key)
        .json(&body)
        .send()
        .await
        .ok()?;
    resp.json().await.ok()
}

async fn explorer_handler(State(s): State<AppState>) -> impl IntoResponse {
    if s.cfg.package_id.is_empty() {
        return Json(json!({ "drops": 0, "files": 0, "totalSize": 0, "recent": [] }));
    }

    {
        let cache = s.explorer_cache.lock().await;
        if let Some((at, value)) = cache.as_ref() {
            if at.elapsed() < Duration::from_secs(60) {
                return Json(value.clone());
            }
        }
    }

    let event_type = format!("{}::receipt::DropCreated", s.cfg.package_id);
    let mut cursor = Value::Null;
    let mut drops: u64 = 0;
    let mut total: u64 = 0;
    let mut recent: Vec<Value> = Vec::new();
    let mut pages = 0;

    loop {
        let params = json!([{ "MoveEventType": event_type }, cursor, 50, true]);
        let res = match tatum_call(&s, "suix_queryEvents", params).await {
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

    let out = json!({ "drops": drops, "files": drops, "totalSize": total, "recent": recent });
    {
        let mut cache = s.explorer_cache.lock().await;
        *cache = Some((Instant::now(), out.clone()));
    }
    Json(out)
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

    let state = AppState {
        http: reqwest::Client::new(),
        rpc_gate: Arc::new(Mutex::new(Instant::now() - RPC_MIN_INTERVAL)),
        explorer_cache: Arc::new(Mutex::new(None)),
        cfg: cfg.clone(),
    };

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/api/config", get(config_handler))
        .route("/api/rpc", post(rpc_proxy))
        .route("/api/walrus/upload", post(walrus_upload))
        .route("/api/walrus/blob/:id", get(walrus_download))
        .route("/api/explorer", get(explorer_handler))
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
        "SuiDrop listening on http://{addr} (network: {}, walrus: {})",
        cfg.network,
        cfg.walrus_publisher
    );

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind port");
    axum::serve(listener, app).await.expect("server crashed");
}
