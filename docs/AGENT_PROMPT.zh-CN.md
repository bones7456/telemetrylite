# 给 AI 编程 agent 的集成提示词

[English](AGENT_PROMPT.md) | 简体中文

**最省事:** 在管理界面打开你的 App → 设置 →「让 AI agent 完成接入」。提示词里已经自动填好了本服务器地址和 App ID;签发新 key 后,key 也会自动填入(key 只显示一次)。点「复制提示词」,在**你的 App 仓库根目录**粘贴给 Claude Code、Codex、Cursor 等编程 agent 即可。

下面是同一段提示词,供手动使用:先填好三个值(也可以保留 `<…>` 占位符,agent 会来问你)。

````text
把这个项目接入我自托管的 TelemetryLite 统计服务,它兼容 TelemetryDeck SDK。

参数(不要自己编造;如果下面仍是 <占位符>,先问我):
- 服务器地址: <SERVER_URL>   例如 https://telemetry.example.com
- 写入 key:   <WRITE_KEY>
- App ID:     <APP_ID>

步骤:
1. 识别平台和构建系统。修改文件之前,先说明你要用下面哪种方式。
2. 在 App 启动时集成(只初始化一次):
   - Swift(iOS/macOS/watchOS/tvOS/visionOS):添加 Swift Package
     https://github.com/TelemetryDeck/SwiftSDK(Up to Next Major,库选 "TelemetryDeck"),然后
         import TelemetryDeck
         let config = TelemetryDeck.Config(appID: "<APP_ID>", baseURL: URL(string: "<SERVER_URL>/k/<WRITE_KEY>/")!)
         TelemetryDeck.initialize(config: config)
         TelemetryDeck.signal("App.launched")
   - 其他平台:添加该平台官方的 TelemetryDeck SDK,并把它的自定义 API base URL 设为
     <SERVER_URL>/k/<WRITE_KEY>/ (保留末尾斜杠;SDK 会自己拼上 "v2/")。
     如果 SDK 没有这个选项,改用纯 HTTP:
         POST <SERVER_URL>/k/<WRITE_KEY>/v2/   Content-Type: application/json
         body: [{"appID":"<APP_ID>","clientUser":"<匿名用户 id>","sessionID":"<会话 id>",
                 "type":"App.launched","payload":{},"isTestMode":"true"}]
   - 项目已在用 OpenTelemetry?改用 OTLP/HTTP 导出 Logs:
     endpoint <SERVER_URL>/v1/logs,请求头 "Authorization: Bearer <WRITE_KEY>",事件名放在
     LogRecord.event_name。
3. 构建项目,并确认服务器接受这个 key(应返回 HTTP 200):
     curl -i -X POST "<SERVER_URL>/k/<WRITE_KEY>/v2/" -H 'content-type: application/json' \
       -d '[{"type":"Integration.test","clientUser":"agent-test","sessionID":"s1","isTestMode":"true","payload":{}}]'
   (401 = key 错误,503 = 服务繁忙。)然后告诉我在哪里看到这条事件:管理界面 → 你的 App →
   「事件」页(包含测试数据)。
4. 改动保持最小:只做接入所需的部分,不要添加其他事件,也不要重构。
````

Swift SDK 的 Debug 构建会以测试模式上报,在界面的「测试数据」里查看。目前只有 TelemetryDeck **Swift** SDK 对照本服务验证过;其他平台服务端说的是同一套 `v2` 协议,OTLP 或纯 HTTP 则一定可用。
