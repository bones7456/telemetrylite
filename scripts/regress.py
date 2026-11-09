#!/usr/bin/env python3
"""Black-box regression test for TelemetryLite (Python 3 standard library only).

  scripts/regress.py --spawn target/release/telemetrylite          # start the binary on a free port, test it, clean up
  scripts/regress.py --base http://127.0.0.1:8888 --password PW    # test an instance that is already running

Options: --host HEADER   send this Host header (testing through nginx)
         --geoip-db PATH --spawn: also run the GeoIP checks with this mmdb file (DB-IP City Lite)
         --geo           with --base: run the GeoIP checks (the server needs GeoIP and must trust this client)
         --nginx         with --base: run the checks that only make sense behind the nginx the installer sets up
Exit status 0 = all checks passed.
"""
import argparse, re, gzip, http.client, json, os, socket, sqlite3, subprocess, sys, tempfile, time, urllib.parse, urllib.request

ap = argparse.ArgumentParser()
ap.add_argument("--spawn"); ap.add_argument("--base"); ap.add_argument("--password", default="regress-pw")
ap.add_argument("--host"); ap.add_argument("--expect-version", help="the version the binary and the page must report (e.g. 0.1.7)"); ap.add_argument("--geoip-db"); ap.add_argument("--geo", action="store_true"); ap.add_argument("--nginx", action="store_true")
args = ap.parse_args()
if bool(args.spawn) == bool(args.base):
    ap.error("give exactly one of --spawn BINARY or --base URL")

