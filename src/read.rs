//! Read API for analysis tools and AI agents (`/r/v1/*`).
//!
//! Authenticated with a per-app read key (`tlr_…`) in `Authorization: Bearer`. Read-only JSON; every
//! error carries a machine-readable `error` code and a `hint` that says how to fix the request.

use crate::{AppState, admin::{DIMS, ERR_TYPE, q, retention_query}, db::now_ms, ingest::sha256_hex};
use axum::{
    Extension, Json, Router,
    extract::{Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use rusqlite::{Connection, ToSql};
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};

type S = State<Arc<AppState>>;
type RR = Result<Response, ApiErr>;

/// Requests per read key per minute, and the time a single query may run.
const RATE_PER_MIN: u32 = 120;
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const DAY: i64 = 86_400_000;

const COMMON: [&str; 7] = ["days", "since", "until", "test", "platform", "tz", "format"];
const EVENTS_PARAMS: [&str; 4] = ["type", "where", "limit", "cursor"];
const QUERY_PARAMS: [&str; 7] = ["type", "where", "metric", "group_by", "interval", "order", "limit"];
const FIELDS_HINT: &str = "type, platform, app_version, os_version, device_model, locale, country, city, build, sdk, payload.<key>";

// ---------- errors ----------

#[derive(Debug)]
pub struct ApiErr {
    status: StatusCode,
    code: &'static str,
    message: String,
    hint: String,
}

impl ApiErr {
    fn bad(code: &'static str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, code, message: message.into(), hint: hint.into() }
    }
}

impl From<rusqlite::Error> for ApiErr {
    fn from(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(f, _) = &e {
            if f.code == rusqlite::ErrorCode::OperationInterrupted {
                return Self {
                    status: StatusCode::GATEWAY_TIMEOUT, code: "query_timeout",
                    message: format!("the query ran longer than {}s and was cancelled", QUERY_TIMEOUT.as_secs()),
                    hint: "narrow the time window (days/since/until), add a platform or type filter, or group by fewer fields".into(),
                };
            }
        }
        tracing::error!("read api db: {e}");
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, code: "internal", message: "internal error".into(), hint: "retry; if it persists the server log has details".into() }
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.code, "message": self.message, "hint": self.hint }))).into_response()
    }
}

// ---------- time ----------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Unix ms → `2026-11-09T08:30:00Z`.
fn iso(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(DAY), ms.rem_euclid(DAY) / 1000);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Accepts unix milliseconds, `YYYY-MM-DD` (UTC midnight) or RFC 3339.
fn parse_time(name: &str, s: &str) -> Result<i64, ApiErr> {
    let bad = || ApiErr::bad("bad_time", format!("cannot parse {name}={s:?}"), "use RFC 3339 (2026-11-01T00:00:00Z), a date (2026-11-01) or unix milliseconds");
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse().map_err(|_| bad());
    }
    if let [y, m, d] = s.split('-').collect::<Vec<_>>()[..] {
        if s.len() == 10 {
            if let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i64>(), m.parse::<i64>(), d.parse::<i64>()) {
                if (1..=12).contains(&m) && (1..=31).contains(&d) {
                    return Ok(days_from_civil(y, m, d) * DAY);
                }
            }
            return Err(bad());
        }
    }
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
        .map_err(|_| bad())
}

// ---------- parameters ----------

struct P(Vec<(String, String)>);

impl P {
    fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().find(|(n, v)| n == k && !v.is_empty()).map(|(_, v)| v.as_str())
    }
    fn all(&self, k: &str) -> Vec<&str> {
        self.0.iter().filter(|(n, v)| n == k && !v.is_empty()).map(|(_, v)| v.as_str()).collect()
    }
    /// Reject unknown parameters: a silently ignored typo would make an agent analyse the wrong thing.
    fn check(&self, allowed: &[&[&str]]) -> Result<(), ApiErr> {
        let ok: Vec<&str> = allowed.iter().flat_map(|a| a.iter().copied()).collect();
        match self.0.iter().find(|(n, _)| !ok.contains(&n.as_str())) {
            None => Ok(()),
            Some((n, _)) => Err(ApiErr::bad("unknown_parameter", format!("unknown parameter {n:?}"), format!("valid parameters here: {}", ok.join(", ")))),
        }
    }
    fn int(&self, k: &str, default: i64, min: i64, max: i64) -> Result<i64, ApiErr> {
        match self.get(k) {
            None => Ok(default),
            Some(v) => v.parse::<i64>().ok().filter(|n| (min..=max).contains(n))
                .ok_or_else(|| ApiErr::bad("bad_parameter", format!("{k}={v:?} is not an integer in {min}..{max}"), format!("use {k}=<integer between {min} and {max}>"))),
        }
    }
}

