# TelemetryLite 设计文档

> 状态:M1–M3 已实现(2026-11-07)。与初稿的差异见文末「实现偏离」。
> 目标:Rust 单二进制 + SQLite 的轻量事件收集服务,面向 iOS / Android 等 App,同时兼容 OTLP 和 TelemetryDeck SDK。

## 1. 已确认的决策

| 项 | 决定 |
|---|---|
| OTLP 范围 | 只收 **Logs**,LogRecord 视为「事件」。Metrics/Traces 返回成功但丢弃(见 §4.2) |
| 内部数据模型 | **TelemetryDeck 风格 signal** 宽表,OTLP LogRecord 映射进来 |
| 账户模型 | 单管理员 + 多 App;每个 App 可签发多个写入 API key |
| 规模 | 自托管单机,日事件 < 100 万 |

## 2. 总体架构

```
 iOS/Android App                              浏览器
 ├─ TelemetryDeck SDK ──POST /v2/──┐             │
 └─ OTel SDK ──POST /v1/logs ──────┤             │ /api/*  (cookie 会话)
                                   ▼             ▼
                    ┌──────────────────────────────────────┐
                    │  axum HTTP server (单进程)             │
                    │  ingest 层: 鉴权 → 解码 → 归一化 → 校验 │
                    │        │                              │
                    │        ▼  mpsc channel (有界)         │
                    │  writer 任务: 批量事务写入 (每 200ms/500 条)│
                    │        │                              │
                    │  SQLite (WAL) ◄── query 层 (只读连接池) │
                    │  后台任务: 保留期清理、日聚合            │
                    │  静态资源: 内嵌前端 (rust-embed)         │
                    └──────────────────────────────────────┘
```

要点:
- **单写入者**:所有写入经一个 writer 任务批量落库,避开 SQLite 写锁竞争;查询走独立只读连接。
- **有界队列 + 背压**:队列满时返回 `503 + Retry-After`,SDK 会自行重试,不会 OOM。
- **前端内嵌**:前端构建产物用 `rust-embed` 打进二进制,真正单文件部署。

## 3. 数据模型(SQLite)

```sql
CREATE TABLE app (
  id          TEXT PRIMARY KEY,          -- UUID,即 TelemetryDeck 的 appID
  name        TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  retention_days INTEGER NOT NULL DEFAULT 90
);

CREATE TABLE api_key (
  id          TEXT PRIMARY KEY,
  app_id      TEXT NOT NULL REFERENCES app(id) ON DELETE CASCADE,
  label       TEXT NOT NULL,
  prefix      TEXT NOT NULL,             -- 明文前 8 位,用于列表展示/定位
  key_hash    BLOB NOT NULL UNIQUE,      -- SHA-256(key);key 本身高熵,无需 argon2
  created_at  INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at  INTEGER
);

CREATE TABLE signal (
  id          INTEGER PRIMARY KEY,
  app_id      TEXT    NOT NULL,
  ts          INTEGER NOT NULL,          -- 事件发生时间,unix ms
  received_at INTEGER NOT NULL,
  type        TEXT    NOT NULL,          -- 事件名,如 "App.launched"
  user_hash   TEXT,                      -- 匿名用户标识(客户端已哈希)
  session_id  TEXT,
  float_value REAL,
  is_test     INTEGER NOT NULL DEFAULT 0,
  -- 常用维度提取成独立列,便于索引和分组
  app_version TEXT, build TEXT, os TEXT, os_version TEXT, device_model TEXT,
  locale TEXT, sdk TEXT,
  country TEXT, region TEXT, city TEXT,  -- 由 GeoIP 解析,未配置库时为 NULL
  payload     TEXT NOT NULL DEFAULT '{}' -- 其余自定义参数,JSON
);
CREATE INDEX signal_app_ts   ON signal(app_id, ts);
CREATE INDEX signal_app_type ON signal(app_id, type, ts);

-- 日聚合,让仪表盘不扫原始表
CREATE TABLE daily_stat (
  app_id TEXT, day TEXT, type TEXT, app_version TEXT, os TEXT,
  events INTEGER, users INTEGER,         -- users = 当日去重用户数
  PRIMARY KEY (app_id, day, type, app_version, os)
);

CREATE TABLE admin_session (token_hash BLOB PRIMARY KEY, expires_at INTEGER);
```

