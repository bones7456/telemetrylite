//! 两个写入入口:TelemetryDeck `/v2/` 与 OTLP `/v1/logs`(仅 Logs,protobuf/JSON)。

use crate::{AppState, db::{Signal, now_ms}};
use axum::{
    Json,
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use prost::Message;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, io::Read, net::{IpAddr, SocketAddr}, sync::Arc};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

type S = State<Arc<AppState>>;
type Attrs = Vec<(String, String)>;

const MAX_BATCH: usize = 1000;
const MAX_ATTRS: usize = 50;
const MAX_VALUE: usize = 1024;
const MAX_BODY: u64 = 8 << 20;

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

// ---------- 鉴权 / 公共 ----------

fn app_for_key(st: &AppState, key: &str) -> Result<String, StatusCode> {
    match st.keys.read().unwrap().get(&sha256_hex(key)) {
        Some(k) if !k.read => Ok(k.app_id.clone()),
        Some(_) => Err(StatusCode::FORBIDDEN), // a read key (tlr_) must never write
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

fn header_key(h: &HeaderMap) -> Option<&str> {
    let bearer = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    bearer.or_else(|| h.get("x-api-key").and_then(|v| v.to_str().ok()))
}

fn client_ip(st: &AppState, peer: SocketAddr, h: &HeaderMap) -> IpAddr {
    let ip = peer.ip();
    if ip.is_loopback() || st.trusted.contains(&ip) {
        let fwd = h.get("x-forwarded-for").or_else(|| h.get("x-real-ip")).and_then(|v| v.to_str().ok());
        if let Some(f) = fwd.and_then(|v| v.split(',').next()).and_then(|v| v.trim().parse().ok()) {
            return f;
        }
    }
    ip
}

fn decode_body(h: &HeaderMap, body: Bytes) -> Result<Vec<u8>, StatusCode> {
    let gz = h.get(header::CONTENT_ENCODING).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"gzip"));
    if !gz {
        return Ok(body.to_vec());
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&body[..]).take(MAX_BODY).read_to_end(&mut out).map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(out)
}

/// 时间戳不在 ±7 天内则视为客户端时钟错误,改用接收时间。
fn sane_ts(ts: Option<i64>, now: i64) -> i64 {
    ts.filter(|t| (t - now).abs() < 7 * 86_400_000).unwrap_or(now)
}

fn clip(mut s: String) -> String {
    if s.len() > MAX_VALUE {
        let mut n = MAX_VALUE;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
    s
}

struct Ctx<'a> {
    salt: &'a str,
    app_id: &'a str,
    place: crate::geo::Place,
    now: i64,
}

impl Ctx<'_> {
    /// `attrs` 进 payload;维度列从 `attrs` 再到 `extra`(OTLP 的 Resource 属性)中查找。
    fn signal(&self, kind: String, ts: i64, user: Option<&str>, session: Option<String>, attrs: &Attrs, extra: &Attrs, dims: &Dims) -> Signal {
        let pick = |names: &[&str]| names.iter().find_map(|n| attrs.iter().chain(extra).find(|(k, _)| k == n).map(|(_, v)| clip(v.clone())));
        let user_hash = user.filter(|u| !u.is_empty()).map(|u| sha256_hex(&format!("{}{}{}", self.salt, self.app_id, u)));
        let mut payload = serde_json::Map::new();
        let kept = attrs.iter().filter(|(k, _)| !dims.hidden.contains(&k.as_str()) && !dims.noise.iter().any(|p| k.starts_with(p)));
        for (k, v) in kept.take(MAX_ATTRS) {
            payload.insert(k.clone(), Value::String(clip(v.clone())));
        }
        Signal {
            app_id: self.app_id.into(),
            ts,
            received_at: self.now,
            kind: clip(kind),
            user_hash,
            session_id: session,
            app_version: pick(dims.app_version),
            build: pick(dims.build),
            os: pick(dims.os).map(|o| normalize_os(&o)),
            os_version: pick(dims.os_version),
            device_model: pick(dims.device_model),
            locale: pick(dims.locale),
            sdk: pick(dims.sdk),
            country: self.place.0.clone(),
            region: self.place.1.clone(),
            city: self.place.2.clone(),
            payload: Value::Object(payload).to_string(),
            ..Default::default()
        }
    }
}

