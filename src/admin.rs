//! 管理端 JSON API(/api/*):登录会话、App 与写入 key 管理、统计查询。

use crate::{AppState, db::now_ms, ingest::sha256_hex};
use argon2::{Argon2, password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash}};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, ToSql, types::ValueRef};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::HashMap, sync::Arc};

type S = State<Arc<AppState>>;
type R = Result<Json<Value>, E>;

const SESSION_MS: i64 = 30 * 86_400_000;

pub struct E(StatusCode);

impl From<rusqlite::Error> for E {
    fn from(e: rusqlite::Error) -> Self {
        tracing::error!("db: {e}");
        E(StatusCode::INTERNAL_SERVER_ERROR)
    }
}
impl IntoResponse for E {
    fn into_response(self) -> Response {
        self.0.into_response()
    }
}

pub fn hash_pw(pw: &str) -> String {
    Argon2::default().hash_password(pw.as_bytes()).expect("hash").to_string()
}

fn verify_pw(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

/// 在阻塞线程里用共享的只读/管理连接执行。
async fn with_db<T: Send + 'static>(st: &Arc<AppState>, f: impl FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static) -> Result<T, E> {
    let st = st.clone();
    tokio::task::spawn_blocking(move || f(&st.db.lock().unwrap()))
        .await
        .map_err(|_| E(StatusCode::INTERNAL_SERVER_ERROR))?
        .map_err(E::from)
}

/// 查询结果直接转成 JSON 对象数组。
fn q(conn: &Connection, sql: &str, p: &[&dyn ToSql]) -> rusqlite::Result<Vec<Value>> {
    let mut st = conn.prepare_cached(sql)?;
    let names: Vec<String> = st.column_names().map(String::from).collect();
    st.query_map(p, |r| {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            m.insert(n.clone(), match r.get_ref(i)? {
                ValueRef::Integer(v) => v.into(),
                ValueRef::Real(v) => v.into(),
                ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
                _ => Value::Null,
            });
        }
        Ok(Value::Object(m))
    })?
    .collect()
}

pub fn load_keys(conn: &Connection) -> rusqlite::Result<HashMap<String, String>> {
    let mut st = conn.prepare("SELECT key_hash, app_id FROM api_key WHERE revoked_at IS NULL")?;
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
}

async fn refresh_keys(st: &Arc<AppState>) -> Result<(), E> {
    let keys = with_db(st, |c| load_keys(c)).await?;
    *st.keys.write().unwrap() = keys;
    Ok(())
}

pub fn router(st: Arc<AppState>) -> Router<Arc<AppState>> {
    let authed = Router::new()
        .route("/api/me", get(|| async { Json(json!({})) }))
        .route("/api/apps", get(apps_list).post(app_create))
        .route("/api/apps/{id}", get(app_get).patch(app_update).delete(app_delete))
        .route("/api/apps/{id}/keys", get(keys_list).post(key_create))
        .route("/api/keys/{id}", delete(key_revoke))
        .route("/api/apps/{id}/summary", get(summary))
        .route("/api/apps/{id}/stats", get(stats))
        .route("/api/apps/{id}/event-types", get(event_types))
        .route("/api/apps/{id}/users", get(users))
        .route("/api/apps/{id}/retention", get(retention))
        .route("/api/apps/{id}/versions", get(versions))
        .route("/api/apps/{id}/errors", get(errors))
        .route("/api/apps/{id}/events", get(events))
        .layer(middleware::from_fn_with_state(st, require_auth));
    Router::new().route("/api/login", post(login)).route("/api/logout", post(logout)).merge(authed)
}

// ---------- 会话 ----------

fn cookie_token(h: &HeaderMap) -> Option<String> {
    let c = h.get(header::COOKIE)?.to_str().ok()?;
    c.split(';').find_map(|p| p.trim().strip_prefix("tl_session=")).map(String::from)
}

async fn require_auth(State(st): S, req: Request, next: Next) -> Response {
    let ok = cookie_token(req.headers())
        .is_some_and(|t| st.sessions.lock().unwrap().get(&sha256_hex(&t)).is_some_and(|exp| *exp > now_ms()));
    if ok { next.run(req).await } else { StatusCode::UNAUTHORIZED.into_response() }
}