struct W {
    since: i64,
    until: i64,
    /// 0 live, 1 test, 2 both
    test: i64,
    os: Option<String>,
    tz_min: i64,
}

fn window(p: &P) -> Result<W, ApiErr> {
    let now = now_ms();
    let until = p.get("until").map(|v| parse_time("until", v)).transpose()?.unwrap_or(now);
    let since = match p.get("since") {
        Some(v) => parse_time("since", v)?,
        None => until - p.int("days", 7, 1, 3650)? * DAY,
    };
    if since >= until {
        return Err(ApiErr::bad("bad_window", "since must be earlier than until", "e.g. days=30, or since=2026-10-01&until=2026-11-01"));
    }
    let test = match p.get("test") {
        None | Some("0") | Some("live") => 0,
        Some("1") | Some("test") => 1,
        Some("all") => 2,
        Some(v) => return Err(ApiErr::bad("bad_parameter", format!("test={v:?}"), "use test=0 (live data, default), test=1 (test-mode data) or test=all")),
    };
    let os = p.get("platform").map(str::trim).filter(|s| !s.is_empty());
    if os.is_some_and(|s| s.chars().count() > 50) {
        return Err(ApiErr::bad("bad_parameter", "platform is too long", "use a name from /r/v1/ → platforms, e.g. platform=iOS"));
    }
    Ok(W { since, until, test, os: os.map(String::from), tz_min: p.int("tz", 0, -840, 840)? })
}

/// Collects positional SQL parameters; the first five are shared by every query (see `BASE`).
struct Qb {
    params: Vec<Box<dyn ToSql + Send>>,
}

impl Qb {
    fn new(app: &str, w: &W, since: i64, until: i64) -> Self {
        let mut qb = Qb { params: vec![] };
        qb.push(app.to_string());
        qb.push(since);
        qb.push(until);
        qb.push(w.test);
        qb.push(w.os.clone());
        qb
    }
    fn push(&mut self, v: impl ToSql + Send + 'static) -> usize {
        self.params.push(Box::new(v));
        self.params.len()
    }
    fn refs(&self) -> Vec<&dyn ToSql> {
        self.params.iter().map(|b| b.as_ref() as &dyn ToSql).collect()
    }
    fn run(&self, c: &Connection, sql: &str) -> rusqlite::Result<Vec<Value>> {
        q(c, sql, &self.refs())
    }
}

const BASE: &str = "app_id=?1 AND ts>=?2 AND ts<?3 AND (?4=2 OR is_test=?4) AND (?5 IS NULL OR os = ?5 COLLATE NOCASE)";

/// SQL expression for a field name from the whitelist; `payload.<key>` binds the JSON path as a parameter.
fn field(name: &str, qb: &mut Qb) -> Result<String, ApiErr> {
    if let Some(k) = name.strip_prefix("payload.") {
        if k.is_empty() || k.chars().count() > 100 || k.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
            return Err(ApiErr::bad("bad_field", format!("invalid payload key in {name:?}"), "use payload.<key> with the exact parameter name listed under custom_parameters in /r/v1/"));
        }
        let n = qb.push(format!("$.\"{k}\""));
        return Ok(format!("CAST(json_extract(payload, ?{n}) AS TEXT)"));
    }
    Ok(match name {
        "type" => "type".to_string(),
        "platform" | "os" => "os".to_string(),
        "build" => "build".to_string(),
        "sdk" => "sdk".to_string(),
        other => DIMS.iter().find(|(n, _)| *n == other).map(|(_, e)| e.to_string())
            .ok_or_else(|| ApiErr::bad("unknown_field", format!("unknown field {other:?}"), format!("valid fields: {FIELDS_HINT}")))?,
    })
}

/// `type=` and repeated `where=field:value` → extra AND conditions.
fn filters(p: &P, qb: &mut Qb) -> Result<String, ApiErr> {
    let mut sql = String::new();
    if let Some(t) = p.get("type") {
        let n = qb.push(t.to_string());
        sql += &format!(" AND type = ?{n}");
    }
    for w in p.all("where") {
        let (f, v) = w.split_once(':').ok_or_else(|| ApiErr::bad("bad_where", format!("where={w:?}"), "use where=<field>:<value>, e.g. where=country:DE or where=payload.plan:pro (repeat where= to AND several)"))?;
        let e = field(f.trim(), qb)?;
        let n = qb.push(v.to_string());
        sql += &format!(" AND {e} = ?{n}");
    }
    Ok(sql)
}

fn meta(app: &str, w: &W) -> Value {
    let data = ["live", "test", "all"][w.test as usize];
    json!({
        "app_id": app,
        "window": { "since": iso(w.since), "until": iso(w.until), "since_ms": w.since, "until_ms": w.until },
        "data": data,
        "platform": w.os,
        "tz_minutes": w.tz_min,
        "generated_at": iso(now_ms()),
    })
}

