# TelemetryLite

English | [简体中文](README.zh-CN.md)

A lightweight event-collection server: a single Rust binary plus SQLite. It accepts events from the **TelemetryDeck SDKs** and from **OTLP/HTTP Logs**, and ships with an admin UI, write-only API keys for apps and read-only keys for analysis. The UI is available in English and Chinese.

> **Integration by AI, in one paste.** The admin UI generates a ready-made prompt with your server URL, app id and write key already filled in. Copy it into Claude Code, Codex or Cursor, and your coding agent adds the SDK, points it at your server and verifies the connection. No docs to read, no snippets to adapt. → [How it works](#connecting-apps)

- Ingest: `/k/<key>/v2/` (TelemetryDeck) and `/v1/logs` (OTLP, protobuf or JSON, gzip supported)
- Never stores IP addresses; optional GeoIP (DB-IP City Lite) resolves country / region / city
- Admin UI: overview (active users and events, weekly retention cohorts, app versions over time, top events, breakdowns by app version / OS / device / locale / country / city), new vs returning users, event types, errors, event browser, API key management; a hint when test-mode events are hidden; manual refresh (default) or optional auto refresh that only re-fetches data
- **One app, several platforms**: iOS and Android (and macOS, watchOS, …) can share one app and key; a platform filter (taken from the SDK's operating-system parameter) narrows every page to one platform
- **Read API for agents**: a per-app read-only key and a self-describing JSON API (`/r/v1`), so Claude, Codex or a script can analyse your data without opening the page → [docs/READ_API.md](docs/READ_API.md)
- **AI-assisted integration**: a copy-and-paste prompt for your coding agent, generated per app with everything filled in (English and Chinese)
- Design notes: [DESIGN.md](DESIGN.md)

## Install

> ## ⚠️ USE ON YOUR OWN RISK
>
> The install script runs with root/sudo and modifies your server: it installs a binary, creates a system user and a systemd service, and — when you pass `--domain` — writes an nginx config and requests a certificate from Let's Encrypt (accepting its terms of service on your behalf).
> `curl | sh` executes code downloaded from the network. **Read the script before you run it**, and take a backup before using it on a production machine. This project is provided "as is" under the MIT license, without warranty of any kind.

**Recommended**: download → read → preview with `--dry-run` → install:

```sh
curl -fsSL https://github.com/bones7456/telemetrylite/releases/latest/download/install.sh -o install.sh
less install.sh
sh install.sh --dry-run     # detect the environment and print the plan; changes nothing
sh install.sh
```

One-liner (riskier — equivalent to running it without reading):

```sh
curl -fsSL https://github.com/bones7456/telemetrylite/releases/latest/download/install.sh | sh
```

This is the **minimal install**: it only installs the binary and the service. The service listens on `127.0.0.1:8888`, **does not touch nginx and does not request certificates**, so it is not reachable from the internet yet. When it finishes it prints the admin password (shown only once).

### Enable HTTPS (optional): `--domain`

Apps need a public HTTPS address to report to. With `--domain` the script additionally writes an nginx reverse-proxy config and asks certbot to obtain a Let's Encrypt certificate (**accepting its terms of service on your behalf**). Prerequisites: the domain already resolves to this machine, port 80 is reachable from the internet, and nginx and certbot are installed (the script never installs packages for you).

```sh
sh install.sh --domain telemetry.example.com --email me@example.com   # --email receives expiry notices; optional
```

If you would rather not let the script touch nginx, set up your own reverse proxy or use Caddy. After a minimal install you can re-run the command with `--domain` to add HTTPS later.

The script inspects the environment first, then picks how to deploy:

| Checked | Behavior |
|---|---|
| OS / CPU | Linux only; picks the x86_64 or aarch64 binary automatically, exits otherwise |
| Privileges | Runs directly as root; otherwise uses `sudo`; exits if neither is available |
| Downloader | `curl` or `wget`; the download is verified against `SHA256SUMS` from the release and aborts on mismatch |
| systemd | Present: creates a dedicated user and a service enabled at boot. Absent: installs the binary only and prints how to start it manually |
| Already installed? | Then it **updates**: compares the installed binary with the release and does nothing if it is already the latest; otherwise it replaces the binary (the old one is kept as `telemetrylite.prev`), restarts the service, and **rolls back automatically** if the new version does not start. Your service file and settings are kept; on hosts without systemd it tells you to restart your process |
| Port in use | Exits if the default 8888 is taken and suggests `--port` |
| Reverse proxy | With `--domain`: nginx → configured automatically (with login rate limiting) and certbot obtains HTTPS; Caddy → prints a snippet only, never edits your Caddyfile; neither → tells you how to install nginx, never installs packages on its own |

Options: `--domain`, `--email` (certbot email), `--version v0.1.0`, `--port N`, `--no-geoip`, `--force` (reinstall even if up to date), `--rewrite-unit` (regenerate the systemd service file), `--lang en|zh`, `--dry-run`, `--yes`, `--uninstall` (keeps the data directory). **To update later, re-run the same command.** On a fresh install the script prints the admin UI URL and the admin password, **shown only once**.

Next steps (issuing keys, connecting apps, backups, troubleshooting): **[docs/DEPLOY.md](docs/DEPLOY.md)**.

### Releasing (maintainers)

`version` in `Cargo.toml` is the single source of truth for the version (between releases it carries a `-dev` suffix). To release:

```sh
# 1. set version = "0.1.7" in Cargo.toml, then refresh the lock file
cargo build && git commit -am "Release 0.1.7"
# 2. tag and push: the tag must equal the Cargo.toml version, otherwise CI refuses to publish
git tag v0.1.7 && git push origin main v0.1.7
# 3. afterwards start the next cycle: version = "0.1.8-dev"
```

A `v*` tag triggers GitHub Actions: tests and the regression test run in parallel with the two builds (x86_64 and aarch64 static binaries); the regression test is also run on the x86_64 artifact; only when everything is green are the binaries published to a Release together with `SHA256SUMS` and `install.sh` (repository name filled in). The running version is shown by `telemetrylite --version`, in the startup log and in the page footer.

To deploy from source to your own server: `./scripts/deploy.sh you@server [domain]` (cross-compiles locally and uploads; needs zig and cargo-zigbuild).

## Configuration (environment variables)

| Variable | Default | Description |
|---|---|---|
| `TL_LISTEN` | `127.0.0.1:8888` | Listen address; put nginx in front |
| `TL_DB_PATH` | `./telemetrylite.db` | SQLite file |
| `TL_ADMIN_PASSWORD` | random on first start | If set, it is applied on every start |
| `TL_GEOIP_DB` | empty | Path to an mmdb file; GeoIP is disabled when unset |
| `TL_GEOIP_AUTO_UPDATE` | `false` | `true` downloads DB-IP City Lite and refreshes it monthly (stored as `./dbip-city-lite.mmdb` by default) |
| `TL_TRUSTED_PROXY` | empty | Extra trusted reverse-proxy IPs (comma-separated); loopback is always trusted, used to read `X-Forwarded-For` |

## Connecting apps

Issue a write key under Settings in the admin UI (shown only once); the UI also shows ready-to-paste snippets.

**Let your AI coding agent do it (recommended).** In the admin UI open your app → Settings → "Integrate with an AI agent":

1. Issue a write key. The prompt below it fills in this server's URL, the app id and the new key by itself.
2. Press "Copy prompt".
3. Paste it into Claude Code, Codex, Cursor or any coding agent, from the root of your app's repository.

The agent detects your platform, adds the SDK, points it at your server, builds the project and checks with a test event that the key works. The same prompt for manual use is in **[docs/AGENT_PROMPT.md](docs/AGENT_PROMPT.md)**.

Or do it by hand:

**TelemetryDeck SDK (Swift)**

```swift
let config = TelemetryDeck.Config(appID: "<app id>", baseURL: URL(string: "https://telemetry.example.com/k/<key>/")!)
TelemetryDeck.initialize(config: config)
```

**OpenTelemetry**

```
OTEL_EXPORTER_OTLP_LOGS_ENDPOINT=https://telemetry.example.com/v1/logs
OTEL_EXPORTER_OTLP_LOGS_PROTOCOL=http/protobuf
OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer <key>
```

LogRecord mapping: `event_name` (or the `event.name` attribute, or the body) → event name; `user.id` / `enduser.id` → anonymous user (salted and hashed before storing); `session.id` → session; resource attributes `service.version`, `os.*`, `device.model.*` → dimension columns; all other attributes go into the payload. Metrics and traces are accepted and discarded.

## Analysing data with an AI agent

Issue a **read key** (`tlr_…`, read-only, bound to one app) under Settings → "Read API keys", press "Copy prompt" in "Analyze with an AI agent", paste it into Claude Code, Codex or any agent that can run `curl`, and add your question. The agent starts at `GET /r/v1/`, which describes the app's data and every endpoint, then uses `/r/v1/overview`, `/r/v1/query` and `/r/v1/events`. Details: **[docs/READ_API.md](docs/READ_API.md)**.

## Data and privacy

- Raw IP addresses are never stored; client user identifiers are salted and hashed again before they are stored.
- Each app has its own retention period (90 days by default); expired events are purged hourly.
- To keep storage small, the roughly 30 system parameters the TelemetryDeck SDK attaches to every signal automatically (calendar, accessibility, screen metrics, retention counters and the like) are dropped on ingest. App version, OS, device model and locale are kept as dimensions, and your own parameters as well as those of the SDK's preset events (purchase, error, navigation, …) are kept.

## Development

```sh
cargo test
TL_LISTEN=127.0.0.1:8899 TL_ADMIN_PASSWORD=pw cargo run

# black-box regression test: starts the binary on a free port with a temporary database
cargo build --release && python3 scripts/regress.py --spawn target/release/telemetrylite
```

CI runs the same regression test before anything is published (see [DESIGN.md](DESIGN.md)).

## License

MIT. IP geolocation data comes from [DB-IP.com](https://db-ip.com) (CC BY 4.0).
