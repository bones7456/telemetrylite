# 部署指南

[English](DEPLOY.md) | 简体中文

环境:Linux x86_64 服务器(Debian/Ubuntu + systemd),前面用 nginx 提供 HTTPS。整个服务就是一个静态二进制 + 一个 SQLite 文件。

```
App ──HTTPS──► nginx :443 ──► telemetrylite 127.0.0.1:8888 ──► SQLite
```

## 安装

> **USE ON YOUR OWN RISK** —— 安装脚本以 root/sudo 修改系统(用户、systemd、nginx、证书)。请先阅读脚本,详见 [README](../README.zh-CN.md)「安装」。

**前置条件**:Linux(x86_64 / aarch64)服务器,有 root 或 sudo。

**最小安装**(下载 → 阅读 → 预演 → 安装):

```sh
curl -fsSL https://github.com/bones7456/telemetrylite/releases/latest/download/install.sh -o install.sh
less install.sh
sh install.sh --dry-run
sh install.sh
```

最小安装只装二进制和 systemd 服务(监听 `127.0.0.1:8888`),不碰 nginx、不申请证书。此时可以:

- 用 SSH 端口转发先体验:`ssh -L 8888:127.0.0.1:8888 you@server`,再打开 `http://127.0.0.1:8888`;
- 自己配反代 / Caddy;
- 或让脚本代劳,见下一节。

**启用 HTTPS(`--domain`)**:让脚本写入 nginx 反代配置,并用 certbot 申请 Let's Encrypt 证书(代你同意其服务条款)。前提:域名已解析到服务器、80 端口可从公网访问、已装好 nginx 与 certbot(脚本不会替你装包):

```sh
sudo apt install -y nginx certbot python3-certbot-nginx     # Debian/Ubuntu
sh install.sh --dry-run --domain telemetry.example.com
sh install.sh --domain telemetry.example.com --email me@example.com
```

certbot 失败(如域名未解析好)时服务已装好、只是没有 HTTPS,脚本会给出补救命令。

脚本会探测环境并打印部署计划(系统/架构/权限/systemd/端口/反向代理),校验二进制的 SHA256,安装并启动服务,(指定 `--domain` 时)配置 nginx 与证书,最后打印**管理员密码**(仅首次显示,请立即保存)。选项与各探测项的处理方式见 [README](../README.zh-CN.md)「安装」。

**更新**:重新运行同一条命令(`sh install.sh`,需要的话带上同样的 `--domain`)。脚本会把已安装的二进制与发布版比较:已是最新就什么都不做(`--force` 可强制重装);否则下载并校验新二进制,把旧的备份为 `/usr/local/bin/telemetrylite.prev`,然后重启服务。**新版本起不来时,会自动恢复上一个二进制**,并以错误退出。数据库在启动时自动迁移(旧版二进制遇到更新版本写过的库会拒绝启动,所以回滚不会损坏数据)。已有的 service 文件和 nginx 配置不会被覆盖;如果 service 文件与安装脚本当前会生成的内容不同,会给出提示,`--rewrite-unit` 可重新生成(自定义设置建议用 `systemctl edit telemetrylite`)。重启期间 SDK 会缓存并重试事件;重启后需重新登录管理界面。没有 systemd 的机器只替换二进制,需要你自己重启进程。

**卸载**:`sh install.sh --uninstall`(保留 `/var/lib/telemetrylite` 数据)。

**从源码部署**(开发者):`./scripts/deploy.sh you@server telemetry.example.com`,在本机交叉编译后上传,服务器端同样调用 `install.sh`。需先 `brew install zig && cargo install cargo-zigbuild --locked && rustup target add x86_64-unknown-linux-musl`(Rust < 1.91 时 cargo-zigbuild 固定 `--version 0.20.1`)。

## 部署后

1. 浏览器打开 `https://telemetry.example.com/`,用管理员密码登录。页面右上角可切换界面语言(English / 中文)。
2. 新建 App →「设置」→「签发新 key」,**立即复制保存 key**(只显示一次),页面会给出接入代码。
3. 验证(把 `<key>` 换成刚签发的):

```sh
curl -i -X POST https://telemetry.example.com/k/<key>/v2/ -H 'content-type: application/json' \
  -d '[{"type":"Test.event","clientUser":"me","sessionID":"s1","isTestMode":"true","payload":{}}]'
```

