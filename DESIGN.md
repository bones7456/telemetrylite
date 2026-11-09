# TelemetryLite — design

English | [简体中文](DESIGN.zh-CN.md)

> Status: implemented and released (see the Releases page). This document describes the system **as built**; the last section lists what is deliberately not implemented yet.
> Goal: a lightweight event-collection service — one Rust binary plus SQLite — for iOS / Android and similar apps, compatible with both **TelemetryDeck SDKs** and **OTLP/HTTP Logs**.

## 1. Decisions

| Topic | Decision |
|---|---|
| OTLP scope | **Logs only**; a LogRecord is an "event". Metrics and traces are accepted and discarded (§4.2) |
| Internal data model | A TelemetryDeck-style **signal** wide table; OTLP LogRecords are mapped into it |
| Accounts | One admin, many apps; each app can issue several write keys and several read keys |
| Scale | Self-hosted single machine, up to about 1 M events per day |
| Privacy | Never store IPs; GeoIP is resolved in memory and only country / region / city are kept |
| Onboarding | **AI-assisted integration**: the UI generates a per-app prompt (server URL, app id, key filled in) that a coding agent follows to integrate the SDK |
| UI languages | English (default) and Chinese, switchable in the page; docs and CLI output are bilingual too |

## 2. Architecture

```
 iOS / Android app                              browser
 ├─ TelemetryDeck SDK ──POST /k/<key>/v2/──┐      │
 └─ OTel SDK ──POST /v1/logs ──────────────┤      │ /api/*  (cookie session)
                                           ▼      ▼
                    ┌────────────────────────────────────────────┐
                    │  axum HTTP server (single process)          │
                    │  ingest: auth → decode → normalize → limits │
                    │        │                                    │
                    │        ▼  bounded mpsc channel              │
                    │  writer thread: batches into one transaction│
                    │        │                                    │
                    │  SQLite (WAL) ◄── admin API (own connection)│
                    │  background: retention purge, GeoIP update  │
                    │  static page: web/index.html (include_str!) │
                    └────────────────────────────────────────────┘
```

- **Single writer**: every write goes through one thread that drains the queue into one transaction, so there is no write-lock contention. Admin/query code uses a separate connection (WAL lets readers run alongside the writer).
- **Bounded queue + backpressure**: when the queue is full the server answers `503 + Retry-After`; SDKs retry, memory stays bounded.
- **Everything embedded**: the UI is one HTML file embedded with `include_str!`, so deployment is one file.
- **Hot path never touches SQLite for auth**: write keys live in an in-memory map (SHA-256 → app id) refreshed whenever keys change.

## 3. Data model (SQLite)

```sql
CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);      -- salt, admin password hash
CREATE TABLE app (
  id TEXT PRIMARY KEY,                -- UUID (the TelemetryDeck "appID")
  name TEXT NOT NULL, created_at INTEGER NOT NULL,
  retention_days INTEGER NOT NULL DEFAULT 90
);
CREATE TABLE api_key (
  id TEXT PRIMARY KEY, app_id TEXT NOT NULL REFERENCES app(id) ON DELETE CASCADE,
  label TEXT NOT NULL, prefix TEXT NOT NULL,   -- first 10 chars, for display
  key_hash TEXT NOT NULL UNIQUE,               -- hex SHA-256; keys are high-entropy, no argon2 needed
  created_at INTEGER NOT NULL, revoked_at INTEGER
);
CREATE TABLE signal (
  id INTEGER PRIMARY KEY,
  app_id TEXT NOT NULL, ts INTEGER NOT NULL, received_at INTEGER NOT NULL,   -- unix ms
  type TEXT NOT NULL, user_hash TEXT, session_id TEXT, float_value REAL,
  is_test INTEGER NOT NULL DEFAULT 0,
  app_version TEXT, build TEXT, os TEXT, os_version TEXT, device_model TEXT,  -- common dimensions as columns
  locale TEXT, sdk TEXT,
  country TEXT, region TEXT, city TEXT,                                       -- from GeoIP, NULL when disabled
  payload TEXT NOT NULL DEFAULT '{}'                                           -- remaining parameters, JSON
);
CREATE INDEX signal_app_ts   ON signal(app_id, ts);
CREATE INDEX signal_app_type ON signal(app_id, type, ts);
```

