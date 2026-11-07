# Deployment guide

English | [简体中文](DEPLOY.zh-CN.md)

Target: a Linux x86_64 or aarch64 server (Debian/Ubuntu + systemd) with nginx in front for HTTPS. The whole service is one static binary plus one SQLite file.

```
App ──HTTPS──► nginx :443 ──► telemetrylite 127.0.0.1:8888 ──► SQLite
```

## Install

> **USE ON YOUR OWN RISK** — the install script runs as root/sudo and modifies the system (user, systemd, nginx, certificates). Read the script first; see "Install" in the [README](../README.md).

**Prerequisites**: a Linux (x86_64 / aarch64) server with root or sudo.

**Minimal install** (download → read → preview → install):

```sh
curl -fsSL https://github.com/bones7456/telemetrylite/releases/latest/download/install.sh -o install.sh
less install.sh
sh install.sh --dry-run
sh install.sh
```

The minimal install only installs the binary and a systemd service (listening on `127.0.0.1:8888`); it does not touch nginx and does not request certificates. From there you can:

- try it out through SSH port forwarding: `ssh -L 8888:127.0.0.1:8888 you@server`, then open `http://127.0.0.1:8888`;
- set up your own reverse proxy or Caddy;
- or let the script do it, see the next section.

**Enable HTTPS (`--domain`)**: the script writes an nginx reverse-proxy config and obtains a Let's Encrypt certificate with certbot (accepting its terms of service on your behalf). Prerequisites: the domain resolves to the server, port 80 is reachable from the internet, and nginx and certbot are installed (the script never installs packages):

```sh
sudo apt install -y nginx certbot python3-certbot-nginx     # Debian/Ubuntu
sh install.sh --dry-run --domain telemetry.example.com
sh install.sh --domain telemetry.example.com --email me@example.com
```

If certbot fails (for example the domain does not resolve yet), the service is already installed and running, just without HTTPS; the script prints the command to finish the job.

The script detects the environment and prints a deployment plan (OS / architecture / privileges / systemd / port / reverse proxy), verifies the binary's SHA256, installs and starts the service, configures nginx and the certificate (when `--domain` is given), and finally prints the **admin password** (shown only on first install — save it right away). The options and how each check is handled are described under "Install" in the [README](../README.md).

**Upgrade**: re-run the same command. The database is migrated automatically; an existing service file and nginx config are not overwritten; SDKs cache and retry events while the service restarts; you must log in to the admin UI again afterwards.

**Uninstall**: `sh install.sh --uninstall` (keeps the `/var/lib/telemetrylite` data).

**Deploy from source** (developers): `./scripts/deploy.sh you@server telemetry.example.com` cross-compiles locally, uploads, and runs `install.sh` on the server. First run `brew install zig && cargo install cargo-zigbuild --locked && rustup target add x86_64-unknown-linux-musl` (with Rust < 1.91 pin cargo-zigbuild to `--version 0.20.1`).

## After installing

1. Open `https://telemetry.example.com/` in a browser and log in with the admin password. The UI language (English / 中文) can be switched in the page header.
2. Create an app → Settings → "Issue new key", and **copy the key immediately** (it is shown only once); the page shows ready-to-use integration code.
3. Verify (replace `<key>` with the key you just issued):

```sh
curl -i -X POST https://telemetry.example.com/k/<key>/v2/ -H 'content-type: application/json' \
  -d '[{"type":"Test.event","clientUser":"me","sessionID":"s1","isTestMode":"true","payload":{}}]'
```

It should return `200`; the event shows up on the Events page (🧪 marks test data), and on Overview after switching to "Test data".

## Connecting apps

Let an AI coding agent do it: copy the prompt from [AGENT_PROMPT.md](AGENT_PROMPT.md). Or by hand:

**TelemetryDeck SDK (Swift)** — the key goes into the base URL path:

```swift
let config = TelemetryDeck.Config(
    appID: "<App ID from the admin UI>",
    baseURL: URL(string: "https://telemetry.example.com/k/<key>/")!)
TelemetryDeck.initialize(config: config)
```

**OpenTelemetry (Logs only)**:

```
OTEL_EXPORTER_OTLP_LOGS_ENDPOINT=https://telemetry.example.com/v1/logs
OTEL_EXPORTER_OTLP_LOGS_PROTOCOL=http/protobuf
OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer <key>
```

Keys are write-only and cannot read data. If one leaks, revoke it under Settings and issue a new one; revocation takes effect immediately.

## Configuration

Use `sudo systemctl edit telemetrylite` to add `[Service]` + `Environment=...`, then `sudo systemctl restart telemetrylite`.

| Variable | Default | Description |
|---|---|---|
| `TL_LISTEN` | `127.0.0.1:8888` | Listen address; if you change the port, update nginx's `proxy_pass` too |
| `TL_DB_PATH` | `/var/lib/telemetrylite/telemetrylite.db` | SQLite file |
| `TL_ADMIN_PASSWORD` | random on first start | If set, it is applied on every start; **use it to reset a forgotten password** |
| `TL_GEOIP_AUTO_UPDATE` | `true` (written by the script) | Downloads and monthly refreshes DB-IP City Lite (about 127 MB; needs access to `download.db-ip.com`). Set `false` to disable |
| `TL_GEOIP_DB` | empty | Use your own mmdb; without it (and without auto update) country/city stay empty |
| `TL_TRUSTED_PROXY` | empty | Extra trusted reverse-proxy IPs besides loopback (comma-separated); not needed when nginx runs on the same host |

Each app's retention (in days) is set on its Settings page (default 90); expired data is purged hourly.

## Backups

All data lives in `/var/lib/telemetrylite/telemetrylite.db` (WAL mode — **do not just `cp` it**). Online backup, no downtime needed:

```sh
sudo -u telemetrylite sqlite3 /var/lib/telemetrylite/telemetrylite.db ".backup '/var/lib/telemetrylite/backup.db'"
```

Put it in a daily cron job and copy the result elsewhere. To restore: stop the service, replace the db file (and delete the `-wal` and `-shm` files next to it), then start it again. The `meta` table holds the salt used for user hashes and must be restored together with the data.

## Troubleshooting

| Symptom | What to check |
|---|---|
| Reporting returns `401` | Wrong or revoked key, or the URL lacks the `/k/<key>/` prefix |
| `400` | Body is not valid JSON/protobuf; for OTLP make sure `Content-Type` matches the encoding |
| `503` | Write queue is full; SDKs retry according to `Retry-After`. If it persists, check disk IO |
| `413` | Request body larger than 2 MB |
| No country / city | Check `journalctl -u telemetrylite \| grep GeoIP` (download failed?), or nginx is not passing `X-Forwarded-For` |
| All locations are the same | A CDN sits in front of nginx, so `$remote_addr` is the CDN's address; restore the real IP with nginx's `real_ip` module |
| Logged out right after login | Use HTTPS, and make sure nginx sends `X-Forwarded-Proto` |
| Service will not start | `sudo journalctl -u telemetrylite -n 50`; common causes: data-directory permissions, port already in use |

Live logs: `sudo journalctl -u telemetrylite -f`.

## Manual deployment (without the script)

`scripts/install.sh` is the complete list of manual steps; read it and follow along.

## Security checklist

- [ ] The service listens on `127.0.0.1` only; only 80/443 are open to the internet
- [ ] HTTPS everywhere; a long admin password (the script already rate-limits `/api/login`)
- [ ] Regular backups
- [ ] Say so in your privacy policy: IPs are not stored, user identifiers are hashed, location is accurate to city level
- [ ] Keep the DB-IP attribution (already in the admin UI footer)