// ---------- auth, execution ----------

#[derive(Clone)]
struct Ctx {
    app_id: String,
}

fn err(status: StatusCode, code: &'static str, message: &str, hint: &str) -> Response {
    ApiErr { status, code, message: message.into(), hint: hint.into() }.into_response()
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| h.get("x-api-key").and_then(|v| v.to_str().ok()))
}

async fn auth(State(st): S, mut req: Request, next: Next) -> Response {
    let Some(key) = bearer(req.headers()) else {
        return err(StatusCode::UNAUTHORIZED, "missing_key", "no read key was sent", "send the header  Authorization: Bearer tlr_…  (issue a read key in the admin UI → app → Settings)");
    };
    let hash = sha256_hex(key);
    let info = st.keys.read().unwrap().get(&hash).cloned();
    let info = match info {
        None => return err(StatusCode::UNAUTHORIZED, "invalid_key", "this key is unknown or revoked", "check the key, or issue a new read key in the admin UI"),
        Some(i) if !i.read => return err(StatusCode::FORBIDDEN, "wrong_key_type", "this is a write key (tl_…); it can only report events", "use a read key (tlr_…) for /r/v1/*"),
        Some(i) => i,
    };
    let now = now_ms();
    let wait = {
        let mut m = st.read_rate.lock().unwrap();
        let e = m.entry(hash).or_insert((now, 0));
        if now - e.0 >= 60_000 { *e = (now, 0); }
        e.1 += 1;
        (e.1 > RATE_PER_MIN).then(|| (60_000 - (now - e.0)).max(1000) / 1000)
    };
    if let Some(secs) = wait {
        let mut r = err(StatusCode::TOO_MANY_REQUESTS, "rate_limited", &format!("more than {RATE_PER_MIN} requests per minute for this key"), "slow down, or fetch /r/v1/overview once instead of many small calls");
        r.headers_mut().insert(header::RETRY_AFTER, secs.to_string().parse().unwrap());
        return r;
    }
    req.extensions_mut().insert(Ctx { app_id: info.app_id });
    next.run(req).await
}

/// Runs `f` on its own read-only connection: at most 4 queries at once, cancelled after `QUERY_TIMEOUT`.
async fn run<T: Send + 'static>(st: &Arc<AppState>, f: impl FnOnce(&Connection) -> Result<T, ApiErr> + Send + 'static) -> Result<T, ApiErr> {
    let _slot = st.read_slots.acquire().await.map_err(|_| ApiErr::from(rusqlite::Error::InvalidQuery))?;
    let conn = crate::db::open(&st.db_path)?;
    conn.execute_batch("PRAGMA query_only=ON")?;
    let handle = conn.get_interrupt_handle();
    let timer = tokio::spawn(async move { tokio::time::sleep(QUERY_TIMEOUT).await; handle.interrupt(); });
    let out = tokio::task::spawn_blocking(move || f(&conn)).await;
    timer.abort();
    out.map_err(|_| ApiErr::from(rusqlite::Error::InvalidQuery))?
}

// ---------- output ----------

fn without_nulls(mut m: Map<String, Value>) -> Map<String, Value> {
    m.retain(|_, v| !v.is_null());
    m
}

/// json (default) / csv / md. csv and md render `body[rows_key]` as a table, which costs far fewer tokens than JSON.
fn respond(p: &P, body: Value, rows_key: Option<&str>) -> RR {
    match (p.get("format").unwrap_or("json"), rows_key) {
        ("json", _) => Ok(Json(body).into_response()),
        (f @ ("csv" | "md"), Some(k)) => {
            let rows = body[k].as_array().cloned().unwrap_or_default();
            let mut cols: Vec<String> = vec![];
            for r in &rows {
                for c in r.as_object().into_iter().flat_map(|o| o.keys()) {
                    if !cols.contains(c) { cols.push(c.clone()); }
                }
            }
            let cell = |v: Option<&Value>| match v {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(o) => o.to_string(),
            };
            let line = |cells: Vec<String>| -> String {
                if f == "csv" {
                    cells.iter().map(|c| if c.contains([',', '"', '\n']) { format!("\"{}\"", c.replace('"', "\"\"")) } else { c.clone() }).collect::<Vec<_>>().join(",")
                } else {
                    format!("| {} |", cells.iter().map(|c| c.replace('|', "\\|").replace('\n', " ")).collect::<Vec<_>>().join(" | "))
                }
            };
            let mut out = vec![line(cols.clone())];
            if f == "md" { out.push(format!("|{}", " --- |".repeat(cols.len()))); }
            for r in &rows { out.push(line(cols.iter().map(|c| cell(r.get(c))).collect())); }
            let mut resp = (
                [(header::CONTENT_TYPE, if f == "csv" { "text/csv; charset=utf-8" } else { "text/markdown; charset=utf-8" })],
                out.join("\n") + "\n",
            ).into_response();
            let h = resp.headers_mut();
            h.insert("x-tl-truncated", body["meta"]["truncated"].as_bool().unwrap_or(false).to_string().parse().unwrap());
            if let Some(c) = body["next_cursor"].as_i64() { h.insert("x-tl-next-cursor", c.to_string().parse().unwrap()); }
            Ok(resp)
        }
        (f, None) if f == "csv" || f == "md" => Err(ApiErr::bad("bad_format", format!("format={f} is not available here"), "only /r/v1/events and /r/v1/query support csv and md; use format=json")),
        (f, _) => Err(ApiErr::bad("bad_format", format!("format={f:?}"), "use format=json (default), csv or md")),
    }
}