/// The platform filter groups by the exact `os` text, so SDKs that differ only in letter case ("android" / "Android") must not split into two platforms.
fn normalize_os(os: &str) -> String {
    let os = os.trim();
    for known in ["iOS", "iPadOS", "macOS", "watchOS", "tvOS", "visionOS", "Android", "Windows", "Linux", "HarmonyOS"] {
        if os.eq_ignore_ascii_case(known) {
            return known.to_string();
        }
    }
    os.to_string()
}

/// 各维度列的候选属性名(靠前的优先),`hidden` 里的键不再重复写入 payload。
struct Dims {
    app_version: &'static [&'static str],
    build: &'static [&'static str],
    os: &'static [&'static str],
    os_version: &'static [&'static str],
    device_model: &'static [&'static str],
    locale: &'static [&'static str],
    sdk: &'static [&'static str],
    hidden: &'static [&'static str],
    /// 前缀匹配的键直接丢弃(SDK 每条信号自动附带、几乎无分析价值且占空间的系统参数)
    noise: &'static [&'static str],
}

const TD_DIMS: Dims = Dims {
    app_version: &["TelemetryDeck.AppInfo.version", "appVersion"],
    build: &["TelemetryDeck.AppInfo.buildNumber", "buildNumber"],
    os: &["TelemetryDeck.Device.operatingSystem", "operatingSystem"],
    os_version: &["TelemetryDeck.Device.systemVersion", "systemVersion"],
    device_model: &["TelemetryDeck.Device.modelName", "modelName"],
    locale: &["TelemetryDeck.RunContext.locale", "locale"],
    sdk: &["TelemetryDeck.SDK.nameAndVersion", "telemetryClientVersion"],
    // 以上列 + SDK 为兼容老版本同时发送的旧名重复项
    hidden: &[
        "TelemetryDeck.AppInfo.version", "appVersion", "TelemetryDeck.AppInfo.buildNumber", "buildNumber",
        "TelemetryDeck.Device.operatingSystem", "operatingSystem", "TelemetryDeck.Device.systemVersion", "systemVersion",
        "TelemetryDeck.Device.modelName", "modelName", "TelemetryDeck.RunContext.locale", "locale",
        "TelemetryDeck.SDK.nameAndVersion", "telemetryClientVersion",
        "platform", "majorSystemVersion", "majorMinorSystemVersion", "isSimulator", "isDebug", "isTestFlight",
        "isAppStore", "architecture", "targetEnvironment", "region", "appLanguage", "preferredLanguage",
        "extensionIdentifier",
    ],
    // 新版 SDK 每条信号附带约 50 个 TelemetryDeck.* 系统参数,约 3 KB。以下几类丢弃;
    // 保留 RunContext.isTestFlight/isAppStore/isDebug/isSimulator、UserPreference.language/region、
    // Device.timeZone 等有分析价值的,以及各预设事件(Purchase/Error/Navigation…)自带的参数。
    noise: &[
        "TelemetryDeck.Accessibility.", "TelemetryDeck.Calendar.", "TelemetryDeck.Retention.",
        "TelemetryDeck.Acquisition.firstSessionDate", "TelemetryDeck.AppInfo.versionAndBuildNumber",
        "TelemetryDeck.Device.screen", "TelemetryDeck.Device.orientation", "TelemetryDeck.Device.systemMajor",
        "TelemetryDeck.Device.architecture", "TelemetryDeck.Device.platform",
        "TelemetryDeck.SDK.", "TelemetryDeck.RunContext.targetEnvironment", "TelemetryDeck.RunContext.language",
        "TelemetryDeck.UserPreference.colorScheme", "TelemetryDeck.UserPreference.layoutDirection",
    ],
};

const OTEL_DIMS: Dims = Dims {
    app_version: &["service.version", "app.version"],
    build: &["app.build_id"],
    os: &["os.name", "os.type"],
    os_version: &["os.version"],
    device_model: &["device.model.identifier", "device.model.name"],
    locale: &["locale", "user.locale"],
    sdk: &["telemetry.sdk.name"],
    hidden: &["user.id", "enduser.id", "session.id", "event.name"],
    noise: &[],
};

// ---------- TelemetryDeck /v2/ ----------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TdSignal {
    received_at: Option<String>,
    client_user: Option<String>,
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    r#type: String,
    float_value: Option<f64>,
    #[serde(default)]
    payload: HashMap<String, Value>,
    is_test_mode: Option<Value>,
}