说明:
- 常用维度(版本、系统、机型、语言)单独成列:图表按它们分组最频繁,避免到处 `json_extract`。其余自定义参数留在 `payload`,需要时用 SQLite JSON 函数查询,或建表达式索引。
- `daily_stat.users` 为精确去重数,由后台任务从原始表增量重算「今天和昨天」两天。DAU 超过一天后不可再相加,**跨天的独立用户数查询需要走原始表**(保留期内)。
- 保留期清理:每小时 `DELETE FROM signal WHERE ts < now - retention`,分批删除并定期 `PRAGMA incremental_vacuum`。`daily_stat` 永久保留(体积极小)。
- PRAGMA:`journal_mode=WAL`、`synchronous=NORMAL`、`busy_timeout=5000`。

## 4. 入口协议

### 4.1 TelemetryDeck 兼容入口:`POST /v2/`

SDK 只需把 `apiBaseURL` 指向本服务即可。

- 请求体:signal 对象数组,字段大致为 `appID`、`clientUser`、`sessionID`、`type`、`payload`、`floatValue`、`isTestMode`、`receivedAt`。
- 鉴权:TelemetryDeck 的 SDK 里 appID 本身就是公开标识,没有 key 头。我们的方案是 **appID 必须存在 + 同时校验写入 key**(见 §5),key 通过自定义 base URL 的路径或 header 带入。
- `payload` 有两种历史形态:字典 `{"k":"v"}` 与字符串数组 `["k:v"]`,两种都要解析。
- 以 `TelemetryDeck.` 为前缀的系统参数(如 `appVersion`、`systemVersion`、`modelName`、`locale`)映射到 §3 的维度列。

> ⚠️ **待核实(实现第一步的 spike)**:以上字段名、payload 形态、SDK 是否支持自定义 header/路径,需对照各端 SDK 当前版本源码确认(Swift、Kotlin、Flutter 等)。这是整个项目兼容性的风险点,必须先做。

### 4.2 OTLP 入口:`POST /v1/logs`

- 支持 **OTLP/HTTP**,`application/x-protobuf` 和 `application/json` 两种编码,支持 gzip 请求体。
- 不支持 gRPC(4317)——OTLP/HTTP 在移动端更常用,且省掉 tonic 依赖,体积更小。
- 鉴权:`Authorization: Bearer <key>` 或 `x-api-key`(OTel SDK 通过 `OTEL_EXPORTER_OTLP_HEADERS` 配置)。
- 映射规则:

| OTLP | signal |
|---|---|
| `LogRecord.event_name`(无则取 attribute `event.name`,再无则 `body` 字符串) | `type` |
| `time_unix_nano`(缺省取 `observed_time_unix_nano`) | `ts` |
| attribute `user.id` / `enduser.id`(服务端再加盐哈希,不原样存) | `user_hash` |
| attribute `session.id` | `session_id` |
| Resource `service.version` / `os.type` / `os.version` / `device.model.identifier` / `telemetry.sdk.name` | 对应维度列 |
| 其余 attributes | `payload` |

- `/v1/metrics`、`/v1/traces`:返回 `200` 空的 Partial Success 响应并丢弃,避免客户端报错重试。
- Protobuf 解码:引入 `opentelemetry-proto`(仅启用 logs 相关 + `prost`),不需要完整 OTel SDK。

### 4.3 通用入口行为

