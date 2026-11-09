# Read API for analysis (`/r/v1`)

English | [简体中文](READ_API.zh-CN.md)

A read-only JSON API so that AI agents (Claude Code, Codex, ...) and scripts can analyse an app's data without opening the admin UI.

## Read keys

- Issue one in the admin UI: app → Settings → "Read API keys". It starts with `tlr_` and is shown only once.
- It is bound to **one app** (the app is implied by the key, there is no app id to pass) and can only call `/r/v1/*`. It cannot report events (the ingest endpoints answer `403`), cannot open the admin API and cannot manage keys.
- Send it as `Authorization: Bearer tlr_…` (or `x-api-key`). Never put it in a URL. Revoke it at any time; the revocation is immediate.
- Limits: 120 requests per minute per key (`429` + `Retry-After`), at most 4 queries run at once, and a query is cancelled after 5 s (`504 query_timeout`).

## Let an agent do the analysis

In the admin UI, app → Settings → "Analyze with an AI agent": issue a read key, press "Copy prompt", paste it into your agent and write your question at the end. The prompt is short on purpose: the API describes itself.

```text
curl -s -H "Authorization: Bearer $KEY" https://telemetry.example.com/r/v1/
```

## Endpoints

| Endpoint | Purpose |
|---|---|
| `GET /r/v1/` | Self-description: the app, data range, platforms, event types (last 30 days), custom parameter names, every endpoint with its parameters, conventions. Call it first. |
| `GET /r/v1/overview` | One call for a first look: users / events / sessions with the change against the previous period, new users, trend, top events, breakdowns (app version, OS version, device, locale, country, city) with each value's share of users, top errors, weekly retention cohorts. |
| `GET /r/v1/events` | Raw events, newest first. Custom parameters are under `params`. Pages with `cursor`. |
| `GET /r/v1/query` | Aggregation: `metric` (`events`, `users`, `sessions`; several allowed) grouped by up to 3 fields, optionally per `interval` (`hour`, `day`, `week`, `month`). |

Common parameters: `days` (default 7), `since` / `until` (RFC 3339, `YYYY-MM-DD` or unix ms), `test` (`0` live, default; `1` test-mode; `all`), `platform` (e.g. `iOS`, case-insensitive), `tz` (minutes east of UTC for buckets, default 0), `format` (`json`, and `csv` / `md` on `events` and `query`).

`events` and `query` also take `type` (exact event name) and repeatable `where=<field>:<value>` (ANDed). `events`: `limit` (max 500), `cursor`. `query`: `metric`, `group_by`, `interval`, `order` (`value` or `key`), `limit` (max 1000).

Fields for `group_by` / `where`: `type`, `platform`, `app_version`, `os_version`, `device_model`, `locale`, `country`, `city`, `build`, `sdk`, and `payload.<key>` for any custom parameter (names are listed under `custom_parameters` in `/r/v1/`).

```text
# purchases per plan and country, weekly, last 90 days
/r/v1/query?type=Purchase.completed&metric=events,users&group_by=payload.plan,country&interval=week&days=90
# raw events of one plan, as a compact table
/r/v1/events?type=Purchase.completed&where=payload.plan:yearly&limit=20&format=md
```

## Built for agents

- Self-describing (`/r/v1/`): event types, custom parameters, `guide_for_agents` and ready-made `examples`, so the prompt does not go stale.
- Every response has `meta` with the effective window, data kind (live / test / all), platform filter, time zone and `truncated`. Nothing is cut silently.
- Unknown parameters are rejected instead of ignored, so a typo cannot make an agent analyse the wrong thing.
- Errors are `{"error": "<code>", "message": "...", "hint": "..."}`; the `hint` says how to fix the request.
- Timestamps are ISO 8601 UTC. `csv` / `md` output costs far fewer tokens than JSON for large tables.
- No raw SQL: fields are whitelisted and custom parameter names are bound as parameters.

## Privacy

Users are salted hashes and IP addresses are never stored. A read key can see everything in `payload`, i.e. any custom parameters your app sends, so do not put personal data there.