// ---------- routes ----------

pub fn router(st: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/r/v1", get(describe))
        .route("/r/v1/", get(describe))
        .route("/r/v1/overview", get(overview))
        .route("/r/v1/events", get(events))
        .route("/r/v1/query", get(query))
        .layer(middleware::from_fn_with_state(st, auth))
}

fn endpoints() -> Value {
    let common = json!({
        "days": "time window ending now (or at `until`), default 7, max 3650",
        "since": "window start: RFC 3339, YYYY-MM-DD (UTC) or unix ms; overrides days",
        "until": "window end, same formats, default now",
        "test": "0 = live data (default), 1 = test-mode data, all = both",
        "platform": "only this platform (the os column, e.g. iOS or Android; case-insensitive); see `platforms`",
        "tz": "minutes east of UTC used for day/hour/week buckets, default 0 (e.g. 480 for UTC+8)",
    });
    let with = |extra: Value| { let mut m = common.as_object().unwrap().clone(); m.extend(extra.as_object().unwrap().clone()); Value::Object(m) };
    json!([
        { "path": "/r/v1/", "description": "This document: the app, its data range, platforms, event types, custom parameters and the endpoints. Call it first.", "params": {} },
        { "path": "/r/v1/overview", "description": "Everything for a first look in ONE call: users/events/sessions with change versus the previous period, trend, top events, breakdowns, top errors and weekly retention.",
          "params": common, "example": "/r/v1/overview?days=30&platform=iOS" },
        { "path": "/r/v1/events", "description": "Raw events, newest first. Custom parameters are under `params`. Pages by `cursor`.",
          "params": with(json!({ "type": "exact event name", "where": "field:value, repeatable (AND)", "limit": "default 100, max 500", "cursor": "next_cursor from the previous page", "format": "json (default), csv or md" })),
          "example": "/r/v1/events?type=Purchase.completed&where=payload.plan:yearly&limit=20" },
        { "path": "/r/v1/query", "description": "Aggregate any metric by any fields and an optional time interval. This is the tool for questions the overview does not answer.",
          "params": with(json!({
              "metric": "events, users (distinct anonymous users) or sessions; comma-separate for several; default events",
              "group_by": format!("up to 3 comma-separated fields: {FIELDS_HINT}"),
              "interval": "none (default), hour, day, week (starts Monday) or month; adds a `t` column",
              "type": "exact event name", "where": "field:value, repeatable (AND)",
              "order": "value (first metric, descending; default without interval) or key (by t/group; default with interval)",
              "limit": "default 100, max 1000", "format": "json (default), csv or md" })),
          "example": "/r/v1/query?type=Purchase.completed&metric=events,users&group_by=payload.plan,country&interval=week&days=90" },
    ])
}