- 请求体上限 1 MB,单批最多 1000 条;单条 payload 键数 ≤ 50,值长度 ≤ 1 KB,超限截断并计数。
- 时间戳校验:`ts` 超出「现在 ± 7 天」则以 `received_at` 代替。
- 按 key 的限流(令牌桶,内存),默认每 key 200 req/s,可配置。
- 隐私:**不存 IP**。IP 只在请求处理中用于 GeoIP 查询(见 §4.4),查完即丢,不写库、不写日志;`user_hash` 入库前用「服务端 salt + app_id」再哈希一次(TelemetryDeck 同样做法),原始 ID 不落库。

### 4.4 GeoIP(国家 / 地区 / 城市)

- **数据库选型:DB-IP "IP to City Lite"(mmdb 格式)**。
  - 理由:无需注册和 license key,可直接下载(`https://download.db-ip.com/free/dbip-city-lite-YYYY-MM.mmdb.gz`),每月更新;CC BY 4.0 授权,允许自托管和再分发,只需署名;mmdb 格式可直接用 `maxminddb` crate 读取,查询是内存映射,微秒级。
  - 未选 MaxMind GeoLite2:精度相近,但需账号 + license key,EULA 还要求定期更新并有使用限制,不利于「开箱即用单二进制」。
  - 体积:City Lite 解压后约上百 MB;若只要国家,可换 "Country Lite"(几 MB)。`TL_GEOIP_DB` 指向哪个就用哪个,代码只读 country/region/city 中存在的字段。
- **默认行为**:不配置 `TL_GEOIP_DB` 且未开启自动更新 → 不做解析,三列为 NULL,界面隐藏地理卡片。
- **自动更新**(`TL_GEOIP_AUTO_UPDATE=true`):启动时若库不存在则下载到数据目录;之后后台任务每月检查一次,下载到临时文件、校验可打开后原子替换并热加载。下载失败只记 warn,不影响收数。
- **取 IP**:默认用 TCP 对端地址;仅当对端在 `TL_TRUSTED_PROXY` 内时才信任 `X-Forwarded-For` / `X-Real-IP`(防伪造)。解析结果只取 country(ISO 码)、region、city 三个字符串;私网/回环地址直接跳过。
- **署名义务**:CC BY 4.0 要求署名,管理界面页脚和 README 加 "IP geolocation by DB-IP.com"。
- OTLP 与 TelemetryDeck 两个入口共用同一套解析;客户端自报的位置属性不采信。

## 5. API key 与认证

**写入 key(给 SDK)**
- 形如 `tl_<app 短前缀>_<32 字节随机 base64url>`;只在创建时展示一次,库里仅存 SHA-256。
- 每个 App 可建多个(按渠道/版本分发),可随时吊销,记录 `last_used_at`。
- key 只具备「写入该 App」权限,不能读数据。
- 校验路径:内存 `HashMap<hash, (app_id, revoked)>` 缓存,吊销/新建时刷新,热路径不碰 SQLite。

**管理端**
- 单管理员,密码首次启动时生成并打印到日志,或由环境变量 `TL_ADMIN_PASSWORD` 指定;库里存 argon2 哈希。
- 登录后发 `HttpOnly + SameSite=Strict` cookie 会话(不透明随机 token,库里存哈希)。
- 管理 API 全部在 `/api/*`,与 ingest 路径分离;管理端 mutating 请求校验 `Origin` 防 CSRF。
- TLS 由前置 nginx 终结,服务本身只跑明文 HTTP,不内置 TLS(见 §8.1)。

## 6. 管理界面(参考 TelemetryDeck)

TelemetryDeck 的核心体验是「Dashboard = 一组可排布的 Insight 卡片」。MVP 做精简版:

**页面**
1. **登录页**
2. **App 列表 / 新建 App**
3. **App 概览(Dashboard)**:顶部时间范围选择(24h / 7d / 30d / 自定义)+ 测试数据开关,下面是固定卡片:
   - 活跃用户趋势(DAU 折线)
   - 事件总量趋势
   - 热门事件 Top N(条形图)
   - 版本分布、系统版本分布、机型分布(饼/条形)
   - 新用户 vs 回访用户