/// SDK 发送 `yyyy-MM-dd'T'HH:mm:ssZ`(时区形如 +0000),补冒号后按 RFC3339 解析。
fn parse_time(s: &str) -> Option<i64> {
    let mut s = s.to_string();
    let n = s.len();
    if n > 5 && matches!(s.as_bytes()[n - 5], b'+' | b'-') && !s[n - 4..].contains(':') {
        s.insert(n - 2, ':');
    }
    OffsetDateTime::parse(&s, &Rfc3339).ok().map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
}

fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub async fn td_key(st: S, Path(key): Path<String>, peer: ConnectInfo<SocketAddr>, h: HeaderMap, body: Bytes) -> Response {
    td_ingest(st, key, peer, h, body).await
}

pub async fn td_key_ns(st: S, Path((key, _ns)): Path<(String, String)>, peer: ConnectInfo<SocketAddr>, h: HeaderMap, body: Bytes) -> Response {
    td_ingest(st, key, peer, h, body).await
}

async fn td_ingest(State(st): S, key: String, ConnectInfo(peer): ConnectInfo<SocketAddr>, h: HeaderMap, body: Bytes) -> Response {
    let app_id = match app_for_key(&st, &key) {
        Ok(a) => a,
        Err(c) => return c.into_response(),
    };
    let Ok(raw) = decode_body(&h, body) else { return StatusCode::BAD_REQUEST.into_response() };
    let Ok(sigs) = serde_json::from_slice::<Vec<TdSignal>>(&raw) else { return StatusCode::BAD_REQUEST.into_response() };
    let now = now_ms();
    let ctx = Ctx { salt: &st.salt, app_id: &app_id, place: st.geo.lookup(client_ip(&st, peer, &h)), now };
    let batch: Vec<Signal> = sigs.into_iter().take(MAX_BATCH).map(|s| {
        let attrs: Attrs = s.payload.iter().map(|(k, v)| (k.clone(), value_str(v))).collect();
        let ts = sane_ts(s.received_at.as_deref().and_then(parse_time), now);
        let mut sig = ctx.signal(s.r#type, ts, s.client_user.as_deref(), s.session_id, &attrs, &vec![], &TD_DIMS);
        sig.float_value = s.float_value;
        sig.is_test = match &s.is_test_mode {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(v)) => v == "true",
            _ => false,
        };
        sig
    }).collect();
    enqueue(&st, batch)
}

fn enqueue(st: &AppState, batch: Vec<Signal>) -> Response {
    if batch.is_empty() {
        return StatusCode::OK.into_response();
    }
    match st.tx.try_send(batch) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "5")]).into_response(),
    }
}

// ---------- OTLP /v1/logs ----------

// 只声明用到的字段,免去 protoc 与完整 opentelemetry-proto 依赖。
#[derive(Message)]
struct ExportLogsRequest {
    #[prost(message, repeated, tag = "1")]
    resource_logs: Vec<ResourceLogs>,
}
#[derive(Message)]
struct ResourceLogs {
    #[prost(message, optional, tag = "1")]
    resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    scope_logs: Vec<ScopeLogs>,
}
#[derive(Message)]
struct Resource {
    #[prost(message, repeated, tag = "1")]
    attributes: Vec<KeyValue>,
}
#[derive(Message)]
struct ScopeLogs {
    #[prost(message, repeated, tag = "2")]
    log_records: Vec<LogRecord>,
}
#[derive(Message)]
struct LogRecord {
    #[prost(fixed64, tag = "1")]
    time_unix_nano: u64,
    #[prost(message, optional, tag = "5")]
    body: Option<AnyValue>,
    #[prost(message, repeated, tag = "6")]
    attributes: Vec<KeyValue>,
    #[prost(fixed64, tag = "11")]
    observed_time_unix_nano: u64,
    #[prost(string, tag = "12")]
    event_name: String,
}
#[derive(Message)]
struct KeyValue {
    #[prost(string, tag = "1")]
    key: String,
    #[prost(message, optional, tag = "2")]
    value: Option<AnyValue>,
}
#[derive(Message)]
struct AnyValue {
    #[prost(oneof = "any_value::Value", tags = "1, 2, 3, 4")]
    value: Option<any_value::Value>,
}
mod any_value {
    #[derive(prost::Oneof)]
    pub enum Value {
        #[prost(string, tag = "1")]
        Str(String),
        #[prost(bool, tag = "2")]
        Bool(bool),
        #[prost(int64, tag = "3")]
        Int(i64),
        #[prost(double, tag = "4")]
        Double(f64),
    }
}