返回 `200`;「事件」页能看到这条(🧪 为测试数据),「概览」切到「测试数据」可见。

## 接入 App

可以让 AI 编程 agent 代劳:复制 [AGENT_PROMPT.zh-CN.md](AGENT_PROMPT.zh-CN.md) 里的提示词。也可以手动接入:

**TelemetryDeck SDK(Swift)**,key 放在 base URL 路径里:

```swift
let config = TelemetryDeck.Config(
    appID: "<管理界面里的 App ID>",
    baseURL: URL(string: "https://telemetry.example.com/k/<key>/")!)
TelemetryDeck.initialize(config: config)
```

**OpenTelemetry(仅 Logs)**:

```
OTEL_EXPORTER_OTLP_LOGS_ENDPOINT=https://telemetry.example.com/v1/logs
OTEL_EXPORTER_OTLP_LOGS_PROTOCOL=http/protobuf
OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer <key>
```

key 只能写入、不能读取;泄露后在「设置」里吊销并重签即可,立即生效。

## 配置

用 `sudo systemctl edit telemetrylite` 加 `[Service]` + `Environment=...`,然后 `sudo systemctl restart telemetrylite`。

| 变量 | 默认 | 说明 |
|---|---|---|
| `TL_LISTEN` | `127.0.0.1:8888` | 监听地址;改端口需同步改 nginx 的 `proxy_pass` |
| `TL_DB_PATH` | `/var/lib/telemetrylite/telemetrylite.db` | SQLite 文件 |
| `TL_ADMIN_PASSWORD` | 首次随机生成 | 设置后每次启动都以它为准;**忘记密码就用它重置** |
| `TL_GEOIP_AUTO_UPDATE` | `true`(脚本写入) | 自动下载并每月更新 DB-IP City Lite(约 127 MB,需能访问 `download.db-ip.com`)。想关闭设为 `false` |
| `TL_GEOIP_DB` | 空 | 指定自备 mmdb;都不配则国家/城市为空 |
| `TL_TRUSTED_PROXY` | 空 | 除回环外额外信任的反代 IP(逗号分隔);nginx 与服务同机无需设置 |

每个 App 的数据保留天数在「设置」页修改(默认 90),后台每小时清理。

## 备份

数据全在 `/var/lib/telemetrylite/telemetrylite.db`(WAL 模式,**不要直接 cp**)。在线备份,无需停服:

```sh
sudo -u telemetrylite sqlite3 /var/lib/telemetrylite/telemetrylite.db ".backup '/var/lib/telemetrylite/backup.db'"
```

可放进 cron 每天执行并同步到别处。恢复:停服务,替换 db 文件(并删除同目录 `-wal`、`-shm`),再启动。`meta` 表里有用户哈希的盐,必须随库一起恢复。

## 排错

| 现象 | 排查 |
|---|---|
| 上报返回 `401` | key 错误/已吊销,或 URL 缺少 `/k/<key>/` |
| `400` | 请求体不是合法 JSON/protobuf;OTLP 注意 `Content-Type` 与编码一致 |
| `503` | 写入队列满,SDK 会按 `Retry-After` 重试;持续出现查磁盘 IO |
| `413` | 请求体超过 2 MB |
| 没有国家/城市 | 查 `journalctl -u telemetrylite \| grep GeoIP`(下载失败?);或 nginx 没传 `X-Forwarded-For` |
| 地理位置都是同一处 | nginx 前面还有 CDN,`$remote_addr` 是 CDN 地址,需用 nginx `real_ip` 模块还原真实 IP |
| 登录后马上掉线 | 用 HTTPS 访问,且 nginx 传了 `X-Forwarded-Proto` |
| 服务起不来 | `sudo journalctl -u telemetrylite -n 50`;常见:数据目录权限、端口占用 |

实时日志:`sudo journalctl -u telemetrylite -f`。

## 手动部署(不用脚本)

`scripts/install.sh` 就是完整的手动步骤,可直接阅读照做。

## 安全清单

- [ ] 服务只监听 `127.0.0.1`,公网只开放 80/443
- [ ] 全站 HTTPS;管理员密码足够长(脚本已对 `/api/login` 限速)
- [ ] 定期备份
- [ ] 隐私政策里如实说明:不存 IP、用户标识为哈希、地理位置精确到城市
- [ ] 保留 DB-IP 署名(管理界面页脚已带)