4. **事件浏览器**:按 `type` 筛选、查看最近原始事件(含 payload JSON),用于调试 SDK 接入。
5. **自定义 Insight(v2)**:选事件 + 聚合方式(次数/去重用户)+ 分组维度(任意维度列或 payload 键)+ 图表类型,保存为卡片。
6. **设置**:API key 管理(创建、复制一次性展示、吊销)、保留期、App 删除、修改管理员密码。

**技术选型**:Vite + Preact(或 Svelte)+ uPlot/轻量图表库,构建产物压缩后目标 < 300 KB,`rust-embed` 内嵌;暗色模式跟随系统。
**查询 API**:`GET /api/apps/:id/insights/query` 接受 JSON 描述(事件过滤、时间范围、粒度、分组、聚合),服务端翻译为参数化 SQL。字段名走白名单,payload 键用 `json_extract(payload, ?)` 绑定参数,杜绝拼接。

## 7. 代码结构(单 crate,按模块分)

```
src/
  main.rs            启动、配置(clap + 环境变量)、优雅停机
  config.rs
  db/                连接、迁移(refinery 或手写 user_version)、queries
  ingest/            td_v2.rs  otlp_logs.rs  normalize.rs  limits.rs
  writer.rs          批量写入任务
  auth/              api_key.rs  admin.rs
  api/               apps.rs  keys.rs  insights.rs  events.rs  session.rs
  geoip.rs           mmdb 加载(可热替换)、下载更新
  jobs/              retention.rs  rollup.rs
  web.rs             rust-embed 静态资源 + SPA fallback
web/                 前端源码
```

主要依赖:`axum`、`tokio`、`rusqlite`(bundled,同步 API + `spawn_blocking`,比 sqlx 更轻)、`prost`、`serde_json`、`flate2`、`argon2`、`sha2`、`rust-embed`、`tracing`、`maxminddb`(GeoIP)。
目标:release 二进制 < 15 MB(strip + lto),空载内存 < 30 MB。

## 8. 配置

环境变量 / CLI 参数:`TL_LISTEN`(默认 `127.0.0.1:8888`,只听本机,由 nginx 反代)、`TL_DB_PATH`(默认 `./telemetrylite.db`)、`TL_ADMIN_PASSWORD`、`TL_USER_SALT`(缺省首次启动随机生成并持久化)、`TL_DEFAULT_RETENTION_DAYS`、`TL_GEOIP_DB`(mmdb 路径,不设则不解析)、`TL_GEOIP_AUTO_UPDATE`(默认 false,设为 true 则自动下载/每月更新)、`TL_TRUSTED_PROXY`(可信反代 IP/CIDR,用于读取 `X-Forwarded-For`)。

### 8.1 部署:nginx 反代 + 单二进制