- Common dimensions are real columns because charts group by them constantly; everything else stays in `payload` and can be queried with SQLite JSON functions.
- Pragmas: `journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`.
- Retention: every hour, expired rows are deleted in batches of 10,000 per app retention setting.
- **Schema versioning**: the schema version lives in `PRAGMA user_version` (1 = every release so far; databases from before versioning have 0 and are accepted). To change the schema, bump `SCHEMA_VERSION` in `db.rs` and append a step to `MIGRATIONS`; pending steps run on start, each in a transaction. A database with a *newer* version than the binary knows is refused with a clear error, so an older binary (for example after a rollback) never touches a schema it does not understand.
- Dashboard queries scan the indexed raw table directly (no pre-aggregation yet — see §10).

## 4. Ingest

### 4.1 TelemetryDeck: `POST /k/<key>/v2/`

Verified against the Swift SDK source: the SDK posts to `{baseURL}/v2/` (or `/v2/namespace/{ns}/`) with only a `Content-Type` header — **there is no auth header**, so the write key travels in the base URL path (`https://host/k/<key>/`).

- Body: array of signals with `appID`, `clientUser`, `sessionID`, `type`, `payload` (string dictionary), `floatValue`, `isTestMode` (a string `"true"/"false"`; booleans are accepted too), `receivedAt` (`yyyy-MM-dd'T'HH:mm:ssZ`, offset like `+0000`; RFC 3339 is accepted too).
- The app is determined by the key; the body's `appID` is ignored.
- New SDKs send every parameter under two names (`TelemetryDeck.AppInfo.version` and the deprecated `appVersion`). The new name wins, the old name is a fallback, and duplicates are removed from the payload.
- **System-noise filtering**: a recent SDK attaches about 50 `TelemetryDeck.*` system parameters to every signal (calendar, accessibility, screen metrics, retention counters, …), about 1.4 KB per event. About 30 of them are dropped on ingest (payload shrinks to about 350 bytes). Kept: dimensions (version, build, OS, device, locale, SDK) as columns; `RunContext.isTestFlight/isAppStore/isDebug/isSimulator`, `UserPreference.language/region`, `Device.timeZone`; and every parameter of the SDK's preset events (purchase, error, navigation, …) plus your own parameters. The filter runs before the 50-key cap so custom parameters can never be crowded out.

### 4.2 OTLP: `POST /v1/logs`

- **OTLP/HTTP** with `application/x-protobuf` or `application/json`, gzip request bodies supported. No gRPC (OTLP/HTTP is the common choice on mobile and avoids tonic).
- Auth: `Authorization: Bearer <key>`, `x-api-key`, or the path `/k/<key>/v1/logs`.
- Only the needed subset of the Logs messages is declared with `prost` (no protoc, no full `opentelemetry-proto`).
- Mapping:

| OTLP | signal |
|---|---|
| `LogRecord.event_name` (else attribute `event.name`, else string body) | `type` |
| `time_unix_nano` (else `observed_time_unix_nano`) | `ts` |
| attribute `user.id` / `enduser.id` (salted and hashed, never stored raw) | `user_hash` |
| attribute `session.id` | `session_id` |
| resource `service.version` / `os.name` / `os.version` / `device.model.*` / `telemetry.sdk.name` | dimension columns |
| other attributes | `payload` |

- `/v1/metrics` and `/v1/traces` return `200` and discard, so clients do not retry.

### 4.3 Common behavior