proc, tmp, db_path = None, None, None
if args.spawn:
    tmp = tempfile.TemporaryDirectory(prefix="tl-regress-")
    s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()
    db_path = os.path.join(tmp.name, "t.db")
    env = {**os.environ, "TL_LISTEN": f"127.0.0.1:{port}", "TL_DB_PATH": db_path, "TL_ADMIN_PASSWORD": args.password, "TL_LANG": "en"}
    if args.geoip_db: env["TL_GEOIP_DB"] = args.geoip_db
    proc = subprocess.Popen([os.path.abspath(args.spawn)], env=env, cwd=tmp.name, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    base = f"http://127.0.0.1:{port}"
    for _ in range(100):
        try:
            if urllib.request.urlopen(base + "/", timeout=1).status == 200: break
        except Exception: time.sleep(0.1)
    else:
        proc.kill(); sys.exit("server did not start: " + proc.stderr.read().decode(errors="replace"))
    GEO = bool(args.geoip_db); NGINX = False
else:
    base, GEO, NGINX = args.base, args.geo, args.nginx
pw = args.password
u = urllib.parse.urlparse(base)
cookie = {"v": None}

def req(method, path, body=None, headers=None, raw=None):
    c = http.client.HTTPConnection(u.hostname, u.port, timeout=20)
    h = dict(headers or {})
    if args.host: h["Host"] = args.host
    if cookie["v"]: h["Cookie"] = cookie["v"]
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    if body is not None and "content-type" not in h: h["content-type"] = "application/json"
    c.request(method, path, data, h); r = c.getresponse(); b = r.read()
    if r.getheader("set-cookie"): cookie["v"] = r.getheader("set-cookie").split(";")[0]
    try: j = json.loads(b) if b else None
    except Exception: j = None
    return r.status, j, b, r

ok = bad = 0
def check(name, cond, detail=""):
    global ok, bad
    if cond: ok += 1; print(f"  PASS  {name}")
    else: bad += 1; print(f"  FAIL  {name}  {detail}")

def run():
    print("== UI and login")
    s, _, b, _ = req("GET", "/"); check("GET / is 200 and serves the UI", s == 200 and b"TelemetryLite" in b)
    m = re.search(rb"TelemetryLite(?:</a>)? v(\d+\.\d+\.\d+[\w.-]*)", b)
    check("the page footer shows the running version (placeholder replaced)", m is not None and b"__VERSION__" not in b)
    if args.spawn:
        out = subprocess.run([os.path.abspath(args.spawn), "--version"], capture_output=True, text=True).stdout.strip()
        check("--version prints the version", out == f"telemetrylite {m.group(1).decode()}" if m else False, out)
    if args.expect_version:
        check(f"the reported version is {args.expect_version}", m is not None and m.group(1).decode() == args.expect_version, m.group(1).decode() if m else "none")
    check("UI contains the AI prompt, refresh and analytics pages", b"agentPrompt" in b and b"autoRefresh" in b and b"retentionTitle" in b)
    check("API without a session is 401", req("GET", "/api/apps")[0] == 401)
    check("wrong password is 401", req("POST", "/api/login", {"password": "nope"})[0] == 401)
    s, _, _, r = req("POST", "/api/login", {"password": pw}); check("right password is 200", s == 200)
    sc = r.getheader("set-cookie") or ""
    check("session cookie is HttpOnly + SameSite=Strict", "HttpOnly" in sc and "SameSite=Strict" in sc)

    print("== apps and keys")
    s, j, _, _ = req("POST", "/api/apps", {"name": "Regression", "retention_days": 30}); app = j["id"]; check("create app", s == 200 and len(app) == 36)
    s, j, _, _ = req("POST", f"/api/apps/{app}/keys", {"label": "reg"}); key = j["key"]; check("issue key (tl_ prefix)", s == 200 and key.startswith("tl_"))
    key2 = req("POST", f"/api/apps/{app}/keys", {"label": "temp"})[1]["key"]
    check("key list never exposes the key", all("key" not in k for k in req("GET", f"/api/apps/{app}/keys")[1]))
    for bad_label, why in [({}, "missing"), ({"label": ""}, "empty"), ({"label": "   "}, "blank"), ({"label": "x" * 101}, "too long")]:
        check(f"issuing a key with a {why} name is rejected (400)", req("POST", f"/api/apps/{app}/keys", bad_label)[0] == 400)
    check("the name is trimmed and stored", req("POST", f"/api/apps/{app}/keys", {"label": "  trimmed  "})[0] == 200 and any(k["label"] == "trimmed" for k in req("GET", f"/api/apps/{app}/keys")[1]))
    check("empty app name is rejected", req("POST", "/api/apps", {"name": " "})[0] == 400)

    print("== TelemetryDeck ingest")
    now = time.strftime("%Y-%m-%dT%H:%M:%S+0000", time.gmtime())
    def td(sig, k=None, extra=None): return req("POST", f"/k/{k or key}/v2/", sig, extra)[0]
    check("wrong key is 401", td([], "tl_wrong") == 401)
    check("empty array is 200", td([]) == 200)
    check("malformed JSON is 400", req("POST", f"/k/{key}/v2/", raw=b"{oops", headers={"content-type": "application/json"})[0] == 400)
    noise = {f"TelemetryDeck.Calendar.n{i}": "x" for i in range(60)}
    payload = {"TelemetryDeck.AppInfo.version": "2.0.1", "appVersion": "2.0.1", "TelemetryDeck.Device.operatingSystem": "iOS", "TelemetryDeck.Device.systemVersion": "iOS 18.2.0",
               "TelemetryDeck.Device.modelName": "iPhone16,2", "TelemetryDeck.RunContext.locale": "zh_CN", "TelemetryDeck.SDK.nameAndVersion": "SwiftSDK 2.14.2",
               "TelemetryDeck.Accessibility.isBoldTextEnabled": "false", "TelemetryDeck.RunContext.isTestFlight": "false", "plan": "yearly", **noise}
    check("SDK-format signal is 200", td([{"receivedAt": now, "appID": "x", "clientUser": "u-1", "sessionID": "s-1", "type": "Purchase.completed", "floatValue": 29.99, "payload": payload, "isTestMode": "false"}]) == 200)
    check("boolean isTestMode and RFC 3339 time are accepted", td([{"receivedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "clientUser": "u-2", "sessionID": "s-2", "type": "Bool.test", "payload": {}, "isTestMode": True}]) == 200)
    check("test-mode signal is 200", td([{"receivedAt": now, "clientUser": "u-3", "sessionID": "s-3", "type": "Debug.event", "payload": {}, "isTestMode": "true"}]) == 200)
    for i in range(3):
        td([{"receivedAt": now, "clientUser": f"err-{i}", "sessionID": f"e{i}", "type": "TelemetryDeck.Error.occurred", "isTestMode": "false",
             "payload": {"TelemetryDeck.Error.id": "NetworkTimeout", "TelemetryDeck.Error.category": "thrown-exception", "TelemetryDeck.Error.message": "Request timed out"}}])
    check("a 1500-signal batch is accepted", td([{"clientUser": "bulk", "type": "Bulk.event", "payload": {}} for _ in range(1500)]) == 200)
    far = time.strftime("%Y-%m-%dT%H:%M:%S+0000", time.gmtime(time.time() - 30 * 86400))
    td([{"receivedAt": far, "clientUser": "u-old", "sessionID": "so", "type": "Old.event", "payload": {}}])

    print("== OTLP ingest")
    otlp = {"resourceLogs": [{"resource": {"attributes": [{"key": "service.version", "value": {"stringValue": "9.9"}}, {"key": "os.name", "value": {"stringValue": "Android"}}]},
            "scopeLogs": [{"logRecords": [{"timeUnixNano": str(int(time.time() * 1e9)), "eventName": "Otlp.json", "attributes": [{"key": "user.id", "value": {"stringValue": "o-1"}}, {"key": "n", "value": {"intValue": "3"}}]}]}]}]}
    s, j, _, _ = req("POST", "/v1/logs", otlp, {"authorization": f"Bearer {key}"}); check("OTLP JSON with Bearer is 200 and returns {}", s == 200 and j == {})
    check("OTLP with x-api-key is 200", req("POST", "/v1/logs", otlp, {"x-api-key": key})[0] == 200)
    check("OTLP with the key in the path is 200", req("POST", f"/k/{key}/v1/logs", otlp)[0] == 200)
    check("OTLP without a key is 401", req("POST", "/v1/logs", otlp)[0] == 401)
    def varint(n):
        o = b""
        while True:
            x = n & 0x7f; n >>= 7
            if n: o += bytes([x | 0x80])
            else: return o + bytes([x])
    ld = lambda tag, b: bytes([tag << 3 | 2]) + varint(len(b)) + b
    kv = lambda k, v: ld(1, k.encode()) + ld(2, ld(1, v.encode()))
    rec = bytes([1 << 3 | 1]) + int(time.time() * 1e9).to_bytes(8, "little") + ld(12, b"Otlp.proto.gzip") + ld(6, kv("user.id", "o-2"))
    pb = gzip.compress(ld(1, ld(1, ld(1, kv("service.version", "9.9"))) + ld(2, ld(2, rec))))
    check("OTLP protobuf + gzip is 200", req("POST", "/v1/logs", raw=pb, headers={"authorization": f"Bearer {key}", "content-type": "application/x-protobuf", "content-encoding": "gzip"})[0] == 200)
    check("garbage protobuf is 400", req("POST", "/v1/logs", raw=b"zzzz", headers={"authorization": f"Bearer {key}", "content-type": "application/x-protobuf"})[0] == 400)
    check("OTLP metrics/traces are accepted and discarded", req("POST", "/v1/metrics", raw=b"", headers={"authorization": f"Bearer {key}"})[0] == 200 and req("POST", "/v1/traces", raw=b"")[0] == 200)
    time.sleep(1.5)

    print("== stored events")
    def ev(t):   # newest event with this exact name (the bulk test above floods the newest-500 window)
        r = req("GET", f"/api/apps/{app}/events?limit=1&type={t}")[1]
        return r[0] if r else None
    p = ev("Purchase.completed"); pj = json.loads(p["payload"]) if p else {}
    check("dimension columns are extracted", p and p["app_version"] == "2.0.1" and p["os"] == "iOS" and p["os_version"] == "iOS 18.2.0" and p["device_model"] == "iPhone16,2" and p["locale"] == "zh_CN" and p["float_value"] == 29.99, str(p))
    check("SDK noise is dropped; custom and useful parameters are kept", pj == {"TelemetryDeck.RunContext.isTestFlight": "false", "plan": "yearly"}, str(pj))
    check("user id is hashed (64 hex chars)", p and len(p["user_hash"]) == 64 and "u-1" not in p["user_hash"])
    check("test-mode signals are flagged", (ev("Debug.event") or {}).get("is_test") == 1 and (ev("Bool.test") or {}).get("is_test") == 1)
    check("a timestamp more than 7 days old is replaced by the receive time", abs(ev("Old.event")["ts"] / 1000 - time.time()) < 60)
    types = {t["type"]: t for t in req("GET", f"/api/apps/{app}/event-types?days=7&test=0")[1]}
    check("a 1500-signal batch stores at most 1000", types.get("Bulk.event", {}).get("events") == 1000, str(types.get("Bulk.event")))
    o = ev("Otlp.json"); check("OTLP mapping: name, version, OS, parameters", o and o["app_version"] == "9.9" and o["os"] == "Android" and json.loads(o["payload"]) == {"n": "3"}, str(o))
    check("OTLP protobuf + gzip event is stored", ev("Otlp.proto.gzip") is not None)

    print("== statistics")
    q = "days=7&test=0&tz=480"
    s, st, _, _ = req("GET", f"/api/apps/{app}/stats?{q}")
    check("stats is 200 with all fields", s == 200 and all(k in st for k in ["totals", "series", "top", "dims", "last_hour_users", "other_events"]), str(st)[:160])
    check("live view hides test events and counts the hidden ones", st["other_events"] == 2 and all(t["type"] != "Debug.event" for t in st["top"]), f'other={st["other_events"]}')
    check("users in the last hour > 0", st["last_hour_users"] > 0)
    check("version breakdown has 2.0.1 and 9.9", {"2.0.1", "9.9"} <= {r["v"] for r in st["dims"]["app_version"]})
    check("OS breakdown does not repeat the platform name", "iOS 18.2.0" in {r["v"] for r in st["dims"]["os_version"]} and not any(r["v"].startswith("iOS iOS") for r in st["dims"]["os_version"]))
    s, sm, _, _ = req("GET", f"/api/apps/{app}/summary"); check("summary: users, SDK, errors", s == 200 and sm["users"] > 0 and sm["sdk"] == "SwiftSDK 2.14.2" and sm["errors"] == 3, str(sm))
    s, et, _, _ = req("GET", f"/api/apps/{app}/event-types?{q}"); check("event-types has users/events/last_ts", s == 200 and et and {"type", "users", "events", "last_ts"} <= set(et[0]))
    s, us, _, _ = req("GET", f"/api/apps/{app}/users?{q}"); check("users: new and active", s == 200 and sum(x["users"] for x in us["new"]) >= 5 and us["active"])
    s, rt, _, _ = req("GET", f"/api/apps/{app}/retention?{q}"); check("retention: cohorts start at week 0", s == 200 and rt["rows"] and any(r["n"] == 0 for r in rt["rows"]), str(rt)[:160])
    s, vs, _, _ = req("GET", f"/api/apps/{app}/versions?{q}"); check("versions over time", s == 200 and vs)
    s, er, _, _ = req("GET", f"/api/apps/{app}/errors?{q}")
    check("errors are grouped by id and category", s == 200 and len(er["top"]) == 1 and er["top"][0]["err_id"] == "NetworkTimeout" and er["top"][0]["events"] == 3 and er["top"][0]["users"] == 3, str(er)[:200])
    st_t = req("GET", f"/api/apps/{app}/stats?days=7&test=1&tz=0")[1]; check("test view contains only test events", st_t["totals"]["events"] == 2 and st_t["other_events"] > 0)
    check("event-name filter", all(e["type"] == "Purchase.completed" for e in req("GET", f"/api/apps/{app}/events?type=Purchase.completed")[1]))

    print("== security and privacy")
    check("hostile version string is stored as data", td([{"clientUser": "x", "type": "Xss.test", "payload": {"TelemetryDeck.AppInfo.version": "<img src=x onerror=1>"}}]) == 200)
    saved = cookie["v"]; cookie["v"] = None
    check("admin API rejects writes without a session", req("POST", "/api/apps", {"name": "x"})[0] == 401)
    cookie["v"] = saved
    if db_path:
        td([{"clientUser": "ip-test", "sessionID": "ip", "type": "Ip.privacy", "payload": {"note": "x"}}], extra={"x-forwarded-for": "8.8.8.8"})
        time.sleep(1.5)
        raw = b"".join(open(f, "rb").read() for f in (db_path, db_path + "-wal") if os.path.exists(f))
        check("no raw IP address is written to the database", b"8.8.8.8" not in raw and b"127.0.0.1" not in raw)
        v = sqlite3.connect(db_path).execute("PRAGMA user_version").fetchone()[0]
        check("database schema version is set", v >= 1, str(v))

    print("== key revocation and management")
    kid = [k for k in req("GET", f"/api/apps/{app}/keys")[1] if k["label"] == "temp"][0]["id"]
    check("key works before revocation", td([], key2) == 200)
    check("revoke the key", req("DELETE", f"/api/keys/{kid}")[0] == 200)
    check("revoked key is rejected immediately", td([], key2) == 401)
    check("other keys are unaffected", td([]) == 200)
    check("rename app and change retention", req("PATCH", f"/api/apps/{app}", {"name": "Regression 2", "retention_days": 45})[0] == 200 and req("GET", f"/api/apps/{app}")[1]["retention_days"] == 45)
    check("app list has the 24h event count", any(a["id"] == app and a["events_24h"] > 0 for a in req("GET", "/api/apps")[1]))
    check("delete app removes its keys and data", req("DELETE", f"/api/apps/{app}")[0] == 200 and td([], key) == 401 and all(a["id"] != app for a in req("GET", "/api/apps")[1]))
    app = key = None

    print("== platform filter (one app, iOS + Android)")
    pa = req("POST", "/api/apps", {"name": "Platforms"})[1]["id"]; pk = req("POST", f"/api/apps/{pa}/keys", {"label": "plat"})[1]["key"]
    nowp = time.strftime("%Y-%m-%dT%H:%M:%S+0000", time.gmtime())
    def sig(user, typ, os_name, ver, extra=None, model="m"):
        pl = {"TelemetryDeck.AppInfo.version": ver, "TelemetryDeck.Device.operatingSystem": os_name, "TelemetryDeck.Device.systemVersion": f"{os_name} 1.0", "TelemetryDeck.Device.modelName": model}; pl.update(extra or {})
        return {"receivedAt": nowp, "clientUser": user, "sessionID": user + "s", "type": typ, "payload": pl, "isTestMode": "false"}
    err = lambda u, os_name, ver: sig(u, "TelemetryDeck.Error.occurred", os_name, ver, {"TelemetryDeck.Error.id": "NetworkTimeout", "TelemetryDeck.Error.category": "thrown-exception"})
    batch = [sig("i1", "Ios.only", "iOS", "3.0"), sig("i2", "Ios.only", "iOS", "3.0"), sig("i1", "Shared.event", "iOS", "3.0"), err("i1", "iOS", "3.0"),
             sig("a1", "Android.only", "android", "7.0"), sig("a2", "Android.only", "ANDROID", "7.0"), sig("a3", "Android.only", "Android", "7.1"),
             sig("a1", "Shared.event", "Android", "7.0"), err("a1", "Android", "7.0"), err("a2", "Android", "7.0")]
    check("mixed iOS/Android batch is accepted", req("POST", f"/k/{pk}/v2/", batch)[0] == 200); time.sleep(1.5)
    g = lambda ep, plat="", extra="": req("GET", f"/api/apps/{pa}/{ep}?days=7&test=0&tz=0&platform={urllib.parse.quote(plat)}{extra}")[1]
    pl = {x["os"]: x for x in req("GET", f"/api/apps/{pa}/platforms")[1]}
    check("platforms lists iOS and Android (case normalized, no duplicates)", set(pl) == {"iOS", "Android"} and pl["iOS"]["users"] == 2 and pl["Android"]["users"] == 3, str(pl))
    check("no platform = everything (5 users)", g("stats")["totals"]["users"] == 5)
    a, i = g("stats", "Android"), g("stats", "iOS")
    check("stats?platform=Android counts only Android", a["totals"]["users"] == 3 and {t["type"] for t in a["top"]} == {"Android.only", "Shared.event", "TelemetryDeck.Error.occurred"}, str(a["top"]))
    check("stats?platform=iOS counts only iOS", i["totals"]["users"] == 2 and "Android.only" not in {t["type"] for t in i["top"]})
    check("series and breakdowns follow the filter", sum(x["users"] for x in a["series"]) == 3 and {r["v"] for r in a["dims"]["app_version"]} == {"7.0", "7.1"} and all(r["v"].lower().startswith("android") for r in a["dims"]["os_version"]))
    check("event-types follows the filter", {t["type"] for t in g("event-types", "iOS")} == {"Ios.only", "Shared.event", "TelemetryDeck.Error.occurred"})
    ua = g("users", "Android"); check("users: new and active follow the filter", sum(x["users"] for x in ua["new"]) == 3 and sum(x["users"] for x in ua["active"]) == 3, str(ua))
    check("versions follow the filter", {r["v"] for r in g("versions", "Android")} == {"7.0", "7.1"} and {r["v"] for r in g("versions", "iOS")} == {"3.0"})
    check("errors follow the filter", g("errors", "Android")["top"][0]["events"] == 2 and g("errors", "iOS")["top"][0]["events"] == 1 and g("errors")["top"][0]["events"] == 3)
    ra, ri = g("retention", "Android"), g("retention", "iOS"); check("retention follows the filter", sum(x["users"] for x in ra["rows"] if x["n"] == 0) == 3 and sum(x["users"] for x in ri["rows"] if x["n"] == 0) == 2, str(ra["rows"]))
    ev = req("GET", f"/api/apps/{pa}/events?limit=50&platform=Android")[1]; check("events follow the filter", ev and all(e["os"] == "Android" for e in ev) and len(ev) == 6, str(len(ev)))
    check("an unknown platform gives empty results", g("stats", "Symbian")["totals"]["users"] == 0)
    evil = "x' OR '1'='1"; check("a hostile platform value is only data (no SQL injection)", g("stats", evil)["totals"]["users"] == 0 and g("stats")["totals"]["users"] == 5)
    req("DELETE", f"/api/apps/{pa}")

    print("== read API (read keys, /r/v1)")
    def rd(path, k, method="GET", extra=None):   # no session cookie: only the read key authenticates
        c = http.client.HTTPConnection(u.hostname, u.port, timeout=20)
        h = dict(extra or {})
        if k: h["Authorization"] = f"Bearer {k}"
        if args.host: h["Host"] = args.host
        c.request(method, path, None, h); r = c.getresponse(); b = r.read()
        try: j = json.loads(b)
        except Exception: j = None
        return r.status, j, b, r
    ra_app = req("POST", "/api/apps", {"name": "ReadA"})[1]["id"]; rb_app = req("POST", "/api/apps", {"name": "ReadB"})[1]["id"]
    wkey = req("POST", f"/api/apps/{ra_app}/keys", {"label": "w"})[1]["key"]; wkey_b = req("POST", f"/api/apps/{rb_app}/keys", {"label": "wb"})[1]["key"]
    s, j, _, _ = req("POST", f"/api/apps/{ra_app}/keys", {"label": "agent", "kind": "read"}); rkey = j["key"]
    check("issue a read key (tlr_ prefix)", s == 200 and rkey.startswith("tlr_"))
    check("an unknown key kind is rejected (400)", req("POST", f"/api/apps/{ra_app}/keys", {"label": "x", "kind": "admin"})[0] == 400)
    kl = req("GET", f"/api/apps/{ra_app}/keys")[1]
    check("key list tells the kinds apart and never exposes keys", {k["kind"] for k in kl} == {"write", "read"} and all("key" not in k for k in kl))
    rkey_b = req("POST", f"/api/apps/{rb_app}/keys", {"label": "agent-b", "kind": "read"})[1]["key"]

    def rtd(user, typ, plan=None, os_name="iOS", test="false", k=None):
        pl = {"TelemetryDeck.Device.operatingSystem": os_name, "TelemetryDeck.AppInfo.version": "5.0"}
        if plan: pl["plan"] = plan
        return req("POST", f"/k/{k or wkey}/v2/", [{"receivedAt": now, "clientUser": user, "sessionID": "s-" + user, "type": typ, "payload": pl, "isTestMode": test}])[0]
    rtd("r1", "Purchase.completed", "yearly"); rtd("r2", "Purchase.completed", "yearly"); rtd("r3", "Purchase.completed", "monthly"); rtd("r4", "Purchase.completed", "yearly", "Android")
    for _ in range(5): rtd("r1", "Page.view")
    rtd("r9", "Debug.event", test="true"); rtd("b1", "Other.app", k=wkey_b)
    time.sleep(1.0)

    print("-- authentication and isolation")
    s, j, _, _ = rd("/r/v1/", None); check("no key is 401 missing_key with a hint", s == 401 and j["error"] == "missing_key" and "Bearer" in j["hint"], str(j))
    s, j, _, _ = rd("/r/v1/", "tlr_wrong"); check("wrong key is 401 invalid_key", s == 401 and j["error"] == "invalid_key")
    s, j, _, _ = rd("/r/v1/", wkey); check("a write key cannot read (403 wrong_key_type)", s == 403 and j["error"] == "wrong_key_type" and "tlr_" in j["hint"])
    check("a read key cannot write: TelemetryDeck path (403)", td([{"clientUser": "x", "type": "Hack"}], rkey) == 403)
    check("a read key cannot write: OTLP bearer (403)", req("POST", "/v1/logs", {"resourceLogs": []}, {"Authorization": f"Bearer {rkey}"})[0] == 403)
    check("a read key does not open the admin API (401)", rd("/api/apps", rkey)[0] == 401 and rd(f"/api/apps/{ra_app}/keys", rkey)[0] == 401)
    check("x-api-key header works too", rd("/r/v1/", None, extra={"x-api-key": rkey})[0] == 200)
    check("POST is not allowed on the read API (405)", rd("/r/v1/overview", rkey, "POST")[0] == 405)

    print("-- describe")
    s, d, _, _ = rd("/r/v1/", rkey); check("GET /r/v1/ is 200", s == 200 and d["service"] == "TelemetryLite")
    check("describe: the app comes from the key", d["app"]["id"] == ra_app and d["app"]["name"] == "ReadA" and d["app"]["retention_days"] == 90)
    check("describe lists event types of live data only", {t["type"] for t in d["event_types"]["rows"]} == {"Purchase.completed", "Page.view"}, str(d["event_types"]))
    check("describe lists platforms and custom parameters", {p["platform"] for p in d["platforms"]} == {"iOS", "Android"} and "plan" in {p["key"] for p in d["custom_parameters"]["rows"]})
    check("describe documents all endpoints with parameters", {e["path"] for e in d["endpoints"]} == {"/r/v1/", "/r/v1/overview", "/r/v1/events", "/r/v1/query"} and "group_by" in next(e for e in d["endpoints"] if e["path"].endswith("query"))["params"])
    check("describe carries the guidance for agents (so the prompt can stay short)", len(d["guide_for_agents"]) >= 3 and "overview" in d["guide_for_agents"][0])
    check("describe: /r/v1 without the slash works", rd("/r/v1", rkey)[0] == 200)
    check("describe: first/last event are ISO timestamps", d["data"]["last_event_at"].endswith("Z") and d["data"]["first_event_at"].endswith("Z"))

    print("-- overview")
    s, o, _, _ = rd("/r/v1/overview?days=7", rkey); check("overview is 200", s == 200)
    check("overview totals: live events and users (test data hidden)", o["totals"]["events"]["value"] == 9 and o["totals"]["users"]["value"] == 4 and o["totals"]["sessions"]["value"] == 4, str(o["totals"]))
    check("overview: previous period and change", o["totals"]["events"]["previous"] == 0 and o["totals"]["events"]["change_pct"] is None and o["totals"]["new_users"] == 4)
    check("overview: meta states the window and data kind", o["meta"]["data"] == "live" and o["meta"]["app_id"] == ra_app and o["meta"]["window"]["since"].endswith("Z"))
    check("overview: top events and series", o["top_events"][0]["type"] == "Page.view" and o["top_events"][0]["events"] == 5 and sum(x["events"] for x in o["series"]) == 9)
    check("overview: breakdowns carry a share", {r["value"] for r in o["breakdowns"]["app_version"]} == {"5.0"} and o["breakdowns"]["app_version"][0]["share"] == 1.0)
    check("overview: retention cohorts are present", o["retention"]["basis"] == "live" and o["retention"]["cohorts"] and o["retention"]["cohorts"][-1]["users"] == 4, str(o["retention"]))
    check("overview: errors section exists", isinstance(o["errors"], list))
    check("overview?test=all includes test data", rd("/r/v1/overview?test=all", rkey)[1]["totals"]["events"]["value"] == 10)
    check("overview?test=1 is test data only", rd("/r/v1/overview?test=1", rkey)[1]["totals"]["events"]["value"] == 1)
    ov = rd("/r/v1/overview?platform=android", rkey)[1]; check("overview?platform= filters (case-insensitive)", ov["totals"]["events"]["value"] == 1 and ov["totals"]["users"]["value"] == 1)
    check("overview?days=1 buckets by hour", rd("/r/v1/overview?days=1", rkey)[1]["meta"]["series_bucket"] == "hour")
    check("overview?tz shifts the buckets", rd("/r/v1/overview?days=3&tz=480", rkey)[1]["meta"]["tz_minutes"] == 480)
    check("overview with a window in the past is empty", rd("/r/v1/overview?since=2020-01-01&until=2020-02-01", rkey)[1]["totals"]["events"]["value"] == 0)

    print("-- events")
    s, e, _, _ = rd("/r/v1/events?type=Purchase.completed", rkey); rows = e["rows"]
    check("events: filter by type", s == 200 and len(rows) == 4 and all(r["type"] == "Purchase.completed" for r in rows))
    r0 = rows[0]
    check("events: ISO time, short user, params as an object, no null fields", r0["time"].endswith("Z") and len(r0["user"]) == 12 and isinstance(r0["params"], dict) and "plan" in r0["params"] and all(v is not None for v in r0.values()), str(r0))
    check("events: where=payload.plan:yearly", len(rd("/r/v1/events?type=Purchase.completed&where=payload.plan:yearly", rkey)[1]["rows"]) == 3)
    check("events: where on a dimension and two where= are ANDed", len(rd("/r/v1/events?where=platform:Android&where=payload.plan:yearly", rkey)[1]["rows"]) == 1)
    p1 = rd("/r/v1/events?type=Page.view&limit=2", rkey)[1]
    check("events: limit truncates and gives next_cursor", len(p1["rows"]) == 2 and p1["meta"]["truncated"] is True and p1["next_cursor"])
    p2 = rd(f"/r/v1/events?type=Page.view&limit=2&cursor={p1['next_cursor']}", rkey)[1]
    check("events: cursor pages without overlap", len(p2["rows"]) == 2 and not ({r["id"] for r in p1["rows"]} & {r["id"] for r in p2["rows"]}))
    p3 = rd(f"/r/v1/events?type=Page.view&limit=2&cursor={p2['next_cursor']}", rkey)[1]
    check("events: the last page is not truncated and has no cursor", len(p3["rows"]) == 1 and p3["meta"]["truncated"] is False and p3["next_cursor"] is None)
    check("events: test data is hidden by default and shown with test=1", all(r["type"] != "Debug.event" for r in rd("/r/v1/events?limit=500", rkey)[1]["rows"]) and rd("/r/v1/events?test=1", rkey)[1]["rows"][0]["test"] is True)
    s, _, b, r = rd("/r/v1/events?type=Purchase.completed&format=csv", rkey)
    check("events: format=csv", s == 200 and "text/csv" in r.getheader("content-type") and b.decode().splitlines()[0].startswith("id,") and len(b.decode().splitlines()) == 5)
    check("events: limit above the maximum is rejected (400)", rd("/r/v1/events?limit=9999", rkey)[0] == 400)

    print("-- query")
    s, qd, _, _ = rd("/r/v1/query?type=Purchase.completed&metric=events,users&group_by=payload.plan", rkey)
    got = {x["payload.plan"]: (x["events"], x["users"]) for x in qd["rows"]}
    check("query: group by a custom parameter with two metrics", s == 200 and got == {"yearly": (3, 3), "monthly": (1, 1)}, str(qd))
    check("query: default order is by the first metric, descending", qd["rows"][0]["payload.plan"] == "yearly")
    check("query: meta echoes the query", qd["meta"]["query"]["metric"] == ["events", "users"] and qd["meta"]["query"]["group_by"] == ["payload.plan"])
    qd = rd("/r/v1/query?metric=users&group_by=platform", rkey)[1]; check("query: group_by=platform", {x["platform"]: x["users"] for x in qd["rows"]} == {"iOS": 3, "Android": 1}, str(qd))
    qd = rd("/r/v1/query?interval=day&metric=events", rkey)[1]; check("query: interval=day adds t and sums to the total", sum(x["events"] for x in qd["rows"]) == 9 and all("t" in x for x in qd["rows"]))
    qd = rd("/r/v1/query?interval=week&group_by=type&days=14", rkey)[1]; check("query: interval=week with a group", len(qd["rows"]) == 2 and len(qd["rows"][0]["t"]) == 10, str(qd["rows"]))
    check("query: interval=month", len(rd("/r/v1/query?interval=month", rkey)[1]["rows"][0]["t"]) == 7)
    qd = rd("/r/v1/query", rkey)[1]; check("query: no grouping gives one total row", qd["rows"] == [{"events": 9}], str(qd["rows"]))
    qd = rd("/r/v1/query?group_by=type&limit=1", rkey)[1]; check("query: limit truncates and says so", len(qd["rows"]) == 1 and qd["meta"]["truncated"] is True and "truncated_hint" in qd["meta"])
    qd = rd("/r/v1/query?group_by=payload.plan&where=type:Purchase.completed&where=platform:iOS&order=key", rkey)[1]; check("query: where + order=key", [x["payload.plan"] for x in qd["rows"]] == ["monthly", "yearly"], str(qd["rows"]))
    qd = rd("/r/v1/query?group_by=payload.missing&metric=sessions", rkey)[1]; check("query: a missing parameter groups as (none)", qd["rows"] == [{"payload.missing": "(none)", "sessions": 4}], str(qd["rows"]))
    s, _, b, _ = rd("/r/v1/query?group_by=type&format=md", rkey)
    check("query: format=md renders a table", s == 200 and b.decode().startswith("| type | events |\n| --- | --- |\n"), b.decode()[:80])
    check("query: a hostile where value is only data", rd("/r/v1/query?where=type:" + urllib.parse.quote("x' OR '1'='1"), rkey)[1]["rows"] == [{"events": 0}])
    for path, code, why in [("/r/v1/query?metric=revenue", "bad_metric", "unknown metric"), ("/r/v1/query?group_by=nope", "unknown_field", "unknown field"),
                            ("/r/v1/query?group_by=type,locale,country,city", "too_many_groups", "more than 3 groups"), ("/r/v1/query?interval=year", "bad_interval", "unknown interval"),
                            ("/r/v1/query?where=oops", "bad_where", "malformed where"), ("/r/v1/query?since=yesterday", "bad_time", "unparsable time"),
                            ("/r/v1/query?since=2026-02-01&until=2026-01-01", "bad_window", "empty window"), ("/r/v1/query?test=maybe", "bad_parameter", "bad test value"),
                            ("/r/v1/query?days=abc", "bad_parameter", "non-numeric days"), ("/r/v1/query?metirc=users", "unknown_parameter", "a typo in a parameter name"),
                            ("/r/v1/query?group_by=payload.a%22b", "bad_field", "a quote in a payload key"), ("/r/v1/overview?format=csv", "bad_format", "csv on overview"), ("/r/v1/?x=1", "unknown_parameter", "any parameter on describe")]:
        s, j, _, _ = rd(path, rkey); check(f"error: {why} -> 400 {code} with a hint", s == 400 and j["error"] == code and j["message"] and j["hint"], str(j))
    check("unknown_parameter lists the valid ones", "metric" in rd("/r/v1/query?metirc=users", rkey)[1]["hint"])
    check("SQL in a field name is rejected, not executed", rd("/r/v1/query?group_by=" + urllib.parse.quote("type; DROP TABLE signal"), rkey)[1]["error"] == "unknown_field" and rd("/r/v1/overview", rkey)[1]["totals"]["events"]["value"] == 9)

    print("-- isolation, revocation, rate limit")
    check("another app's read key sees only its own data", rd("/r/v1/", rkey_b)[1]["app"]["id"] == rb_app and rd("/r/v1/overview", rkey_b)[1]["totals"]["events"]["value"] == 1)
    check("read keys do not leak between apps (types)", {t["type"] for t in rd("/r/v1/", rkey_b)[1]["event_types"]["rows"]} == {"Other.app"})
    kid = next(k["id"] for k in req("GET", f"/api/apps/{ra_app}/keys")[1] if k["kind"] == "read" and k["prefix"] == rkey[:10])
    check("revoke the read key", req("DELETE", f"/api/keys/{kid}")[0] == 200)
    check("a revoked read key stops working immediately (401)", rd("/r/v1/", rkey)[0] == 401)
    check("the write key of the same app still works", rtd("r1", "After.revoke") == 200)
    tkey = req("POST", f"/api/apps/{ra_app}/keys", {"label": "burst", "kind": "read"})[1]["key"]
    codes = [rd("/r/v1/", tkey)[0] for _ in range(125)]
    s, j, _, r = rd("/r/v1/", tkey)
    check("rate limit: 120 requests per minute per key, then 429 with Retry-After", codes[:120] == [200] * 120 and s == 429 and j["error"] == "rate_limited" and int(r.getheader("retry-after")) >= 1, str(codes[118:]))
    check("rate limit is per key", rd("/r/v1/", rkey_b)[0] == 200)
    req("DELETE", f"/api/apps/{ra_app}"); req("DELETE", f"/api/apps/{rb_app}")

    if GEO:
        print("== GeoIP")
        app = req("POST", "/api/apps", {"name": "Geo"})[1]["id"]; key = req("POST", f"/api/apps/{app}/keys", {"label": "geo"})[1]["key"]
        def geo(ip, hn="x-forwarded-for", t="g"):
            td([{"clientUser": f"{t}-{ip}", "sessionID": "g", "type": f"geo-{t}-{ip}", "payload": {}}], extra={hn: ip}); time.sleep(0.4)
            e = req("GET", f"/api/apps/{app}/events?limit=50&type=" + urllib.parse.quote(f"geo-{t}-{ip}"))[1]; return (e[0]["country"], e[0]["city"]) if e else None
        for ip, want in [("8.8.8.8", ("US", "Mountain View")), ("114.114.114.114", ("CN", "Jinan")), ("1.1.1.1", ("AU", "Sydney")), ("212.58.244.20", ("GB", "London")),
                         ("185.199.108.153", ("US", "San Francisco")), ("2606:4700:4700::1111", ("US", "San Francisco"))]:
            got = geo(ip); check(f"{ip} -> {want[0]}/{want[1]}", got == want, str(got))
        check("private IP gives no location", geo("10.0.0.5") == (None, None)); check("garbage header gives no location", geo("not-an-ip") == (None, None))
        check("first address of an X-Forwarded-For chain wins", (geo("1.2.4.8, 8.8.8.8") or [None])[0] == "CN")
        check("X-Real-IP is used", (geo("114.114.114.114", "x-real-ip", "r") or [None])[0] == "CN")
        req("DELETE", f"/api/apps/{app}")
    if NGINX:
        print("== behind nginx")
        app = req("POST", "/api/apps", {"name": "Nginx"})[1]["id"]; key = req("POST", f"/api/apps/{app}/keys", {"label": "nginx"})[1]["key"]
        td([{"clientUser": "spoof", "sessionID": "sp", "type": "Spoof.xff", "payload": {}}], extra={"x-forwarded-for": "8.8.8.8"}); time.sleep(0.5)
        e = req("GET", f"/api/apps/{app}/events?type=Spoof.xff")[1][0]; check("a spoofed X-Forwarded-For is overwritten by nginx (no country)", e["country"] is None)
        codes = [req("POST", "/api/login", {"password": "bad"})[0] for _ in range(15)]
        check("login is rate limited (503 after a few bad attempts)", 503 in codes and codes[0] == 401, str(codes))

try:
    run()
finally:
    if proc:
        proc.terminate()
        try: proc.wait(timeout=10)
        except Exception: proc.kill()
    if tmp: tmp.cleanup()
print(f"\nresult: {ok} passed, {bad} failed")
sys.exit(1 if bad else 0)