**结论:建议放在 nginx 后面。** 服务只听 `127.0.0.1:8888`,nginx 负责 HTTPS 证书(Let's Encrypt / certbot)、HTTP/2、限制请求体大小,并把真实客户端 IP 通过 `X-Forwarded-For` 传给服务(GeoIP 依赖它)。这样服务里不用做 TLS 和证书续期,二进制更小更简单。

nginx 要点:
```nginx
server {
  listen 443 ssl http2;
  server_name telemetry.example.com;
  # ssl_certificate ... 由 certbot 管理
  client_max_body_size 2m;
  location / {
    proxy_pass http://127.0.0.1:8888;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $remote_addr;   # 覆盖而非追加,防客户端伪造
    proxy_set_header X-Forwarded-Proto $scheme;
  }
}
```
对应服务配置 `TL_TRUSTED_PROXY=127.0.0.1`。OTLP 的 gzip 请求体由服务自己解压,nginx 透传即可。

**构建与发布(无 Docker)**
- 目标:Linux 静态二进制 `x86_64-unknown-linux-musl`(服务器是 ARM 则用 `aarch64-unknown-linux-musl`)。musl 静态链接后不依赖服务器的 glibc 版本,scp 过去即可运行。
- 在 macOS 上用 `cargo zigbuild --release --target <target>` 交叉编译(`rusqlite` 的 bundled SQLite 需要 C 工具链,zig 可以搞定);`Cargo.toml` 里 `strip=true, lto=true, codegen-units=1, panic="abort"`。
- 前端先 `npm run build`,产物由 `rust-embed` 在编译期打入,所以 scp 一个文件就是全部。
- 提供 `scripts/release.sh`:构建前端 → 交叉编译 → 输出 `dist/telemetrylite-<target>` 及 sha256。
- 服务器侧提供示例 `telemetrylite.service`(systemd,`User=` 非 root、`Restart=always`、数据目录 `/var/lib/telemetrylite`),升级流程:`scp` → 替换二进制 → `systemctl restart`。SQLite 迁移在启动时自动执行。

## 9. 里程碑

1. **Spike**:核实各端 TelemetryDeck SDK 协议细节,用 Swift SDK 打通一条 signal。(§4.1 风险点)
2. **M1 内核**:schema + writer + `/v2/` + `/v1/logs` + API key + CLI 创建 App/key(先无 UI,curl 可验证)。
3. **M2 查询与 UI**:管理员登录、App/key 管理、概览 Dashboard、事件浏览器。
4. **M3 打磨**:保留期清理、日聚合、限流、交叉编译脚本、systemd 示例、README 与各端接入示例。
5. **M4(可选)**:自定义 Insight、漏斗/留存、数据导出。

## 10. 非目标 / 暂不做

Metrics 与 Traces、gRPC、多用户与角色、告警通知、跨 App 汇总、集群/高可用、存储原始 IP、经纬度级定位。

## 11. 仍待你确认的小问题(有默认值,不回答就按默认走)

1. **写入 key 如何随 TelemetryDeck SDK 发送**:默认方案是 base URL 带路径前缀 `https://host/k/<key>/`,因为 SDK 多数不支持自定义 header。若 spike 发现某端 SDK 支持 header,再增加 header 方式。
2. **项目名**:暂用 `telemetrylite`(沿用目录名)。
3. **前端框架**:默认 Preact;你若更熟 Svelte/Vue 可换。
4. **许可证**:MIT(已确认),仓库根目录放 `LICENSE`。

## 12. 实现偏离(相对上文设计)

为保持代码精简,以下几项与初稿不同:

- **前端**:单文件 `web/index.html`(原生 JS + 内联 SVG 图表),无 Preact、无 npm 构建,用 `include_str!` 内嵌。
- **协议核实已完成**:TelemetryDeck SDK 无鉴权头,`POST {baseURL}/v2/`(或 `/v2/namespace/{ns}/`),`payload` 为字符串字典,`isTestMode` 为字符串,`receivedAt` 时区形如 `+0000`;新旧参数名并发,服务端取新名、旧名作回退并去重。
- **未实现**:`daily_stat` 日聚合表(当前直接查带索引的原始表,日事件 < 100 万时足够;后续数据量大再加)、按 key 限流(靠有界队列背压 + nginx `limit_req`)、`api_key.last_used_at`、新/回访用户卡片、自定义 Insight、修改管理员密码接口(改 `TL_ADMIN_PASSWORD` 重启即可)。
- **会话**:内存存放,重启后需重新登录。
- **用户盐**:首次启动随机生成并存入 `meta` 表,无 `TL_USER_SALT` 环境变量。
- **`TL_TRUSTED_PROXY`**:仅支持 IP 列表(逗号分隔),回环地址始终受信;无 CIDR。
- **OTLP**:不引入 `opentelemetry-proto`,用 `prost` 手写所需的 Logs 消息子集(无需 protoc)。