- Body limit 2 MB (the decompressed size is capped at 8 MB); at most 1000 signals per request; at most 50 payload keys per signal; values are cut at 1 KB.
- A timestamp more than 7 days away from now is treated as a bad client clock and replaced by the receive time.
- `user_hash = SHA-256(server salt + app id + client user)`; the salt is generated on first start and stored in `meta`.
- Never stores IPs (§4.4).

### 4.4 GeoIP (country / region / city)

- **Database: DB-IP "IP to City Lite" (mmdb).** No account or license key needed, monthly updates, CC BY 4.0 (attribution only), read directly by the `maxminddb` crate through a memory map (idle RSS stays around 10 MB). MaxMind GeoLite2 was rejected because it needs an account and a license key and its EULA imposes update obligations.
- Size: about 127 MB unpacked. To save space point `TL_GEOIP_DB` at the Country Lite file instead; the code reads whichever of country/region/city exists.
- Default: without `TL_GEOIP_DB` or auto update, no lookup happens and the three columns are NULL.
- Auto update (`TL_GEOIP_AUTO_UPDATE=true`): download on start if missing, then check daily and refresh when older than 30 days. The file is downloaded to a temp name, verified to open, atomically renamed and hot-reloaded. Failures are logged as warnings and never affect ingest.
- Client IP: the TCP peer, except that `X-Forwarded-For` / `X-Real-IP` are trusted only when the peer is loopback or in `TL_TRUSTED_PROXY`. Private and loopback addresses are skipped. Location attributes sent by clients are ignored. DB-IP city names often carry a district suffix (`San Francisco (South Beach)`); it is stripped on ingest, and the city breakdown strips it for older rows too, so one city is one entry.
- Attribution: the admin UI footer and the README credit DB-IP.com.

## 5. Keys and authentication

**Write keys (for SDKs)**: `tl_<32 random bytes, base64url>`; shown once at creation, only the SHA-256 is stored. Several per app (each needs a name; the UI suggests the current time), revocable at any time, write-only for that app. Authentication is a lookup in the in-memory map.

**Read keys (for analysis tools and AI agents)**: same table (`api_key.kind = 'read'`, schema version 2; existing keys are migrated as `write`), prefix `tlr_`, bound to one app, shown once, revocable. The in-memory map is `hash → (app id, is_read)`: the ingest endpoints refuse a read key (`403`) and `/r/v1/*` refuses a write key (`403 wrong_key_type`), so neither kind can do the other's job. Read keys are sent as `Authorization: Bearer` (never in the path) and cannot reach `/api/*`.

### 5.1 Read API (`/r/v1`)

`GET /r/v1/` (self-description), `/overview` (totals with change versus the previous period, trend, top events, breakdowns, errors, weekly retention), `/events` (raw events, `where=field:value`, cursor paging), `/query` (metric × up to 3 group-by fields × interval). Full reference: [docs/READ_API.md](docs/READ_API.md).

- Built for agents: self-describing, a `meta` block on every response, unknown parameters rejected, errors as `{error, message, hint}`, ISO 8601 timestamps, optional `csv` / `md` output, nothing truncated silently (`truncated`, `next_cursor`).
- Safe by construction: no raw SQL; fields are whitelisted, `payload.<key>` is bound as a JSON path parameter, filter values are bound parameters.
- Protection of the write path: queries run on their own connection with `PRAGMA query_only`, at most 4 at a time, cancelled after 5 s via SQLite's interrupt handle; 120 requests per minute per key (in memory).

**Admin**: one admin. The password is generated on first start and logged once, or set with `TL_ADMIN_PASSWORD` (applied on every start, which is also the way to reset it); stored as an argon2 hash. A login sets an opaque random token in an `HttpOnly; SameSite=Strict` cookie (`Secure` when `X-Forwarded-Proto` is https); sessions live in memory for 30 days and are lost on restart. Failed logins are delayed 0.5 s and rate-limited by nginx. TLS terminates at nginx (§9).

## 6. Admin UI

