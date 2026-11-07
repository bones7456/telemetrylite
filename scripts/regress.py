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
import argparse, gzip, http.client, json, os, socket, sqlite3, subprocess, sys, tempfile, time, urllib.parse, urllib.request

ap = argparse.ArgumentParser()
ap.add_argument("--spawn"); ap.add_argument("--base"); ap.add_argument("--password", default="regress-pw")
ap.add_argument("--host"); ap.add_argument("--geoip-db"); ap.add_argument("--geo", action="store_true"); ap.add_argument("--nginx", action="store_true")
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