/// 解码后的统一中间形态:一条 LogRecord + 其 Resource 属性。
struct Rec {
    res: Attrs,
    attrs: Attrs,
    ts_ns: u64,
    name: String,
    body: String,
}

fn from_proto(req: ExportLogsRequest) -> Vec<Rec> {
    let any = |v: Option<AnyValue>| match v.and_then(|v| v.value) {
        Some(any_value::Value::Str(s)) => s,
        Some(any_value::Value::Bool(b)) => b.to_string(),
        Some(any_value::Value::Int(i)) => i.to_string(),
        Some(any_value::Value::Double(d)) => d.to_string(),
        None => String::new(),
    };
    let kvs = |v: Vec<KeyValue>| -> Attrs { v.into_iter().map(|kv| (kv.key, any(kv.value))).collect() };
    let mut out = Vec::new();
    for rl in req.resource_logs {
        let res = kvs(rl.resource.map(|r| r.attributes).unwrap_or_default());
        for lr in rl.scope_logs.into_iter().flat_map(|s| s.log_records) {
            let ts = if lr.time_unix_nano > 0 { lr.time_unix_nano } else { lr.observed_time_unix_nano };
            out.push(Rec { res: res.clone(), ts_ns: ts, name: lr.event_name, body: any(lr.body), attrs: kvs(lr.attributes) });
        }
    }
    out
}

/// OTLP/JSON:AnyValue 形如 {"stringValue":"x"} / {"intValue":"1"};uint64 既可能是字符串也可能是数字。
fn from_json(v: &Value) -> Vec<Rec> {
    fn any(v: &Value) -> String {
        let Some(o) = v.as_object() else { return String::new() };
        o.values().next().map(value_str).unwrap_or_default()
    }
    fn kvs(v: &Value) -> Attrs {
        v.as_array().into_iter().flatten()
            .filter_map(|kv| Some((kv.get("key")?.as_str()?.to_string(), any(kv.get("value")?))))
            .collect()
    }
    fn nano(v: Option<&Value>) -> u64 {
        match v {
            Some(Value::String(s)) => s.parse().unwrap_or(0),
            Some(n) => n.as_u64().unwrap_or(0),
            None => 0,
        }
    }
    let mut out = Vec::new();
    for rl in v["resourceLogs"].as_array().into_iter().flatten() {
        let res = kvs(&rl["resource"]["attributes"]);
        for lr in rl["scopeLogs"].as_array().into_iter().flatten().flat_map(|s| s["logRecords"].as_array().into_iter().flatten()) {
            let ts = match nano(lr.get("timeUnixNano")) { 0 => nano(lr.get("observedTimeUnixNano")), t => t };
            out.push(Rec {
                res: res.clone(),
                ts_ns: ts,
                name: lr["eventName"].as_str().unwrap_or_default().to_string(),
                body: lr.get("body").map(any).unwrap_or_default(),
                attrs: kvs(&lr["attributes"]),
            });
        }
    }
    out
}

fn rec_to_signal(ctx: &Ctx, r: Rec) -> Signal {
    let get = |names: &[&str]| names.iter().find_map(|n| r.attrs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()));
    let name = [Some(r.name.clone()).filter(|n| !n.is_empty()), get(&["event.name"]), Some(r.body.clone()).filter(|b| !b.is_empty())]
        .into_iter().flatten().next().unwrap_or_else(|| "log".into());
    let user = get(&["user.id", "enduser.id"]);
    let session = get(&["session.id"]);
    let ts = (r.ts_ns > 0).then_some((r.ts_ns / 1_000_000) as i64);
    ctx.signal(name, sane_ts(ts, ctx.now), user.as_deref(), session, &r.attrs, &r.res, &OTEL_DIMS)
}