async fn describe(State(st): S, Extension(ctx): Extension<Ctx>, Query(raw): Query<Vec<(String, String)>>) -> RR {
    P(raw).check(&[])?;
    let (since30, app) = (now_ms() - 30 * DAY, ctx.app_id);
    let a2 = app.clone();
    let out = run(&st, move |c| {
        let info = q(c, "SELECT name, retention_days FROM app WHERE id=?1", &[&a2])?;
        let first = q(c, "SELECT MIN(ts) t FROM signal WHERE app_id=?1", &[&a2])?;
        let last = q(c, "SELECT MAX(ts) t FROM signal WHERE app_id=?1", &[&a2])?;
        let counts = q(c, "SELECT COALESCE(SUM(is_test=0),0) live, COALESCE(SUM(is_test=1),0) test FROM signal WHERE app_id=?1 AND ts>=?2", &[&a2, &since30])?;
        let platforms = q(c, "SELECT os platform, COUNT(DISTINCT user_hash) users, COUNT(*) events FROM signal WHERE app_id=?1 AND ts>=?2 AND is_test=0 AND os IS NOT NULL AND os<>'' GROUP BY os ORDER BY users DESC LIMIT 12", &[&a2, &since30])?;
        let mut types = q(c, "SELECT type, COUNT(*) events, COUNT(DISTINCT user_hash) users, MAX(ts) last_ts FROM signal WHERE app_id=?1 AND ts>=?2 AND is_test=0 GROUP BY type ORDER BY events DESC LIMIT 200", &[&a2, &since30])?;
        for t in &mut types {
            let ts = t["last_ts"].as_i64().unwrap_or(0);
            t.as_object_mut().unwrap().remove("last_ts");
            t["last_event_at"] = iso(ts).into();
        }
        // Custom parameter names, sampled from the newest live events.
        let params = q(c, "SELECT j.key key, COUNT(*) events FROM (SELECT payload FROM signal WHERE app_id=?1 AND is_test=0 ORDER BY id DESC LIMIT 2000), json_each(payload) j GROUP BY j.key ORDER BY events DESC LIMIT 100", &[&a2])?;
        Ok((info, first, last, counts, platforms, types, params))
    }).await?;
    let (info, first, last, counts, platforms, types, params) = out;
    let ms = |v: &[Value]| v.first().and_then(|r| r["t"].as_i64()).map(iso);
    let Some(info) = info.first() else { return Err(ApiErr::bad("app_not_found", "the app of this key no longer exists", "issue a new key")); };
    Ok(Json(json!({
        "service": "TelemetryLite",
        "version": env!("CARGO_PKG_VERSION"),
        "app": { "id": app, "name": info["name"], "retention_days": info["retention_days"] },
        "data": { "first_event_at": ms(&first), "last_event_at": ms(&last), "last_30_days": counts.first() },
        "platforms": platforms,
        "event_types": { "window_days": 30, "live_only": true, "rows": types },
        "custom_parameters": { "note": "names usable as payload.<key> in where/group_by (sampled from the newest 2000 live events)", "rows": params },
        "guide_for_agents": [
            "Start with /r/v1/overview, then drill down with /r/v1/query (aggregations) and /r/v1/events (raw events). For large tables add format=md or format=csv to save tokens.",
            "Errors are JSON with a `hint`: read it and fix the request. Only call /r/v1/*; this API is read-only, do not try to change data.",
            "Report findings with the numbers and the time window behind them. If the data is too thin to conclude anything (few users, short window, test-only data), say so instead of guessing.",
            "Keep the read key secret: do not write it into files, logs or commits.",
        ],
        "examples": { "note": "replace <…> with names from event_types / custom_parameters", "rows": [
            { "ask": "overall picture, 30 days", "path": "/r/v1/overview?days=30" },
            { "ask": "daily active users", "path": "/r/v1/query?metric=users&interval=day&days=30" },
            { "ask": "users per app version", "path": "/r/v1/query?metric=users&group_by=app_version&days=7" },
            { "ask": "users per platform and country", "path": "/r/v1/query?metric=users&group_by=platform,country&days=30" },
            { "ask": "one event over time", "path": "/r/v1/query?type=<event>&metric=events,users&interval=day&days=30" },
            { "ask": "one event split by a custom parameter", "path": "/r/v1/query?type=<event>&metric=events,users&group_by=payload.<key>" },
            { "ask": "top errors", "path": "/r/v1/query?type=TelemetryDeck.Error.occurred&group_by=payload.TelemetryDeck.Error.id&days=7" },
            { "ask": "latest raw events as a table", "path": "/r/v1/events?limit=20&format=md" },
            { "ask": "retention", "path": "/r/v1/overview → retention" },
        ] },
        "fields": FIELDS_HINT,
        "endpoints": endpoints(),
        "conventions": [
            "All timestamps are UTC (`…Z`) unless a parameter says otherwise; day/week buckets use `tz`.",
            "Live data is the default; test-mode data (debug builds) is excluded unless test=1 or test=all.",
            "Users are anonymous, salted hashes. `users` counts distinct hashes, so one person on two devices counts twice.",
            "A response with meta.truncated=true has more rows: raise limit, narrow the window, or use cursor (events).",
            "Errors are JSON: {error, message, hint}. Read `hint` and retry.",
            "Events older than the app's retention_days are deleted, so windows reaching further back return less.",
        ],
    })).into_response())
}

fn trend(cur: i64, prev: i64) -> Value {
    json!({ "value": cur, "previous": prev, "change_pct": (prev > 0).then(|| ((cur - prev) as f64 / prev as f64 * 1000.0).round() / 10.0) })
}

