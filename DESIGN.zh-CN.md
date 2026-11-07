# TelemetryLite — 设计文档

[English](DESIGN.md) | 简体中文

> 状态:已实现并发布(见 Releases)。本文描述**实际实现**;最后一节列出有意暂未实现的内容。
> 目标:Rust 单二进制 + SQLite 的轻量事件收集服务,面向 iOS / Android 等 App,同时兼容 **TelemetryDeck SDK** 与 **OTLP/HTTP Logs**。

## 1. 决策

| 项 | 决定 |
|---|---|
| OTLP 范围 | 只收 **Logs**,LogRecord 视为「事件」。Metrics/Traces 返回成功但丢弃(§4.2) |
| 内部数据模型 | **TelemetryDeck 风格 signal** 宽表,OTLP LogRecord 映射进来 |
| 账户模型 | 单管理员 + 多 App;每个 App 可签发多个只写 API key |
| 规模 | 自托管单机,日事件最多约 100 万 |
| 隐私 | 不存 IP;GeoIP 在内存中解析,只保存国家 / 地区 / 城市 |
| 接入 | **AI 辅助接入**:界面按 App 生成提示词(已填好服务器地址、App ID、key),编程 agent 照着做完 SDK 集成 |
| 界面语言 | 英文(默认)与中文,页面内可切换;文档与命令行输出也是双语 |

## 2. 总体架构

```
 iOS / Android App                              浏览器
 ├─ TelemetryDeck SDK ──POST /k/<key>/v2/──┐      │
 └─ OTel SDK ──POST /v1/logs ──────────────┤      │ /api/*  (cookie 会话)
                                           ▼      ▼
                    ┌────────────────────────────────────────────┐
                    │  axum HTTP 服务(单进程)                      │
                    │  ingest:鉴权 → 解码 → 归一化 → 限制           │
                    │        │                                    │
                    │        ▼  有界 mpsc 队列                     │
                    │  writer 线程:批量合并为一个事务写入            │
                    │        │                                    │
                    │  SQLite (WAL) ◄── 管理 API(独立连接)         │
                    │  后台任务:保留期清理、GeoIP 更新               │
                    │  静态页面:web/index.html(include_str!)       │
                    └────────────────────────────────────────────┘
```

- **单写入者**:所有写入经一个线程把队列合并成一个事务落库,避开写锁竞争;管理/查询走独立连接(WAL 允许读写并行)。
- **有界队列 + 背压**:队列满时返回 `503 + Retry-After`,SDK 会重试,内存有上限。
- **全部内嵌**:界面是一个用 `include_str!` 内嵌的 HTML,部署就是一个文件。
- **鉴权不碰 SQLite**:写入 key 放在内存映射里(SHA-256 → app id),key 变化时刷新。

## 3. 数据模型(SQLite)

```sql
CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);      -- 盐、管理员密码哈希
CREATE TABLE app (
  id TEXT PRIMARY KEY,                -- UUID(即 TelemetryDeck 的 appID)
  name TEXT NOT NULL, created_at INTEGER NOT NULL,
  retention_days INTEGER NOT NULL DEFAULT 90
);
CREATE TABLE api_key (
  id TEXT PRIMARY KEY, app_id TEXT NOT NULL REFERENCES app(id) ON DELETE CASCADE,
  label TEXT NOT NULL, prefix TEXT NOT NULL,   -- 前 10 位,用于展示
  key_hash TEXT NOT NULL UNIQUE,               -- 十六进制 SHA-256;key 本身高熵,无需 argon2
  created_at INTEGER NOT NULL, revoked_at INTEGER
);
CREATE TABLE signal (
  id INTEGER PRIMARY KEY,
  app_id TEXT NOT NULL, ts INTEGER NOT NULL, received_at INTEGER NOT NULL,   -- unix 毫秒
  type TEXT NOT NULL, user_hash TEXT, session_id TEXT, float_value REAL,
  is_test INTEGER NOT NULL DEFAULT 0,
  app_version TEXT, build TEXT, os TEXT, os_version TEXT, device_model TEXT,  -- 常用维度单独成列
  locale TEXT, sdk TEXT,
  country TEXT, region TEXT, city TEXT,                                       -- 来自 GeoIP,未启用时为 NULL
  payload TEXT NOT NULL DEFAULT '{}'                                           -- 其余参数,JSON
);
CREATE INDEX signal_app_ts   ON signal(app_id, ts);
CREATE INDEX signal_app_type ON signal(app_id, type, ts);
```