pub async fn otlp_logs(State(st): S, ConnectInfo(peer): ConnectInfo<SocketAddr>, path_key: Option<Path<String>>, h: HeaderMap, body: Bytes) -> Response {
    let key = path_key.map(|Path(k)| k).or_else(|| header_key(&h).map(String::from)).unwrap_or_default();
    let app_id = match app_for_key(&st, &key) {
        Ok(a) => a,
        Err(c) => return c.into_response(),
    };
    let Ok(raw) = decode_body(&h, body) else { return StatusCode::BAD_REQUEST.into_response() };
    let is_json = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("json"));
    let recs = if is_json {
        match serde_json::from_slice::<Value>(&raw) { Ok(v) => from_json(&v), Err(_) => return StatusCode::BAD_REQUEST.into_response() }
    } else {
        match ExportLogsRequest::decode(&raw[..]) { Ok(r) => from_proto(r), Err(_) => return StatusCode::BAD_REQUEST.into_response() }
    };
    let ctx = Ctx { salt: &st.salt, app_id: &app_id, place: st.geo.lookup(client_ip(&st, peer, &h)), now: now_ms() };
    let batch = recs.into_iter().take(MAX_BATCH).map(|r| rec_to_signal(&ctx, r)).collect();
    let resp = enqueue(&st, batch);
    if resp.status() != StatusCode::OK {
        return resp;
    }
    // 空的 Export*Response:protobuf 为 0 字节,JSON 为 {}
    if is_json { Json(serde_json::json!({})).into_response() } else { ([(header::CONTENT_TYPE, "application/x-protobuf")], Vec::<u8>::new()).into_response() }
}

/// Metrics / Traces 不存储,返回成功避免客户端重试。
pub async fn otlp_ignore() -> StatusCode {
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_time_formats() {
        assert_eq!(parse_time("2026-11-07T12:34:56+0000"), Some(1_794_054_896_000));
        assert_eq!(parse_time("2026-11-07T12:34:56Z"), Some(1_794_054_896_000));
        assert_eq!(parse_time("2026-11-07T20:34:56+08:00"), Some(1_794_054_896_000));
        assert_eq!(parse_time("garbage"), None);
    }

    #[test]
    fn otlp_json_mapping() {
        let v: Value = serde_json::from_str(r#"{"resourceLogs":[{"resource":{"attributes":[{"key":"service.version","value":{"stringValue":"1.2"}}]},
          "scopeLogs":[{"logRecords":[{"timeUnixNano":"1794054896000000000","body":{"stringValue":"hi"},
          "attributes":[{"key":"event.name","value":{"stringValue":"Tapped"}},{"key":"n","value":{"intValue":"3"}}]}]}]}]}"#).unwrap();
        let recs = from_json(&v);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].ts_ns, 1_794_054_896_000_000_000);
        assert_eq!(recs[0].res[0], ("service.version".into(), "1.2".into()));
        assert_eq!(recs[0].attrs[1], ("n".into(), "3".into()));
    }

    #[test]
    fn td_drops_sdk_noise_but_keeps_custom_and_preset_params() {
        let mut attrs: Attrs = vec![
            ("TelemetryDeck.AppInfo.version".into(), "1.2".into()),
            ("appVersion".into(), "1.2".into()),
            ("TelemetryDeck.Calendar.dayOfWeek".into(), "6".into()),
            ("TelemetryDeck.Device.screenResolutionWidth".into(), "2056".into()),
            ("TelemetryDeck.SDK.version".into(), "2.14.2".into()),
            ("TelemetryDeck.RunContext.isTestFlight".into(), "false".into()),
            ("TelemetryDeck.Purchase.type".into(), "subscription".into()),
            ("plan".into(), "yearly".into()),
        ];
        // 模拟 SDK 的大量系统参数排在前面:自定义参数不能因 50 个上限被挤掉
        for i in 0..80 {
            attrs.insert(0, (format!("TelemetryDeck.Calendar.n{i}"), "x".into()));
        }
        let ctx = Ctx { salt: "s", app_id: "a", place: Default::default(), now: 0 };
        let sig = ctx.signal("t".into(), 0, Some("u"), None, &attrs, &vec![], &TD_DIMS);
        let p: Value = serde_json::from_str(&sig.payload).unwrap();
        let keys: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(sig.app_version.as_deref(), Some("1.2"));
        let mut want = vec!["TelemetryDeck.Purchase.type", "TelemetryDeck.RunContext.isTestFlight", "plan"];
        want.sort();
        let mut got = keys.clone();
        got.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn os_names_are_normalized() {
        assert_eq!(normalize_os("android"), "Android");
        assert_eq!(normalize_os(" IOS "), "iOS");
        assert_eq!(normalize_os("macos"), "macOS");
        assert_eq!(normalize_os("Tizen"), "Tizen");
    }
}