async fn overview(State(st): S, Extension(ctx): Extension<Ctx>, Query(raw): Query<Vec<(String, String)>>) -> RR {
    let p = P(raw);
    p.check(&[&COMMON])?;
    if p.get("format").is_some_and(|f| f != "json") { respond(&p, Value::Null, None)?; }
    let w = window(&p)?;
    let app = ctx.app_id.clone();
    let span = w.until - w.since;
    let fmt = if span <= 2 * DAY { "%Y-%m-%d %H:00" } else { "%Y-%m-%d" };
    let tz_sec = w.tz_min * 60;
    let (test, os, a2, since, until) = (w.test, w.os.clone(), app.clone(), w.since, w.until);
    let out = run(&st, move |c| {
        let totals = |s: i64, u: i64| -> rusqlite::Result<(i64, i64, i64)> {
            let qb = Qb::new(&a2, &W { since: s, until: u, test, os: os.clone(), tz_min: 0 }, s, u);
            let r = qb.run(c, &format!("SELECT COUNT(*) e, COUNT(DISTINCT user_hash) u, COUNT(DISTINCT session_id) s FROM signal WHERE {BASE}"))?;
            Ok((r[0]["e"].as_i64().unwrap_or(0), r[0]["u"].as_i64().unwrap_or(0), r[0]["s"].as_i64().unwrap_or(0)))
        };
        let cur = totals(since, until)?;
        let prev = totals(since - span, since)?;
        let qb = Qb::new(&a2, &W { since, until, test, os: os.clone(), tz_min: 0 }, since, until);
        let new_users = qb.run(c, "SELECT COUNT(*) n FROM (SELECT MIN(ts) f FROM signal WHERE app_id=?1 AND (?4=2 OR is_test=?4) AND user_hash IS NOT NULL AND (?5 IS NULL OR os = ?5 COLLATE NOCASE) GROUP BY user_hash) WHERE f>=?2 AND f<?3")?;
        let series = qb.run(c, &format!("SELECT strftime('{fmt}', ts/1000 + {tz_sec}, 'unixepoch') t, COUNT(*) events, COUNT(DISTINCT user_hash) users, COUNT(DISTINCT session_id) sessions FROM signal WHERE {BASE} GROUP BY t ORDER BY t"))?;
        let top = qb.run(c, &format!("SELECT type, COUNT(*) events, COUNT(DISTINCT user_hash) users FROM signal WHERE {BASE} GROUP BY type ORDER BY events DESC LIMIT 20"))?;
        let mut dims = Map::new();
        for (name, expr) in DIMS {
            let mut rows = qb.run(c, &format!("SELECT COALESCE(NULLIF({expr},''),'(none)') value, COUNT(DISTINCT user_hash) users, COUNT(*) events FROM signal WHERE {BASE} GROUP BY value ORDER BY users DESC, events DESC LIMIT 8"))?;
            for r in &mut rows {
                if cur.1 > 0 { r["share"] = json!((r["users"].as_f64().unwrap_or(0.0) / cur.1 as f64 * 1000.0).round() / 1000.0); }
            }
            dims.insert(name.into(), Value::Array(rows));
        }
        let mut qe = Qb::new(&a2, &W { since, until, test, os: os.clone(), tz_min: 0 }, since, until);
        let n = qe.push(ERR_TYPE.to_string());
        let errors = qe.run(c, &format!("SELECT json_extract(payload,'$.\"TelemetryDeck.Error.id\"') error_id, json_extract(payload,'$.\"TelemetryDeck.Error.category\"') category, MAX(json_extract(payload,'$.\"TelemetryDeck.Error.message\"')) message, COUNT(*) events, COUNT(DISTINCT user_hash) users, MAX(ts) last_ts FROM signal WHERE {BASE} AND type=?{n} GROUP BY error_id, category ORDER BY events DESC LIMIT 10"))?;
        let rt = if test == 1 { 1 } else { 0 };
        let ret = retention_query(c, &a2, rt, tz_sec, &os)?;
        Ok((cur, prev, new_users, series, top, dims, errors, ret, rt))
    }).await?;
    let (cur, prev, new_users, series, top, dims, mut errors, ret, rt) = out;
    for e in &mut errors {
        let ts = e["last_ts"].as_i64().unwrap_or(0);
        e.as_object_mut().unwrap().remove("last_ts");
        e["last_event_at"] = iso(ts).into();
    }
    // Weekly cohorts: users in the first week, then how many came back in each later week.
    let (week, mon, tz) = (ret["week_ms"].as_i64().unwrap(), ret["mon_ms"].as_i64().unwrap(), w.tz_min * 60_000);
    let mut cohorts: Vec<(i64, Vec<i64>)> = vec![];
    for r in ret["rows"].as_array().unwrap() {
        let (c, n, u) = (r["cohort"].as_i64().unwrap(), r["n"].as_i64().unwrap() as usize, r["users"].as_i64().unwrap());
        if cohorts.last().is_none_or(|l| l.0 != c) { cohorts.push((c, vec![])); }
        let v = &mut cohorts.last_mut().unwrap().1;
        v.resize(n + 1, 0);
        v[n] = u;
    }
    let cohorts: Vec<Value> = cohorts.into_iter().map(|(c, v)| {
        let start = c * week - mon - tz;
        let size = v.first().copied().unwrap_or(0);
        json!({ "week_start": &iso(start + tz)[..10], "users": size, "retained_users": v,
                "retained_pct": v.iter().map(|x| if size > 0 { (*x as f64 / size as f64 * 1000.0).round() / 1000.0 } else { 0.0 }).collect::<Vec<_>>() })
    }).collect();
    let mut m = meta(&app, &w);
    m["series_bucket"] = (if span <= 2 * DAY { "hour" } else { "day" }).into();
    Ok(Json(json!({
        "meta": m,
        "totals": { "events": trend(cur.0, prev.0), "users": trend(cur.1, prev.1), "sessions": trend(cur.2, prev.2),
                    "new_users": new_users[0]["n"], "previous_period": { "since": iso(w.since - span), "until": iso(w.since) } },
        "series": series,
        "top_events": top,
        "breakdowns": dims,
        "errors": errors,
        "retention": { "basis": if rt == 1 { "test" } else { "live" }, "note": "last 8 weeks (weeks start Monday), independent of the window; retained_users[n] = users of the cohort active n weeks later", "cohorts": cohorts },
    })).into_response())
}

