# TelemetryLite

[English](README.md) | 简体中文

轻量的事件收集服务:Rust 单二进制 + SQLite。同时兼容 **TelemetryDeck SDK** 与 **OTLP/HTTP Logs**,自带管理界面与 API key 签发。管理界面支持中文和英文。

> **一次粘贴,让 AI 完成接入。** 管理界面会为每个 App 生成一段现成的提示词,服务器地址、App ID 和写入 key 都已自动填好。复制给 Claude Code、Codex 或 Cursor,你的编程 agent 就会添加 SDK、指向你的服务器并验证连通性。不用读文档,不用改代码片段。→ [使用方法](#接入)

- 写入:`/k/<key>/v2/`(TelemetryDeck)、`/v1/logs`(OTLP,protobuf/JSON,支持 gzip)
- 不存 IP;可选 GeoIP(DB-IP City Lite)解析国家/地区/城市
- 管理界面:概览(活跃用户与事件、按周留存队列、App 版本随时间变化、热门事件、版本/系统/机型/语言/国家/城市分布)、新用户与回访用户、事件类型、错误、事件浏览器、API key 管理;测试事件被隐藏时有提示;支持手动刷新(默认)和可选的自动刷新,刷新只重新请求数据
- **一个应用,多个平台**:iOS 和 Android(以及 macOS、watchOS 等)可以共用一个应用和 key;平台筛选(取自 SDK 上报的操作系统参数)让每个页面都能只看某一个平台
- **AI 辅助接入**:按 App 生成、信息全部填好的提示词,复制给编程 agent 即可(中英文)
- 设计说明见 [DESIGN.zh-CN.md](DESIGN.zh-CN.md)

## 安装

> ## ⚠️ USE ON YOUR OWN RISK
>
> 安装脚本会以 root/sudo 权限修改你的服务器:安装二进制、创建系统用户和 systemd 服务,指定 `--domain` 时还会写入 nginx 配置并向 Let's Encrypt 申请证书(并代你同意其服务条款)。
> `curl | sh` 会直接执行从网络下载的代码,**请先阅读脚本再运行**,并在生产环境使用前做好备份。本项目按 MIT 许可证「按原样」提供,不附带任何担保。
> *This script runs with root privileges and modifies your system. Read it before you run it. No warranty of any kind.*

**推荐方式**:下载 → 阅读 → 先 `--dry-run` 看计划 → 再安装:

```sh
curl -fsSL https://github.com/bones7456/telemetrylite/releases/latest/download/install.sh -o install.sh
less install.sh
sh install.sh --dry-run     # 只探测环境并打印计划,不做任何修改
sh install.sh
```

一行式(风险更高,等同于不看就运行):

```sh
curl -fsSL https://github.com/bones7456/telemetrylite/releases/latest/download/install.sh | sh
```

这是**最小安装**:只装二进制和服务,服务监听 `127.0.0.1:8888`,**不碰 nginx、不申请证书**,外网暂时访问不到。安装完成会打印管理员密码(仅显示一次)。

### 启用 HTTPS(可选):`--domain`

App 上报需要一个带 HTTPS 的公网地址。加上 `--domain` 后,脚本会额外:写入 nginx 反代配置、用 certbot 向 Let's Encrypt 申请证书(**代你同意其服务条款**)。前提:域名已解析到本机、80 端口可从公网访问、已装好 nginx 与 certbot(脚本不会替你装包)。

```sh
sh install.sh --domain telemetry.example.com --email me@example.com   # --email 接收证书到期提醒,可省略
```

不想让脚本动 nginx,也可以自己配反代或用 Caddy;已装服务后重新运行带 `--domain` 的命令即可补配。

脚本先探测环境,再选择部署方式:

| 探测项 | 处理 |
|---|---|
| 系统 / CPU | 仅 Linux;x86_64 与 aarch64 自动选对应二进制,其他直接退出 |
| 权限 | root 直接执行;否则用 `sudo`;两者都没有则退出 |
| 下载工具 | `curl` 或 `wget`;下载后用 Release 里的 `SHA256SUMS` 校验,不通过即中止 |
| systemd | 有:创建专用用户与服务并开机自启;没有:只装二进制,并给出手动启动命令 |
| 已安装? | 是则走**更新**:先把已安装的二进制与发布版比较,已是最新就什么都不做;否则替换二进制(旧的备份为 `telemetrylite.prev`)并重启服务,新版本起不来时**自动回滚**。保留你的 service 文件与配置;没有 systemd 的机器会提示你自己重启进程 |
| 端口占用 | 默认 8888 被占用时退出并提示 `--port` |
| 反向代理 | 指定 `--domain` 时:有 nginx → 自动配置(含登录限速)并用 certbot 申请 HTTPS;有 Caddy → 只打印配置片段,不改你的 Caddyfile;都没有 → 提示安装命令,不擅自装包 |

选项:`--domain` `--email`(certbot 邮箱)`--version v0.1.0` `--port N` `--no-geoip` `--force`(已是最新也重新安装)`--rewrite-unit`(重新生成 systemd service 文件)`--lang en|zh` `--dry-run` `--yes` `--uninstall`(保留数据目录)。**以后更新,重新运行同一条命令即可**;安装完成后会打印管理界面地址和**仅显示一次**的管理员密码。

后续步骤(签发 key、接入 App、备份、排错)见 **[docs/DEPLOY.zh-CN.md](docs/DEPLOY.zh-CN.md)**。

### 发布(维护者)

推送 `v*` 标签即触发 GitHub Actions:测试通过后为 x86_64 / aarch64 各编一个静态二进制,连同 `SHA256SUMS` 与 `install.sh`(已写入仓库名)发布到 Release:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

从源码部署到自己的服务器:`./scripts/deploy.sh you@server [域名]`(本机交叉编译并上传,需要 zig、cargo-zigbuild)。

## 配置(环境变量)

| 变量 | 默认 | 说明 |
|---|---|---|
| `TL_LISTEN` | `127.0.0.1:8888` | 监听地址,由 nginx 反代 |
| `TL_DB_PATH` | `./telemetrylite.db` | SQLite 文件 |
| `TL_ADMIN_PASSWORD` | 首次随机生成 | 设置后每次启动都以它为准 |
| `TL_GEOIP_DB` | 空 | mmdb 路径;不配置则不解析地理位置 |
| `TL_GEOIP_AUTO_UPDATE` | `false` | `true` 时自动下载并每月更新 DB-IP City Lite(默认存 `./dbip-city-lite.mmdb`) |
| `TL_TRUSTED_PROXY` | 空 | 额外受信反代 IP(逗号分隔);回环地址始终受信,用于读取 `X-Forwarded-For` |

## 接入

在管理界面「设置」中签发写入 key(只显示一次),界面会给出接入片段。

**让 AI 编程 agent 代劳(推荐)。** 在管理界面打开你的 App → 设置 →「让 AI agent 完成接入」:

1. 签发一个写入 key。下方的提示词会自动填入本服务器地址、App ID 和新 key。
2. 点「复制提示词」。
3. 在你的 App 仓库根目录,粘贴给 Claude Code、Codex、Cursor 或任何编程 agent。

agent 会识别你的平台、添加 SDK、指向你的服务器、构建项目,并用一条测试事件确认 key 可用。手动使用的同一段提示词见 **[docs/AGENT_PROMPT.zh-CN.md](docs/AGENT_PROMPT.zh-CN.md)**。

也可以手动接入:

**TelemetryDeck SDK(Swift)**

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

LogRecord 映射:`event_name`(或属性 `event.name`,或 body)→ 事件名;`user.id`/`enduser.id` → 匿名用户(加盐哈希后存储);`session.id` → 会话;Resource 的 `service.version`、`os.*`、`device.model.*` → 维度列;其余属性进 payload。Metrics/Traces 直接返回成功并丢弃。

## 数据与隐私

- 不存原始 IP;客户端用户标识入库前再加盐哈希。
- 每个 App 可设数据保留天数(默认 90),后台每小时清理。
- 为节省存储,TelemetryDeck SDK 每条信号自动附带的约 30 个系统参数(日历、无障碍、屏幕尺寸、留存计数等)在入库时被丢弃。App 版本、系统、设备型号、语言保留为维度列;你自己的参数以及 SDK 预设事件(购买、错误、导航等)自带的参数都会保留。

## 开发

```sh
cargo test
TL_LISTEN=127.0.0.1:8899 TL_ADMIN_PASSWORD=pw cargo run

# 黑盒回归测试:在空闲端口上用临时数据库启动二进制并测试
cargo build --release && python3 scripts/regress.py --spawn target/release/telemetrylite
```

CI 在发布任何东西之前都会跑同一套回归测试(见 [DESIGN.zh-CN.md](DESIGN.zh-CN.md))。

## 许可证

MIT。IP 地理位置数据来自 [DB-IP.com](https://db-ip.com)(CC BY 4.0)。
