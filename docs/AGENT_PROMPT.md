# Integration prompt for AI coding agents

English | [简体中文](AGENT_PROMPT.zh-CN.md)

**Easiest:** open your app in the admin UI → Settings → "Integrate with an AI agent". The prompt is already filled in with this server's URL and the app id, and with the write key as soon as you issue one (the key is shown only once). Press "Copy prompt" and paste it into Claude Code, Codex, Cursor or any other coding agent, **from the root of your app's repository**.

The same prompt for manual use is below: fill in the three values first (or leave the `<…>` placeholders and the agent will ask you).

````text
Integrate this project with my self-hosted TelemetryLite analytics server, which is compatible
with the TelemetryDeck SDKs.

Values (never invent them; if one is still a <placeholder>, ask me):
- Server URL: <SERVER_URL>   e.g. https://telemetry.example.com
- Write key:  <WRITE_KEY>
- App ID:     <APP_ID>

Steps:
1. Detect the platform and build system. Say which route below you use before editing files.
2. Integrate, once at app launch:
   - Swift (iOS/macOS/watchOS/tvOS/visionOS): add the Swift package
     https://github.com/TelemetryDeck/SwiftSDK (Up to Next Major, library "TelemetryDeck"), then
         import TelemetryDeck
         let config = TelemetryDeck.Config(appID: "<APP_ID>", baseURL: URL(string: "<SERVER_URL>/k/<WRITE_KEY>/")!)
         TelemetryDeck.initialize(config: config)
         TelemetryDeck.signal("App.launched")
   - Other platforms: add that platform's official TelemetryDeck SDK and set its custom API base URL
     to  <SERVER_URL>/k/<WRITE_KEY>/  (keep the trailing slash; the SDK appends "v2/" itself).
     If the SDK has no such option, use plain HTTP instead:
         POST <SERVER_URL>/k/<WRITE_KEY>/v2/   Content-Type: application/json
         body: [{"appID":"<APP_ID>","clientUser":"<anonymous user id>","sessionID":"<session id>",
                 "type":"App.launched","payload":{},"isTestMode":"true"}]
   - Already using OpenTelemetry? Export Logs over OTLP/HTTP instead:
     endpoint <SERVER_URL>/v1/logs, header "Authorization: Bearer <WRITE_KEY>", event name in
     LogRecord.event_name.
3. Build the project, and check that the server accepts the key (expect HTTP 200):
     curl -i -X POST "<SERVER_URL>/k/<WRITE_KEY>/v2/" -H 'content-type: application/json' \
       -d '[{"type":"Integration.test","clientUser":"agent-test","sessionID":"s1","isTestMode":"true","payload":{}}]'
   (401 = wrong key, 503 = busy.) Then tell me how to see the event: admin UI -> your app ->
   Events page (it includes test data).
4. Keep the change minimal: only what is needed to connect. Do not add other events or refactor.
````

Debug builds of the Swift SDK send test-mode events, which show under "Test data" in the UI. Only the TelemetryDeck **Swift** SDK has been verified against this server; for other platforms the server speaks the same `v2` protocol, and OTLP or plain HTTP always works.