- 常用维度单独成列,因为图表最常按它们分组;其余参数留在 `payload`,需要时用 SQLite JSON 函数查询。
- PRAGMA:`journal_mode=WAL`、`synchronous=NORMAL`、`busy_timeout=5000`、`foreign_keys=ON`。
- 保留期:每小时按各 App 的保留天数分批(每批 1 万行)删除过期数据。
- **结构版本**:结构版本存放在 `PRAGMA user_version`(1 = 目前所有发布版;版本化之前创建的库是 0,同样接受)。要改表结构,就在 `db.rs` 里提高 `SCHEMA_VERSION` 并在 `MIGRATIONS` 末尾追加一步;启动时按顺序执行尚未应用的步骤,每步一个事务。库的版本**高于**二进制认识的版本时会明确报错并拒绝启动,所以旧版二进制(比如回滚之后)绝不会去动它不认识的结构。
- 仪表盘查询直接扫带索引的原始表(暂无预聚合,见 §10)。

## 4. 写入入口

### 4.1 TelemetryDeck:`POST /k/<key>/v2/`

已对照 Swift SDK 源码核实:SDK 向 `{baseURL}/v2/`(带 namespace 时为 `/v2/namespace/{ns}/`)发送,请求头只有 `Content-Type`,**没有任何鉴权头**,所以写入 key 放在 base URL 路径里(`https://host/k/<key>/`)。

- 请求体:signal 数组,字段 `appID`、`clientUser`、`sessionID`、`type`、`payload`(字符串字典)、`floatValue`、`isTestMode`(字符串 `"true"/"false"`,也接受布尔)、`receivedAt`(`yyyy-MM-dd'T'HH:mm:ssZ`,时区形如 `+0000`;也接受 RFC 3339)。
- App 由 key 决定,请求体里的 `appID` 被忽略。
- 新版 SDK 每个参数发两个名字(`TelemetryDeck.AppInfo.version` 与已弃用的 `appVersion`)。取新名,旧名作回退,并把重复项从 payload 里去掉。
- **系统噪声过滤**:较新的 SDK 每条信号自动附带约 50 个 `TelemetryDeck.*` 系统参数(日历、无障碍、屏幕尺寸、留存计数等),每条约 1.4 KB。入库时丢弃其中约 30 个(payload 缩到约 350 字节)。保留:维度(版本、build、系统、设备、语言、SDK)以列存放;`RunContext.isTestFlight/isAppStore/isDebug/isSimulator`、`UserPreference.language/region`、`Device.timeZone`;以及 SDK 预设事件(购买、错误、导航等)的全部参数和你自己的参数。过滤发生在 50 个键的上限之前,自定义参数不会被挤掉。

### 4.2 OTLP:`POST /v1/logs`

- **OTLP/HTTP**,支持 `application/x-protobuf` 与 `application/json`,支持 gzip 请求体。不支持 gRPC(移动端更常用 OTLP/HTTP,也省掉 tonic)。
- 鉴权:`Authorization: Bearer <key>`、`x-api-key`,或路径 `/k/<key>/v1/logs`。
- 只用 `prost` 声明所需的 Logs 消息子集(不需要 protoc,也不引入完整的 `opentelemetry-proto`)。
- 映射:

| OTLP | signal |
|---|---|
| `LogRecord.event_name`(无则取属性 `event.name`,再无则字符串 body) | `type` |
| `time_unix_nano`(缺省取 `observed_time_unix_nano`) | `ts` |
| 属性 `user.id` / `enduser.id`(加盐哈希,不原样存) | `user_hash` |
| 属性 `session.id` | `session_id` |
| resource 的 `service.version` / `os.name` / `os.version` / `device.model.*` / `telemetry.sdk.name` | 对应维度列 |
| 其余属性 | `payload` |

- `/v1/metrics`、`/v1/traces` 返回 `200` 并丢弃,避免客户端重试。

### 4.3 通用行为

- 请求体上限 2 MB(解压后最多 8 MB);单次最多 1000 条;单条 payload 最多 50 个键;值超过 1 KB 截断。
- 时间戳与当前相差超过 7 天,视为客户端时钟错误,改用接收时间。
- `user_hash = SHA-256(服务端盐 + app id + 客户端用户标识)`;盐首次启动时随机生成并存入 `meta`。
- 不存 IP(§4.4)。

### 4.4 GeoIP(国家 / 地区 / 城市)

- **数据库:DB-IP "IP to City Lite"(mmdb)。** 无需账号和 license key,每月更新,CC BY 4.0(仅需署名),由 `maxminddb` 通过内存映射直接读取(空载 RSS 约 10 MB)。没有选 MaxMind GeoLite2:它需要账号和 license key,EULA 还有更新义务。
- 体积:解压后约 127 MB。想省空间可把 `TL_GEOIP_DB` 指向 Country Lite;代码读取 country/region/city 中存在的字段。
- 默认:不配置 `TL_GEOIP_DB` 也不开自动更新时,不做解析,三列为 NULL。
- 自动更新(`TL_GEOIP_AUTO_UPDATE=true`):启动时没有库就下载,之后每天检查,超过 30 天就更新。先下载到临时文件,确认能打开后原子重命名并热加载。失败只记 warn,不影响收数。
- 客户端 IP:取 TCP 对端地址;仅当对端是回环地址或在 `TL_TRUSTED_PROXY` 中时,才信任 `X-Forwarded-For` / `X-Real-IP`。私网和回环地址跳过。客户端自报的位置属性不采信。DB-IP 的城市名常带区县后缀(`San Francisco (South Beach)`),入库时去掉;城市分布在统计时对旧数据也去掉后缀,保证同一座城市只算一项。
- 署名:管理界面页脚和 README 标注 DB-IP.com。

## 5. Key 与认证

**写入 key(给 SDK)**:`tl_<32 字节随机 base64url>`;创建时只显示一次,库里只存 SHA-256。每个 App 可有多个(每个都必须有名称,界面默认建议当前时间),随时可吊销,只能写入该 App。鉴权就是查内存映射。

**管理端**:单管理员。密码首次启动时生成并在日志里打印一次,或由 `TL_ADMIN_PASSWORD` 指定(每次启动都生效,忘记密码也用它重置);库里存 argon2 哈希。登录后发 `HttpOnly; SameSite=Strict` cookie(`X-Forwarded-Proto` 为 https 时加 `Secure`),内容是不透明随机 token;会话存内存、有效 30 天、重启后失效。登录失败延迟 0.5 秒,并由 nginx 限速。TLS 由 nginx 终结(§9)。

## 6. 管理界面

一个 HTML 文件,原生 JavaScript,内联 SVG 图表,明暗主题跟随系统。