#[derive(Deserialize)]
struct Login {
    password: String,
}

async fn login(State(st): S, h: HeaderMap, Json(b): Json<Login>) -> Response {
    let hash = with_db(&st, |c| Ok(crate::db::meta_get(c, "admin_hash").unwrap_or_default())).await.unwrap_or_default();
    let ok = tokio::task::spawn_blocking(move || verify_pw(&b.password, &hash)).await.unwrap_or(false);
    if !ok {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let token = hex::encode(rand::random::<[u8; 32]>());
    {
        let mut s = st.sessions.lock().unwrap();
        let now = now_ms();
        s.retain(|_, exp| *exp > now);
        s.insert(sha256_hex(&token), now + SESSION_MS);
    }
    let secure = if h.get("x-forwarded-proto").is_some_and(|v| v == "https") { "; Secure" } else { "" };
    let cookie = format!("tl_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{secure}", SESSION_MS / 1000);
    ([(header::SET_COOKIE, cookie)], Json(json!({}))).into_response()
}

async fn logout(State(st): S, h: HeaderMap) -> Response {
    if let Some(t) = cookie_token(&h) {
        st.sessions.lock().unwrap().remove(&sha256_hex(&t));
    }
    ([(header::SET_COOKIE, "tl_session=; Max-Age=0; Path=/")], Json(json!({}))).into_response()
}

// ---------- App / Key ----------

async fn apps_list(State(st): S) -> R {
    let since = now_ms() - 86_400_000;
    let rows = with_db(&st, move |c| q(c,
        "SELECT id, name, created_at, retention_days,
           (SELECT COUNT(*) FROM signal WHERE app_id = app.id AND ts >= ?) AS events_24h
         FROM app ORDER BY created_at", &[&since])).await?;
    Ok(Json(Value::Array(rows)))
}

async fn app_get(State(st): S, Path(id): Path<String>) -> R {
    let rows = with_db(&st, move |c| q(c, "SELECT id, name, created_at, retention_days FROM app WHERE id=?", &[&id])).await?;
    rows.into_iter().next().map(Json).ok_or(E(StatusCode::NOT_FOUND))
}

#[derive(Deserialize)]
struct AppBody {
    name: Option<String>,
    retention_days: Option<i64>,
}

async fn app_create(State(st): S, Json(b): Json<AppBody>) -> R {
    let name = b.name.filter(|n| !n.trim().is_empty()).ok_or(E(StatusCode::BAD_REQUEST))?;
    let id = uuid::Uuid::new_v4().to_string();
    let days = b.retention_days.unwrap_or(90).clamp(1, 3650);
    let id2 = id.clone();
    with_db(&st, move |c| c.execute("INSERT INTO app(id,name,created_at,retention_days) VALUES(?,?,?,?)", rusqlite::params![id2, name, now_ms(), days])).await?;
    Ok(Json(json!({ "id": id })))
}

async fn app_update(State(st): S, Path(id): Path<String>, Json(b): Json<AppBody>) -> R {
    with_db(&st, move |c| {
        if let Some(n) = b.name.filter(|n| !n.trim().is_empty()) {
            c.execute("UPDATE app SET name=? WHERE id=?", [&n, &id])?;
        }
        if let Some(d) = b.retention_days {
            c.execute("UPDATE app SET retention_days=? WHERE id=?", rusqlite::params![d.clamp(1, 3650), id])?;
        }
        Ok(())
    }).await?;
    Ok(Json(json!({})))
}

async fn app_delete(State(st): S, Path(id): Path<String>) -> R {
    with_db(&st, move |c| {
        c.execute("DELETE FROM signal WHERE app_id=?", [&id])?;
        c.execute("DELETE FROM app WHERE id=?", [&id])
    }).await?;
    refresh_keys(&st).await?;
    Ok(Json(json!({})))
}

async fn keys_list(State(st): S, Path(id): Path<String>) -> R {
    let rows = with_db(&st, move |c| q(c,
        "SELECT id, label, prefix, created_at, revoked_at FROM api_key WHERE app_id=? ORDER BY created_at DESC", &[&id])).await?;
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize)]
struct KeyBody {
    label: Option<String>,
}

async fn key_create(State(st): S, Path(app_id): Path<String>, Json(b): Json<KeyBody>) -> R {
    let key = format!("tl_{}", URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()));
    let (id, hash, prefix) = (uuid::Uuid::new_v4().to_string(), sha256_hex(&key), key[..10].to_string());
    let label = b.label.unwrap_or_default();
    with_db(&st, move |c| c.execute(
        "INSERT INTO api_key(id,app_id,label,prefix,key_hash,created_at) VALUES(?,?,?,?,?,?)",
        rusqlite::params![id, app_id, label, prefix, hash, now_ms()])).await?;
    refresh_keys(&st).await?;
    Ok(Json(json!({ "key": key })))
}

async fn key_revoke(State(st): S, Path(id): Path<String>) -> R {
    with_db(&st, move |c| c.execute("UPDATE api_key SET revoked_at=? WHERE id=? AND revoked_at IS NULL", rusqlite::params![now_ms(), id])).await?;
    refresh_keys(&st).await?;
    Ok(Json(json!({})))
}

// ---------- 查询 ----------

/// 维度名 → SQL 表达式(白名单,绝不拼接用户输入)。
const DIMS: [(&str, &str); 6] = [
    ("app_version", "app_version"),
    // SDK 的 systemVersion 已形如 "iOS 18.2.0";只有纯数字版本才补上系统名
    ("os_version", "CASE WHEN coalesce(os_version,'') = '' THEN os WHEN os_version GLOB '[0-9]*' THEN trim(coalesce(os,'') || ' ' || os_version) ELSE os_version END"),
    ("device_model", "device_model"),
    ("locale", "locale"),
    ("country", "country"),
    // 旧数据里的城市名可能带区县后缀("San Francisco (South Beach)"),分组时一并去掉
    ("city", "CASE WHEN instr(city, ' (') > 1 THEN substr(city, 1, instr(city, ' (') - 1) ELSE city END"),
];

/// SDK 预设错误事件的类型名。
const ERR_TYPE: &str = "TelemetryDeck.Error.occurred";

/// 统一解析查询窗口:`days`(时间范围)、`test`(0 正式 / 1 测试)、`tz`(浏览器相对 UTC 的分钟数,图表按本地时区分桶)。
struct Win {
    since: i64,
    test: i64,
    fmt: &'static str,
    tz_sec: i64,
}

fn win(p: &HashMap<String, String>) -> Win {
    let days: i64 = p.get("days").and_then(|d| d.parse().ok()).unwrap_or(7).clamp(1, 3650);
    Win {
        since: now_ms() - days * 86_400_000,
        test: i64::from(p.get("test").is_some_and(|t| t == "1")),
        fmt: if days <= 1 { "%Y-%m-%d %H:00" } else { "%Y-%m-%d" },
        tz_sec: p.get("tz").and_then(|t| t.parse::<i64>().ok()).unwrap_or(0).clamp(-840, 840) * 60,
    }
}

async fn stats(State(st): S, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> R {
    let Win { since, test, fmt, tz_sec } = win(&p);
    let (hour_ago, other) = (now_ms() - 3_600_000, 1 - test);
    let out = with_db(&st, move |c| {
        let w = "FROM signal WHERE app_id=?1 AND ts>=?2 AND is_test=?3";
        let args: &[&dyn ToSql] = &[&id, &since, &test];
        let totals = q(c, &format!("SELECT COUNT(*) events, COUNT(DISTINCT user_hash) users, COUNT(DISTINCT session_id) sessions {w}"), args)?;
        let series = q(c, &format!("SELECT strftime(?4, ts/1000 + ?5, 'unixepoch') t, COUNT(*) events, COUNT(DISTINCT user_hash) users {w} GROUP BY t ORDER BY t"),
            &[&id, &since, &test, &fmt, &tz_sec])?;
        let top = q(c, &format!("SELECT type, COUNT(*) events, COUNT(DISTINCT user_hash) users {w} GROUP BY type ORDER BY events DESC LIMIT 20"), args)?;
        let mut dims = Map::new();
        for (name, expr) in DIMS {
            let rows = q(c, &format!("SELECT COALESCE(NULLIF({expr},''),'') v, COUNT(DISTINCT user_hash) users, COUNT(*) events {w} GROUP BY v ORDER BY users DESC, events DESC LIMIT 8"), args)?;
            dims.insert(name.into(), Value::Array(rows));
        }
        // 最近 1 小时的用户数(接入调试用);另一种数据类型(正式/测试)里有多少事件,用于提示
        let last_hour = q(c, &format!("SELECT COUNT(DISTINCT user_hash) n {w}"), &[&id, &hour_ago, &test])?;
        let hidden = q(c, &format!("SELECT COUNT(*) n {w}"), &[&id, &since, &other])?;
        Ok(json!({ "totals": totals.first(), "series": series, "top": top, "dims": dims,
                   "last_hour_users": last_hour[0]["n"], "other_events": hidden[0]["n"] }))
    }).await?;
    Ok(Json(out))
}

/// 页头的三个数字:用户数、最常用的 SDK、错误数(最近 90 天正式数据)。
async fn summary(State(st): S, Path(id): Path<String>) -> R {
    let since = now_ms() - 90 * 86_400_000;
    let out = with_db(&st, move |c| {
        let w = "FROM signal WHERE app_id=?1 AND ts>=?2 AND is_test=0";
        let users = q(c, &format!("SELECT COUNT(DISTINCT user_hash) n {w}"), &[&id, &since])?;
        let sdk = q(c, &format!("SELECT sdk {w} AND sdk IS NOT NULL AND sdk <> '' GROUP BY sdk ORDER BY COUNT(*) DESC LIMIT 1"), &[&id, &since])?;
        let errs = q(c, &format!("SELECT COUNT(*) n {w} AND type=?3"), &[&id, &since, &ERR_TYPE])?;
        Ok(json!({ "users": users[0]["n"], "sdk": sdk.first().map(|r| r["sdk"].clone()), "errors": errs[0]["n"] }))
    }).await?;
    Ok(Json(out))
}

async fn event_types(State(st): S, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> R {
    let Win { since, test, .. } = win(&p);
    let rows = with_db(&st, move |c| q(c,
        "SELECT type, COUNT(DISTINCT user_hash) users, COUNT(*) events, MAX(ts) last_ts
         FROM signal WHERE app_id=?1 AND ts>=?2 AND is_test=?3 GROUP BY type ORDER BY events DESC LIMIT 500",
        &[&id, &since, &test])).await?;
    Ok(Json(Value::Array(rows)))
}

/// 新用户(按用户在已存储数据里的首次出现时间)与活跃用户。
async fn users(State(st): S, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> R {
    let Win { since, test, fmt, tz_sec } = win(&p);
    let out = with_db(&st, move |c| {
        let new = q(c, "SELECT strftime(?4, f/1000 + ?5, 'unixepoch') t, COUNT(*) users FROM
                (SELECT MIN(ts) f FROM signal WHERE app_id=?1 AND is_test=?3 AND user_hash IS NOT NULL GROUP BY user_hash)
              WHERE f >= ?2 GROUP BY t ORDER BY t", &[&id, &since, &test, &fmt, &tz_sec])?;
        let active = q(c, "SELECT strftime(?4, ts/1000 + ?5, 'unixepoch') t, COUNT(DISTINCT user_hash) users
              FROM signal WHERE app_id=?1 AND ts>=?2 AND is_test=?3 GROUP BY t ORDER BY t", &[&id, &since, &test, &fmt, &tz_sec])?;
        Ok(json!({ "new": new, "active": active }))
    }).await?;
    Ok(Json(out))
}

/// 按周的留存队列:最近 8 个周,每个队列在之后各周仍活跃的用户数。周从周一起算(按浏览器时区)。
async fn retention(State(st): S, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> R {
    const WEEK: i64 = 7 * 86_400_000;
    const MON: i64 = 3 * 86_400_000; // unix 纪元是周四,偏移 3 天后按周一分周
    let Win { test, tz_sec, .. } = win(&p);
    let tz = tz_sec * 1000;
    let cur = (now_ms() + tz + MON) / WEEK;
    let first_cohort = cur - 7;
    let active_since = now_ms() - 9 * WEEK;
    let rows = with_db(&st, move |c| q(c,
        "WITH f AS (SELECT user_hash, MIN(ts) first_ts FROM signal WHERE app_id=?1 AND is_test=?2 AND user_hash IS NOT NULL GROUP BY user_hash),
              a AS (SELECT DISTINCT user_hash, (ts + ?3 + ?6) / ?7 AS wk FROM signal
                    WHERE app_id=?1 AND is_test=?2 AND user_hash IS NOT NULL AND ts >= ?4),
              j AS (SELECT (f.first_ts + ?3 + ?6) / ?7 AS cohort, a.wk AS wk FROM f JOIN a ON a.user_hash = f.user_hash)
         SELECT cohort, wk - cohort AS n, COUNT(*) users FROM j
         WHERE cohort >= ?5 AND wk >= cohort GROUP BY cohort, n ORDER BY cohort, n",
        &[&id, &test, &tz, &active_since, &first_cohort, &MON, &WEEK])).await?;
    Ok(Json(json!({ "rows": rows, "week_ms": WEEK, "mon_ms": MON })))
}

/// 各 App 版本的用户数随时间变化(客户端取前 8 个版本画堆叠图)。
async fn versions(State(st): S, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> R {
    let Win { since, test, fmt, tz_sec } = win(&p);
    let rows = with_db(&st, move |c| q(c,
        "SELECT strftime(?4, ts/1000 + ?5, 'unixepoch') t, COALESCE(NULLIF(app_version,''),'') v, COUNT(DISTINCT user_hash) users
         FROM signal WHERE app_id=?1 AND ts>=?2 AND is_test=?3 GROUP BY t, v ORDER BY t",
        &[&id, &since, &test, &fmt, &tz_sec])).await?;
    Ok(Json(Value::Array(rows)))
}

/// SDK 预设错误事件(`TelemetryDeck.Error.occurred`):趋势、按错误 id 汇总。
async fn errors(State(st): S, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> R {
    let Win { since, test, fmt, tz_sec } = win(&p);
    let out = with_db(&st, move |c| {
        let w = "FROM signal WHERE app_id=?1 AND ts>=?2 AND is_test=?3 AND type=?4";
        let series = q(c, &format!("SELECT strftime(?5, ts/1000 + ?6, 'unixepoch') t, COUNT(*) events, COUNT(DISTINCT user_hash) users {w} GROUP BY t ORDER BY t"),
            &[&id, &since, &test, &ERR_TYPE, &fmt, &tz_sec])?;
        let top = q(c, &format!("SELECT json_extract(payload,'$.\"TelemetryDeck.Error.id\"') err_id,
                json_extract(payload,'$.\"TelemetryDeck.Error.category\"') err_category,
                MAX(json_extract(payload,'$.\"TelemetryDeck.Error.message\"')) err_message,
                COUNT(*) events, COUNT(DISTINCT user_hash) users, MAX(ts) last_ts
             {w} GROUP BY err_id, err_category ORDER BY events DESC LIMIT 100"), &[&id, &since, &test, &ERR_TYPE])?;
        Ok(json!({ "series": series, "top": top }))
    }).await?;
    Ok(Json(out))
}

async fn events(State(st): S, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> R {
    let kind = p.get("type").filter(|t| !t.is_empty()).cloned();
    let before = p.get("before").and_then(|b| b.parse::<i64>().ok());
    let limit: i64 = p.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100).clamp(1, 500);
    let rows = with_db(&st, move |c| q(c,
        "SELECT id, ts, type, user_hash, session_id, float_value, is_test, app_version, os, os_version,
                device_model, locale, country, city, payload
         FROM signal WHERE app_id=?1 AND (?2 IS NULL OR type=?2) AND (?3 IS NULL OR id<?3)
         ORDER BY id DESC LIMIT ?4", &[&id, &kind, &before, &limit])).await?;
    Ok(Json(Value::Array(rows)))
}
