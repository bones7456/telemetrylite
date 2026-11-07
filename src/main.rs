mod admin;
mod db;
mod geo;
mod i18n;
mod ingest;

use axum::{Router, extract::DefaultBodyLimit, response::Html, routing::post};
use rusqlite::Connection;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio::sync::mpsc;

pub struct AppState {
    pub tx: mpsc::Sender<Vec<db::Signal>>,
    /// 写入 key 的 SHA-256(hex) -> app_id,热路径只查内存
    pub keys: RwLock<HashMap<String, String>>,
    /// 管理端/查询用连接(写入走独立线程)
    pub db: Mutex<Connection>,
    pub geo: Arc<geo::Geo>,
    pub trusted: Vec<IpAddr>,
    pub salt: String,
    /// 会话 token 的 SHA-256 -> 过期时间(ms);重启后需重新登录
    pub sessions: Mutex<HashMap<String, i64>>,
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();

    let listen = env("TL_LISTEN").unwrap_or_else(|| "127.0.0.1:8888".into());
    let db_path = env("TL_DB_PATH").unwrap_or_else(|| "./telemetrylite.db".into());
    let auto_update = env("TL_GEOIP_AUTO_UPDATE").is_some_and(|v| v == "true" || v == "1");
    let geo_path = env("TL_GEOIP_DB").map(PathBuf::from).or_else(|| auto_update.then(|| "./dbip-city-lite.mmdb".into()));
    let trusted = env("TL_TRUSTED_PROXY").map(|v| v.split(',').filter_map(|i| i.trim().parse().ok()).collect()).unwrap_or_default();

    let conn = db::open(&db_path).unwrap_or_else(|e| panic!("{}: {e}", i18n::tr("failed to open the database", "打开数据库失败")));
    db::init(&conn).unwrap_or_else(|e| panic!("{}: {e}", i18n::tr("failed to initialize the schema", "初始化表失败")));
    let salt = db::meta_get(&conn, "salt").unwrap_or_else(|| {
        let s = hex::encode(rand::random::<[u8; 16]>());
        db::meta_set(&conn, "salt", &s).unwrap();
        s
    });
    setup_admin_password(&conn);
    let keys = admin::load_keys(&conn).unwrap_or_else(|e| panic!("{}: {e}", i18n::tr("failed to load API keys", "加载 key 失败")));

    let (tx, writer) = db::spawn_writer(db_path.clone());
    let geo = Arc::new(geo::Geo::new(geo_path));
    if auto_update {
        tokio::spawn(geo::auto_update(geo.clone()));
    }
    tokio::spawn(retention_job(db_path));

    let state = Arc::new(AppState { tx, keys: RwLock::new(keys), db: Mutex::new(conn), geo, trusted, salt, sessions: Mutex::new(HashMap::new()) });

    let app = Router::new()
        .route("/", axum::routing::get(|| async { Html(include_str!("../web/index.html")) }))
        // TelemetryDeck SDK:把 baseURL 设为 https://host/k/<key>/
        .route("/k/{key}/v2/", post(ingest::td_key))
        .route("/k/{key}/v2/namespace/{ns}/", post(ingest::td_key_ns))
        // OTLP/HTTP:key 放 Authorization: Bearer / x-api-key,或路径 /k/<key>/v1/logs
        .route("/v1/logs", post(ingest::otlp_logs))
        .route("/k/{key}/v1/logs", post(ingest::otlp_logs))
        .route("/v1/metrics", post(ingest::otlp_ignore))
        .route("/v1/traces", post(ingest::otlp_ignore))
        .merge(admin::router(state.clone()))
        .layer(DefaultBodyLimit::max(2 << 20))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| panic!("{}: {e}", i18n::tr("failed to bind the listen address", "监听失败")));
    i18n::log_tr!(info, "listening on http://{listen}", "监听 http://{listen}");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown())
        .await
        .unwrap();

    // 丢掉发送端,等 writer 把队列写完
    drop(state);
    let _ = writer.join();
}

/// 环境变量 TL_ADMIN_PASSWORD 优先(每次启动都生效);否则首次启动随机生成并打印一次。
fn setup_admin_password(conn: &Connection) {
    if let Some(pw) = env("TL_ADMIN_PASSWORD") {
        db::meta_set(conn, "admin_hash", &admin::hash_pw(&pw)).unwrap();
    } else if db::meta_get(conn, "admin_hash").is_none() {
        let pw = hex::encode(rand::random::<[u8; 8]>());
        db::meta_set(conn, "admin_hash", &admin::hash_pw(&pw)).unwrap();
        i18n::log_tr!(warn, "generated admin password (shown only once): {pw}", "已生成管理员密码(仅显示一次): {pw}");
    }
}

async fn retention_job(db_path: String) {
    loop {
        let p = db_path.clone();
        let r = tokio::task::spawn_blocking(move || db::purge_expired(&db::open(&p)?)).await;
        match r {
            Ok(Ok(n)) if n > 0 => i18n::log_tr!(info, "purged {n} expired events", "清理过期事件 {n} 条"),
            Ok(Err(e)) => i18n::log_tr!(warn, "purge failed: {e}", "清理失败: {e}"),
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

async fn shutdown() {
    let term = async {
        #[cfg(unix)]
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap().recv().await;
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
    };
    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term => {} }
}