One HTML file, plain JavaScript, inline SVG charts, light/dark by system setting.

- **Login**, **apps** list and creation.
- **Platform filter**: when an app has data from more than one platform (the `os` column: `iOS`, `Android`, … — Swift SDK sends `iOS`/`macOS`/…, Kotlin SDK sends `Android`, OTLP uses `os.name`; letter case is normalized on ingest), a *Platform* dropdown appears in the toolbar of every data page and of the Events page. Every statistics endpoint takes an optional `platform` parameter (bound as a SQL parameter, never spliced into the query); `/api/apps/{id}/platforms` lists the platforms of the last 90 days. The choice is remembered for the browser session. The header numbers always cover the whole app. One app with a filter is the recommended setup for iOS + Android; use separate apps only if you want fully separate keys, retention periods and dashboards.
- **Header**: an app switcher, and three headline numbers over the last 90 days of live data: users, most used SDK, errors.
- **Overview**: range (24 h / 7 d / 30 d / 90 d), live vs test data (a banner says how many events of the other kind are hidden, with a one-click switch), tiles (users in the last hour, active users, events, sessions), active-users line chart and events bar chart, **weekly retention cohorts** (last 8 weeks, weeks start on Monday), **app versions over time** (share of users, top 8 versions), top events with their user counts, and breakdowns by app version, OS version, device model, locale, country (shown by name in the UI language, via the browser's `Intl.DisplayNames`) and city. Charts have grid lines and a hover card like TelemetryDeck's (bold date, then one row per series with a colour dot, name and bold value; stacked charts show each series' share to two decimals and **focus the series under the pointer**: it keeps its colour in every column while the others fade, and the matching legend item and card row are emphasized; the hovered column is highlighted); the retention heatmap has its own hover card (retention to two decimals and the week, when the cohort first appeared, and the user count); missing time buckets are filled with zero and bucketed in the browser's time zone.
- **Users**: new users per day and new vs returning users. A user is new on the day their first event appears in the stored data (so after a retention purge, long-gone users can show up as new again).
- **Event types**: every event type with users, events and last-seen time; a name opens the Events page filtered to it.
- **Errors**: trend and a table grouped by error id and category, from the SDK's preset `TelemetryDeck.Error.occurred` event.
- **Events**: recent raw events, exact event-name filter, custom parameters inline, `TelemetryDeck.*` system parameters collapsed.
- **Settings**: rename, retention days, read-key management and an **analysis prompt** for AI agents (server URL and read key filled in; see §5.1), write-key management (copy-once keys with ready-made integration snippets, revoke), an **AI-agent integration prompt** (filled in with the browser's server URL, the app id and — once issued — the write key, in the UI language; the text lives in `web/index.html` and must be kept in sync with `docs/AGENT_PROMPT*.md`), delete app.
- **Refresh**: a manual Refresh button (default) and an optional auto refresh (10 s / 30 s / 1 min / 5 min, remembered in the browser). Both only re-fetch data and update the view in place; auto refresh pauses in background tabs, never overlaps requests, and the events page leaves the DOM alone when there is nothing new, so expanded parameters stay open.
- **Languages**: English by default, Chinese selectable from the header; the choice is remembered in `localStorage`.

## 7. Code layout

```
src/main.rs      startup, environment configuration, routing, graceful shutdown, retention job
src/db.rs        schema, writer thread, retention purge
src/ingest.rs    TelemetryDeck and OTLP handlers, normalization, limits
src/admin.rs     /api/*: sessions, apps, keys, and the stats / users / retention / versions / errors / event queries
src/read.rs      /r/v1/*: read-key auth, rate limit, describe / overview / events / query
src/geo.rs       mmdb loading (hot reload), download and update
src/i18n.rs      log language (English / Chinese)
web/index.html   the whole admin UI
scripts/         install.sh (server installer), deploy.sh (from source), release.sh (local cross build), regress.py (black-box regression test)
.github/workflows/ci.yml   tests on every push; release builds on v* tags
```

Main dependencies: `axum`, `tokio`, `rusqlite` (bundled), `prost`, `serde_json`, `flate2`, `argon2`, `sha2`, `maxminddb`, `ureq`, `tracing`. Release binary about 5 MB (strip + LTO), static musl.

## 8. Configuration (environment variables)

`TL_LISTEN` (default `127.0.0.1:8888`), `TL_DB_PATH` (`./telemetrylite.db`), `TL_ADMIN_PASSWORD`, `TL_GEOIP_DB`, `TL_GEOIP_AUTO_UPDATE` (default false), `TL_TRUSTED_PROXY` (extra trusted proxy IPs, no CIDR), `TL_LANG` (`zh` for Chinese log messages).

## 9. Deployment and release

- Put nginx in front: it terminates HTTPS, limits the body size, rate-limits `/api/login`, and passes the real client IP in `X-Forwarded-For` (overwriting, never appending). The service listens on `127.0.0.1` only.
- **`scripts/install.sh`** (served from the GitHub Release, run with `curl | sh`): detects OS, CPU (x86_64 / aarch64), privileges, downloader, systemd, port use and reverse proxy, prints a plan, verifies the binary against `SHA256SUMS`, then installs, upgrades or uninstalls. With `--domain` it writes an nginx config and requests a Let's Encrypt certificate. Output is bilingual (English default, Chinese by locale or `--lang zh`). `--dry-run` changes nothing.
- **Updates**: `install.sh` is idempotent. If a binary or service file exists it is an update: it compares the installed binary's SHA-256 with the release's `SHA256SUMS` (equal = nothing to do, `--force` overrides), keeps the old binary as `telemetrylite.prev`, restarts, waits for the service to answer, and **rolls back** to `.prev` if it does not. Existing settings (port, GeoIP, language) are read back from the service file; a service file that differs from what the installer would generate is kept and reported (`--rewrite-unit` regenerates it). Without systemd only the binary is replaced.
- **CI** (`.github/workflows/ci.yml`): `test` (unit tests) and `regress` (shellcheck, `install.sh --dry-run`, a no-LTO `ci`-profile build and `scripts/regress.py --spawn`, which starts the real binary and checks ingest, statistics, privacy — including that no raw IP reaches the database — keys, the platform filter and the admin API) run in parallel. On every push to `main` and on tags, `build` also cross-compiles the x86_64 and aarch64 musl binaries in parallel with `cargo-zigbuild` (so the release path is always tested and the build cache stays warm) and **runs the regression test on the x86_64 artifact**. Only a `v*` tag runs `publish`, and only when `test`, `regress` and both builds are green: it creates the Release with the binaries, `SHA256SUMS` and `install.sh` (repository name filled in). The tag must equal the version in `Cargo.toml` (the single source of truth for the version) or the build fails. Caches (`rust-cache`) and cancelling superseded runs keep it fast.
- **From source**: `scripts/deploy.sh user@host [domain]` cross-compiles locally and runs `install.sh` remotely.

## 10. Not implemented yet / deliberately skipped

- Pre-aggregated daily statistics (queries scan the indexed raw table; fine for the intended scale, add a roll-up table if it gets slow).
- Per-key rate limiting on the write path (backpressure plus nginx `limit_req` cover it; the read API has its own limit), `last_used_at` on keys.
- A raw read-only SQL endpoint for agents (`/r/v1/query` covers aggregation by any field and custom parameter; add SQL only if that proves too limiting).
- Custom queries / insights (a query builder like TelemetryDeck's Playground), funnels, export, user-defined dashboards.
- Changing the admin password in the UI (restart with a new `TL_ADMIN_PASSWORD`), persistent sessions, multiple users and roles.
- CIDR support in `TL_TRUSTED_PROXY`; Metrics and Traces; gRPC; alerts; high availability.