async fn events(State(st): S, Extension(ctx): Extension<Ctx>, Query(raw): Query<Vec<(String, String)>>) -> RR {
    let p = P(raw);
    p.check(&[&COMMON, &EVENTS_PARAMS])?;
    let w = window(&p)?;
    let limit = p.int("limit", 100, 1, 500)?;
    let cursor = p.get("cursor").map(|c| c.parse::<i64>().map_err(|_| ApiErr::bad("bad_parameter", format!("cursor={c:?}"), "pass next_cursor exactly as returned"))).transpose()?;
    let mut qb = Qb::new(&ctx.app_id, &w, w.since, w.until);
    let extra = filters(&p, &mut qb)?;
    let cn = qb.push(cursor);
    let ln = qb.push(limit + 1);
    let sql = format!("SELECT id, ts, type, user_hash, session_id, float_value, is_test, app_version, build, os, os_version, device_model, locale, sdk, country, region, city, payload
                       FROM signal WHERE {BASE}{extra} AND (?{cn} IS NULL OR id<?{cn}) ORDER BY id DESC LIMIT ?{ln}");
    let mut rows = run(&st, move |c| Ok(qb.run(c, &sql)?)).await?;
    let truncated = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next = truncated.then(|| rows.last().and_then(|r| r["id"].as_i64())).flatten();
    // Column order matters for csv/md: identity first, then dimensions, then custom parameters.
    let rows: Vec<Value> = rows.into_iter().map(|r| {
        let mut o = r.as_object().unwrap().clone();
        let ts = o.remove("ts").and_then(|t| t.as_i64()).unwrap_or(0);
        let user = o.remove("user_hash").and_then(|u| u.as_str().map(|s| s[..s.len().min(12)].to_string()));
        let params: Value = o.remove("payload").and_then(|p| p.as_str().and_then(|s| serde_json::from_str(s).ok())).unwrap_or(json!({}));
        let is_test = o.remove("is_test").and_then(|t| t.as_i64()) == Some(1);
        let mut out = Map::new();
        out.insert("id".into(), o.remove("id").unwrap_or(Value::Null));
        out.insert("time".into(), iso(ts).into());
        out.insert("type".into(), o.remove("type").unwrap_or(Value::Null));
        out.insert("user".into(), user.into());
        out.insert("test".into(), is_test.into());
        out.extend(o);
        if params.as_object().is_some_and(|m| !m.is_empty()) { out.insert("params".into(), params); }
        Value::Object(without_nulls(out))
    }).collect();
    let mut m = meta(&ctx.app_id, &w);
    m["truncated"] = truncated.into();
    m["returned"] = rows.len().into();
    respond(&p, json!({ "meta": m, "next_cursor": next, "rows": rows }), Some("rows"))
}

async fn query(State(st): S, Extension(ctx): Extension<Ctx>, Query(raw): Query<Vec<(String, String)>>) -> RR {
    let p = P(raw);
    p.check(&[&COMMON, &QUERY_PARAMS])?;
    let w = window(&p)?;
    let limit = p.int("limit", 100, 1, 1000)?;

    let mut metrics: Vec<&str> = vec![];
    for m in p.get("metric").unwrap_or("events").split(',').map(str::trim) {
        if !["events", "users", "sessions"].contains(&m) {
            return Err(ApiErr::bad("bad_metric", format!("unknown metric {m:?}"), "metric is one or more of: events, users, sessions"));
        }
        if !metrics.contains(&m) { metrics.push(m); }
    }
    let interval = p.get("interval").unwrap_or("none");
    let bucket = match interval {
        "none" => None,
        "hour" => Some("strftime('%Y-%m-%d %H:00', ts/1000 + {tz}, 'unixepoch')"),
        "day" => Some("strftime('%Y-%m-%d', ts/1000 + {tz}, 'unixepoch')"),
        "week" => Some("date(ts/1000 + {tz}, 'unixepoch', 'weekday 0', '-6 days')"),
        "month" => Some("strftime('%Y-%m', ts/1000 + {tz}, 'unixepoch')"),
        o => return Err(ApiErr::bad("bad_interval", format!("interval={o:?}"), "use none, hour, day, week or month")),
    };
    let groups: Vec<&str> = p.get("group_by").map(|g| g.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    if groups.len() > 3 {
        return Err(ApiErr::bad("too_many_groups", "group_by takes at most 3 fields", "drop a field, or run two queries"));
    }
    let by_key = match p.get("order") {
        None => interval != "none",
        Some("key") => true,
        Some("value") => false,
        Some(o) => return Err(ApiErr::bad("bad_parameter", format!("order={o:?}"), "use order=value or order=key")),
    };

    let mut qb = Qb::new(&ctx.app_id, &w, w.since, w.until);
    let mut select: Vec<String> = vec![];
    if let Some(b) = bucket { select.push(format!("{} AS t", b.replace("{tz}", &(w.tz_min * 60).to_string()))); }
    for (i, g) in groups.iter().enumerate() {
        let e = field(g, &mut qb)?;
        select.push(format!("COALESCE(NULLIF({e},''),'(none)') AS g{i}"));
    }
    for (i, m) in metrics.iter().enumerate() {
        select.push(format!("{} AS m{i}", match *m { "events" => "COUNT(*)", "users" => "COUNT(DISTINCT user_hash)", _ => "COUNT(DISTINCT session_id)" }));
    }
    let extra = filters(&p, &mut qb)?;
    let keys: Vec<String> = bucket.iter().map(|_| "t".to_string()).chain((0..groups.len()).map(|i| format!("g{i}"))).collect();
    let group = if keys.is_empty() { String::new() } else { format!(" GROUP BY {}", keys.join(", ")) };
    let order = if by_key && !keys.is_empty() { format!(" ORDER BY {}", keys.join(", ")) } else { " ORDER BY m0 DESC".to_string() };
    let ln = qb.push(limit + 1);
    let sql = format!("SELECT {} FROM signal WHERE {BASE}{extra}{group}{order} LIMIT ?{ln}", select.join(", "));

    let mut rows = run(&st, move |c| Ok(qb.run(c, &sql)?)).await?;
    let truncated = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let rows: Vec<Value> = rows.into_iter().map(|r| {
        let mut o = Map::new();
        if bucket.is_some() { o.insert("t".into(), r["t"].clone()); }
        for (i, g) in groups.iter().enumerate() { o.insert((*g).into(), r[format!("g{i}")].clone()); }
        for (i, m) in metrics.iter().enumerate() { o.insert((*m).into(), r[format!("m{i}")].clone()); }
        Value::Object(o)
    }).collect();
    let mut m = meta(&ctx.app_id, &w);
    m["truncated"] = truncated.into();
    m["returned"] = rows.len().into();
    m["query"] = json!({ "metric": metrics, "group_by": groups, "interval": interval, "type": p.get("type"), "where": p.all("where") });
    if truncated { m["truncated_hint"] = "more rows exist: raise limit (max 1000), narrow the window, or add where= filters".into(); }
    respond(&p, json!({ "meta": m, "rows": rows }), Some("rows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_and_parse_roundtrip() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_793_000_000_000), "2026-10-26T07:33:20Z");
        assert_eq!(parse_time("t", "2026-11-01").unwrap(), parse_time("t", "2026-11-01T00:00:00Z").unwrap());
        assert_eq!(parse_time("t", "1793000000000").unwrap(), 1_793_000_000_000);
        assert_eq!(iso(parse_time("t", "2024-02-29T12:00:00+08:00").unwrap()), "2024-02-29T04:00:00Z");
        assert!(parse_time("t", "yesterday").is_err());
        assert!(parse_time("t", "2026-13-01").is_err());
    }

    #[test]
    fn field_whitelist_and_payload_path_is_bound() {
        let w = W { since: 0, until: 1, test: 0, os: None, tz_min: 0 };
        let mut qb = Qb::new("a", &w, 0, 1);
        assert_eq!(field("type", &mut qb).unwrap(), "type");
        assert!(field("payload; DROP TABLE signal", &mut qb).is_err());
        assert!(field("nope", &mut qb).is_err());
        assert!(field("payload.a\"b", &mut qb).is_err());
        let e = field("payload.plan", &mut qb).unwrap();
        assert!(e.contains("?6") && !e.contains("plan"), "{e}");
    }
}