- **登录**、**App 列表**与新建。
- **平台筛选**:当一个应用里出现多个平台的数据时(`os` 列:`iOS`、`Android` 等——Swift SDK 发 `iOS`/`macOS`/…,Kotlin SDK 发 `Android`,OTLP 取 `os.name`;写入时统一大小写),每个数据页和事件页的工具栏都会出现「平台」下拉。所有统计接口都接受可选的 `platform` 参数(作为 SQL 参数绑定,绝不拼进查询);`/api/apps/{id}/platforms` 列出最近 90 天出现过的平台。选择在浏览器会话内记住。页头的数字始终是整个应用的。iOS + Android 推荐用「一个应用 + 平台筛选」;只有想要完全独立的 key、保留期和仪表盘时才拆成两个应用。
- **页头**:App 切换下拉,以及最近 90 天正式数据的三个关键数字:用户、最常用 SDK、错误数。
- **概览**:时间范围(24 小时 / 7 / 30 / 90 天)、正式/测试数据(横幅提示另一种数据有多少事件被隐藏,可一键切换)、指标卡(最近 1 小时用户、活跃用户、事件、会话)、活跃用户折线图与事件数柱状图、**按周留存队列**(最近 8 周,周一起算)、**App 版本随时间变化**(用户占比,前 8 个版本)、带用户数的热门事件,以及按 App 版本、系统版本、设备型号、语言、国家(界面上按界面语言显示国家名,用浏览器自带的 `Intl.DisplayNames`)、城市的分布。图表有网格线和类似 TelemetryDeck 的悬浮卡片(粗体日期,每个系列一行:彩色圆点、名称、加粗数值;堆叠图显示各系列占比(保留两位小数),并**聚焦鼠标所在位置的系列**:它在每一列里都保持原色,其他系列淡化,对应的图例项和卡片行被强调;被悬停的列会高亮);留存热力图有自己的悬浮卡片(保留两位小数的留存率及第几周、该队列首次出现的那一周、具体人数);缺失的时间桶补零,按浏览器本地时区分桶。
- **用户**:每日新用户与新/回访用户对比。用户的第一条事件出现在已存储数据里的那一天算作新用户(所以过期清理之后,早已不活跃的用户可能再次被算作新用户)。
- **事件类型**:所有事件类型及其用户数、事件数、最近一次时间;点击名称跳到按该事件过滤的事件页。
- **错误**:趋势,以及按错误 id 和类别汇总的表格,数据来自 SDK 预设的 `TelemetryDeck.Error.occurred` 事件。
- **事件**:最近的原始事件、按事件名精确过滤、自定义参数直接展示、`TelemetryDeck.*` 系统参数折叠。
- **设置**:重命名、数据保留天数、写入 key 管理(只显示一次并附接入代码片段、吊销)、**AI agent 接入提示词**(按界面语言,自动填入浏览器当前的服务器地址、App ID,以及签发后的写入 key;文本写在 `web/index.html` 里,需与 `docs/AGENT_PROMPT*.md` 保持同步)、删除 App。
- **刷新**:手动「刷新」按钮(默认)和可选的自动刷新(10 秒 / 30 秒 / 1 分钟 / 5 分钟,选择记在浏览器里)。两者都只重新请求数据并原地更新;后台标签页时自动刷新暂停,请求不会重叠;事件页没有新事件时不动 DOM,已展开的参数不会被折叠。
- **语言**:默认英文,页头可切换中文,选择记在 `localStorage`。

## 7. 代码结构

```
src/main.rs      启动、环境变量配置、路由、优雅停机、保留期任务
src/db.rs        表结构、writer 线程、过期清理
src/ingest.rs    TelemetryDeck 与 OTLP 入口、归一化、限制
src/admin.rs     /api/*:会话、App、key,以及统计 / 用户 / 留存 / 版本 / 错误 / 事件查询
src/geo.rs       mmdb 加载(热替换)、下载与更新
src/i18n.rs      日志语言(英文 / 中文)
web/index.html   整个管理界面
scripts/         install.sh(服务器安装)、deploy.sh(从源码部署)、release.sh(本机交叉编译)、regress.py(黑盒回归测试)
.github/workflows/ci.yml   每次推送跑测试;v* 标签触发发布构建
```

主要依赖:`axum`、`tokio`、`rusqlite`(bundled)、`prost`、`serde_json`、`flate2`、`argon2`、`sha2`、`maxminddb`、`ureq`、`tracing`。发布的二进制约 5 MB(strip + LTO),musl 静态链接。

## 8. 配置(环境变量)

`TL_LISTEN`(默认 `127.0.0.1:8888`)、`TL_DB_PATH`(`./telemetrylite.db`)、`TL_ADMIN_PASSWORD`、`TL_GEOIP_DB`、`TL_GEOIP_AUTO_UPDATE`(默认 false)、`TL_TRUSTED_PROXY`(额外受信的反代 IP,不支持 CIDR)、`TL_LANG`(`zh` 时日志为中文)。

## 9. 部署与发布

- 前面放 nginx:终结 HTTPS、限制请求体大小、给 `/api/login` 限速,并用 `X-Forwarded-For` 传真实客户端 IP(覆盖而非追加)。服务只监听 `127.0.0.1`。
- **`scripts/install.sh`**(从 GitHub Release 获取,`curl | sh` 运行):探测系统、CPU(x86_64 / aarch64)、权限、下载工具、systemd、端口占用和反向代理,打印部署计划,用 `SHA256SUMS` 校验二进制,然后安装、升级或卸载。带 `--domain` 时写入 nginx 配置并申请 Let's Encrypt 证书。输出为双语(默认英文,系统语言为中文或 `--lang zh` 时为中文)。`--dry-run` 不做任何修改。
- **更新**:`install.sh` 是幂等的。只要存在二进制或 service 文件就按更新处理:把已安装二进制的 SHA-256 与发布版 `SHA256SUMS` 里的比较(相同就什么都不做,`--force` 可覆盖),把旧二进制备份为 `telemetrylite.prev`,重启,等待服务响应,**不响应就回滚**到 `.prev`。已有设置(端口、GeoIP、语言)从 service 文件里读回;与安装脚本当前会生成的内容不同的 service 文件会被保留并提示(`--rewrite-unit` 可重新生成)。没有 systemd 时只替换二进制。
- **CI**(`.github/workflows/ci.yml`):`test`(单元测试)与 `regress`(shellcheck、`install.sh --dry-run`、不开 LTO 的 `ci` 配置构建,以及 `scripts/regress.py --spawn`——它启动真实二进制,检查写入、统计、隐私(包括原始 IP 不会落库)、key、平台筛选与管理 API)并行运行。`main` 上每次推送和每个标签还会运行 `build`:用 `cargo-zigbuild` 并行交叉编译 x86_64 与 aarch64 的 musl 二进制(这样发布路径始终被验证,构建缓存也保持热),并**对 x86_64 产物再跑一遍回归测试**。只有 `v*` 标签才会运行 `publish`,且要求 `test`、`regress` 和两个架构的编译全部通过:创建 Release,附上二进制、`SHA256SUMS` 与 `install.sh`(已写入仓库名)。标签必须等于 `Cargo.toml` 里的版本(版本号的唯一来源),否则构建失败。缓存(`rust-cache`)和取消被取代的运行让它保持较快。
- **从源码部署**:`scripts/deploy.sh user@host [域名]` 在本机交叉编译,并在远端运行 `install.sh`。

## 10. 暂未实现 / 有意跳过

- 预聚合的日统计(目前查询直接扫带索引的原始表,对目标规模足够;变慢了再加汇总表)。
- 按 key 限流(靠背压加 nginx `limit_req`)、key 的 `last_used_at`。
- 自定义查询 / Insight(类似 TelemetryDeck Playground 的查询构建器)、漏斗、导出、用户自定义 Dashboard。
- 界面里修改管理员密码(改 `TL_ADMIN_PASSWORD` 后重启)、会话持久化、多用户与角色。
- `TL_TRUSTED_PROXY` 的 CIDR 支持;Metrics 与 Traces;gRPC;告警;高可用。
