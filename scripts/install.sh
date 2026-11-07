#!/bin/sh
# TelemetryLite installer (POSIX sh). USE ON YOUR OWN RISK.
# TelemetryLite 安装脚本(POSIX sh)。USE ON YOUR OWN RISK.
#
#   curl -fsSL https://github.com/bones7456/telemetrylite/releases/latest/download/install.sh | sh -s -- --domain telemetry.example.com
#
# It inspects the environment first (OS, CPU, privileges, downloader, systemd, port, reverse proxy),
# prints a plan, then deploys in the way that fits. Run with --help for the options.
# 先探测环境,打印部署计划,再按环境选择部署方式。选项见 --help。
#
# Output language: English by default; Chinese when your locale is zh_* or with --lang zh.
# 输出语言:默认英文;系统语言为 zh_* 或加 --lang zh 时为中文。
set -eu

REPO="${TL_REPO:-__REPO__}"
DOMAIN=""; EMAIL=""; VERSION="latest"; PORT=8888; GEOIP=true; BIN=""
DRY=0; YES=0; UNINSTALL=0; FORCE=0; REWRITE=0; GEOIP_SET=0; LANG_SET=0
BIN_PATH=/usr/local/bin/telemetrylite
L=en
case "${LC_ALL:-${LC_MESSAGES:-${LANG:-}}}" in zh*) L=zh;; esac
UNIT=/etc/systemd/system/telemetrylite.service
NGINX_CONF=/etc/nginx/conf.d/telemetrylite.conf
DATA=/var/lib/telemetrylite

# say/warn/die take (English, Chinese); with one argument the text is printed as is in both languages.
msg()  { if [ "$L" = zh ] && [ $# -ge 2 ]; then printf '%s' "$2"; else printf '%s' "$1"; fi; }
say()  { printf '%s\n' "$(msg "$@")"; }
warn() { printf '! %s\n' "$(msg "$@")" >&2; }
die()  { printf '✘ %s\n' "$(msg "$@")" >&2; exit 1; }

usage() {
  if [ "$L" = zh ]; then cat <<'USAGE_ZH'
用法: install.sh [选项]
  --domain D     配置 nginx 反代并申请 HTTPS 证书(需已装 nginx、certbot,域名已解析到本机)
  --email E      certbot 注册邮箱(不给则无邮箱注册;使用 --domain 即代你同意 Let's Encrypt 服务条款)
  --version V    安装指定版本,如 v0.1.0(默认最新)
  --port N       服务监听 127.0.0.1:N(默认 8888)
  --no-geoip     不启用 GeoIP 自动下载
  --bin PATH     使用本地二进制而不是下载(从源码部署时用)
  --lang en|zh   输出语言(默认按系统语言,否则英文)
  --force        即使已是最新版本也重新安装并重启
  --rewrite-unit 按当前设置重新生成 systemd service 文件(会覆盖你对它的手动修改,建议用 systemctl edit)
  --dry-run      只探测环境并打印计划,不做任何修改
  --yes          跳过开始前的 5 秒等待
  --uninstall    卸载服务与二进制(保留 /var/lib/telemetrylite 数据)
USAGE_ZH
  else cat <<'USAGE_EN'
Usage: install.sh [options]
  --domain D     configure an nginx reverse proxy and get an HTTPS certificate (needs nginx and certbot
                 installed, and the domain already pointing at this machine)
  --email E      certbot registration email (without it, registers without an email; using --domain
                 accepts the Let's Encrypt terms of service on your behalf)
  --version V    install a specific version, e.g. v0.1.0 (default: latest)
  --port N       listen on 127.0.0.1:N (default 8888)
  --no-geoip     do not enable automatic GeoIP download
  --bin PATH     use a local binary instead of downloading (for source deployments)
  --lang en|zh   output language (default: from your locale, else English)
  --force        reinstall and restart even if the installed version is already the latest
  --rewrite-unit regenerate the systemd service file from the current settings (overwrites hand edits; prefer `systemctl edit`)
  --dry-run      only inspect the environment and print the plan; change nothing
  --yes          skip the 5 second wait before starting
  --uninstall    remove the service and binary (keeps the /var/lib/telemetrylite data)
USAGE_EN
  fi
}

while [ $# -gt 0 ]; do
  case $1 in
    --domain|--email|--version|--port|--bin|--lang) [ $# -ge 2 ] || die "$1 needs a value" "$1 需要一个参数"; v=$2
      case $1 in --domain) DOMAIN=$v;; --email) EMAIL=$v;; --version) VERSION=$v;; --port) PORT=$v;; --bin) BIN=$v;;
        --lang) case $v in en|zh) L=$v; LANG_SET=1;; *) die "--lang must be en or zh" "--lang 只能是 en 或 zh";; esac;; esac
      shift 2;;
    --no-geoip) GEOIP=false; GEOIP_SET=1; shift;;
    --force) FORCE=1; shift;;
    --rewrite-unit) REWRITE=1; shift;;
    --dry-run) DRY=1; shift;;
    --yes) YES=1; shift;;
    --uninstall) UNINSTALL=1; shift;;
    -h|--help) usage; exit 0;;
    *) die "unknown option: $1" "未知选项: $1";;
  esac
done

# The domain, email and port end up in config files: validate strictly.
# 域名/邮箱/端口会写进配置文件,严格校验。
case $DOMAIN in *[!A-Za-z0-9.-]*) die "the domain contains illegal characters" "域名含非法字符";; esac
case $EMAIL  in *[!A-Za-z0-9.@_+-]*) die "the email contains illegal characters" "邮箱含非法字符";; esac
case $PORT   in ''|*[!0-9]*) die "the port must be a number" "端口必须是数字";; esac
case $VERSION in *[!A-Za-z0-9._-]*) die "the version contains illegal characters" "版本号含非法字符";; esac

# ---------- Environment detection / 环境探测 ----------
[ "$(uname -s)" = Linux ] || die "Linux only (detected $(uname -s))" "仅支持 Linux(检测到 $(uname -s))"
case $(uname -m) in
  x86_64|amd64)  ARCH=x86_64;;
  aarch64|arm64) ARCH=aarch64;;
  *) die "unsupported CPU architecture: $(uname -m) (x86_64 / aarch64 only)" "不支持的 CPU 架构: $(uname -m)(仅 x86_64 / aarch64)";;
esac

if [ "$(id -u)" = 0 ]; then SUDO=""
elif command -v sudo >/dev/null 2>&1; then SUDO="sudo"
else die "root privileges required: run as root, or install sudo first" "需要 root 权限:请用 root 运行,或先安装 sudo"; fi

if command -v curl >/dev/null 2>&1; then dl() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then dl() { wget -qO "$2" "$1"; }
else die "curl or wget is required" "需要 curl 或 wget"; fi

if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then INIT=systemd; else INIT=none; fi

# Installed = a binary or a service file exists (hosts without systemd have no service file).
# 已安装 = 存在二进制或 service 文件(没有 systemd 的机器没有 service 文件)。
MODE=install
if [ -f "$UNIT" ] || [ -x "$BIN_PATH" ]; then MODE=upgrade; fi
U_GEOIP=""; U_LANG=""
if [ -f "$UNIT" ]; then
  p=$(sed -n 's/^Environment=TL_LISTEN=.*:\([0-9][0-9]*\)$/\1/p' "$UNIT" | head -n 1)
  [ -z "$p" ] || PORT=$p
  U_GEOIP=$(sed -n 's/^Environment=TL_GEOIP_AUTO_UPDATE=//p' "$UNIT" | head -n 1)
  U_LANG=$(sed -n 's/^Environment=TL_LANG=//p' "$UNIT" | head -n 1)
  # an upgrade keeps the existing settings unless you pass an option explicitly
  [ "$GEOIP_SET" = 1 ] || [ -z "$U_GEOIP" ] || GEOIP=$U_GEOIP
fi
UNIT_LANG=$L; if [ "$LANG_SET" = 0 ] && [ -n "$U_LANG" ]; then UNIT_LANG=$U_LANG; fi

port_busy() {
  command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | awk '{print $4}' | grep -q ":$PORT\$"
}
if [ "$MODE" = install ] && [ "$UNINSTALL" = 0 ] && port_busy; then
  die "port $PORT is already in use, pick another with --port N" "端口 $PORT 已被占用,请换一个: --port N"
fi

PROXY=none
if [ -n "$DOMAIN" ]; then
  if command -v nginx >/dev/null 2>&1; then PROXY=nginx
  elif command -v caddy >/dev/null 2>&1; then PROXY=caddy; fi
fi
HAS_CERTBOT=0; command -v certbot >/dev/null 2>&1 && HAS_CERTBOT=1

pkg_hint() {
  # shellcheck disable=SC1091
  ids=$( . /etc/os-release 2>/dev/null; echo "${ID:-} ${ID_LIKE:-}" ) || ids=""
  case $ids in
    *debian*|*ubuntu*) echo "sudo apt install -y nginx certbot python3-certbot-nginx";;
    *rhel*|*fedora*|*centos*) echo "sudo dnf install -y nginx certbot python3-certbot-nginx";;
    *alpine*) echo "apk add nginx certbot certbot-nginx";;
    *) if [ "$L" = zh ]; then echo "用系统包管理器安装 nginx 与 certbot(含 nginx 插件)"; else echo "install nginx and certbot (with the nginx plugin) using your package manager"; fi;;
  esac
}

# ---------- Uninstall / 卸载 ----------
if [ "$UNINSTALL" = 1 ]; then
  say "Will uninstall telemetrylite (the data directory $DATA is kept)" "将卸载 telemetrylite(保留数据目录 $DATA)"
  [ "$DRY" = 1 ] && exit 0
  if [ "$INIT" = systemd ]; then
    $SUDO systemctl disable --now telemetrylite 2>/dev/null || true
    $SUDO rm -f "$UNIT"; $SUDO systemctl daemon-reload
  fi
  $SUDO rm -f /usr/local/bin/telemetrylite /usr/local/bin/telemetrylite.prev
  if [ -f "$NGINX_CONF" ]; then $SUDO rm -f "$NGINX_CONF"; $SUDO nginx -t >/dev/null 2>&1 && $SUDO systemctl reload nginx || true; fi
  say "✔ Uninstalled. Your data is still in $DATA; to remove it too: sudo rm -rf $DATA" "✔ 已卸载。数据仍在 $DATA;如需彻底删除: sudo rm -rf $DATA"
  exit 0
fi

# ---------- Versions / 版本比较 ----------
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
sha_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'; fi
}
ASSET="telemetrylite-$ARCH-linux"
if [ "$VERSION" = latest ]; then BASE="https://github.com/$REPO/releases/latest/download"
else BASE="https://github.com/$REPO/releases/download/$VERSION"; fi
CUR=""; [ -x "$BIN_PATH" ] && CUR=$(sha_of "$BIN_PATH")
WANT=""
if [ -z "$BIN" ] && [ "$MODE" = upgrade ]; then
  case $REPO in *__REPO*) ;; *)
    if dl "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null; then WANT=$(grep " $ASSET\$" "$TMP/SHA256SUMS" | awk '{print $1}'); fi;; esac
elif [ -n "$BIN" ] && [ -f "$BIN" ]; then WANT=$(sha_of "$BIN"); fi
UPTODATE=0
if [ "$MODE" = upgrade ] && [ "$FORCE" = 0 ] && [ "$REWRITE" = 0 ] && [ -n "$CUR" ] && [ "$WANT" = "$CUR" ]; then UPTODATE=1; fi
short() { printf '%s' "$1" | cut -c1-8; }

# The service file this installer would write (used for installs, --rewrite-unit and drift detection).
gen_unit() {
  cat <<UNIT_EOF
[Unit]
Description=TelemetryLite
After=network.target

[Service]
User=telemetrylite
WorkingDirectory=$DATA
ExecStart=/usr/local/bin/telemetrylite
Environment=TL_LISTEN=127.0.0.1:$PORT
Environment=TL_DB_PATH=$DATA/telemetrylite.db
Environment=TL_GEOIP_AUTO_UPDATE=$GEOIP
Environment=TL_LANG=$UNIT_LANG
Restart=always
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=$DATA

[Install]
WantedBy=multi-user.target
UNIT_EOF
}

# ---------- Plan / 部署计划 ----------
say "== TelemetryLite install ==" "== TelemetryLite 安装 =="
say "  ⚠ USE ON YOUR OWN RISK: this script modifies the system with ${SUDO:-root} privileges (see below)" "  ⚠ USE ON YOUR OWN RISK:本脚本将以 ${SUDO:-root} 权限修改系统(见下)"
say "Environment: Linux/$ARCH · privileges: ${SUDO:-root} · init system: $INIT · mode: $MODE" "环境:Linux/$ARCH · 权限: ${SUDO:-root} · 初始化系统: $INIT · 模式: $MODE"
if [ -n "$BIN" ]; then say "Source: local file $BIN" "来源:本地文件 $BIN"; else say "Source: github.com/$REPO releases/$VERSION" "来源:github.com/$REPO releases/$VERSION"; fi
if [ "$MODE" = upgrade ]; then
  if [ "$UPTODATE" = 1 ]; then
    say "Already up to date: the installed binary ($(short "$CUR")) matches the release. Nothing to do (use --force to reinstall)." "已是最新版本:已安装的二进制($(short "$CUR"))与发布版一致,无需操作(如需重新安装请加 --force)。"
    exit 0
  elif [ -n "$WANT" ] && [ -n "$CUR" ]; then
    say "Update: installed $(short "$CUR") -> $(short "$WANT")" "更新:已安装 $(short "$CUR") -> $(short "$WANT")"
  elif [ -z "$BIN" ]; then
    say "Could not check the latest version; the release will be downloaded." "无法检查最新版本,将直接下载发布版。"
  fi
fi
say "Will do:" "将执行:"
say "  - install the binary to /usr/local/bin/telemetrylite (a copy of the current one is kept as telemetrylite.prev)" "  - 安装二进制到 /usr/local/bin/telemetrylite(当前版本会备份为 telemetrylite.prev)"
if [ "$INIT" = systemd ]; then
  if [ "$MODE" = install ]; then
    say "  - create the system user telemetrylite, the data directory $DATA and the service $UNIT (listening on 127.0.0.1:$PORT, GeoIP auto download=$GEOIP)" "  - 创建系统用户 telemetrylite、数据目录 $DATA、服务 $UNIT(监听 127.0.0.1:$PORT,GeoIP 自动下载=$GEOIP)"
  else
    say "  - restart the service (existing service file and settings are kept); if the new version does not start, the previous binary is restored automatically" "  - 重启服务(保留现有 service 文件与配置);新版本起不来时自动恢复上一个二进制"
  fi
else
  say "  - no systemd detected: only the binary is installed, no service is created; start instructions are printed at the end" "  - 未检测到 systemd:只安装二进制,不会创建服务,结束后给出手动启动方式"
fi
if [ -n "$DOMAIN" ]; then
  case $PROXY in
    nginx) say "  - write the nginx config $NGINX_CONF (domain $DOMAIN, login endpoint rate-limited)" "  - 写入 nginx 配置 $NGINX_CONF(域名 $DOMAIN,登录接口限速)"
           if [ "$HAS_CERTBOT" = 1 ]; then say "  - get an HTTPS certificate with certbot (accepting the Let's Encrypt terms of service)" "  - 用 certbot 申请 HTTPS 证书(同意 Let's Encrypt 服务条款)"
           else say "  - certbot not found: certificate issuance is skipped, the command is printed at the end" "  - 未检测到 certbot:跳过证书申请,结束后给出命令"; fi;;
    caddy) say "  - Caddy detected: your Caddyfile is not touched, a snippet is printed at the end" "  - 检测到 Caddy:不会改动你的 Caddyfile,结束后给出配置片段";;
    none)  say "  - neither nginx nor caddy found: no reverse proxy is configured (install one first: $(pkg_hint))" "  - 未检测到 nginx/caddy:不配置反向代理(请先安装: $(pkg_hint))";;
  esac
else
  say "  - no --domain given: no reverse proxy / HTTPS is configured (the service only listens locally)" "  - 未指定 --domain:不配置反向代理与 HTTPS(服务仅监听本机)"
fi
[ "$DRY" = 1 ] && { say "(dry-run: nothing was changed)" "(dry-run:未做任何修改)"; exit 0; }
[ "$YES" = 1 ] || { say "Starting in 5 seconds, press Ctrl-C to cancel..." "5 秒后开始,Ctrl-C 取消…"; sleep 5; }

# ---------- Get the binary / 取得二进制 ----------
if [ -n "$BIN" ]; then
  [ -f "$BIN" ] || die "cannot find $BIN" "找不到 $BIN"
  cp "$BIN" "$TMP/bin"
else
  case $REPO in *__REPO*) die "repository name not set: download this script from a Release, or set TL_REPO=owner/repo" "未设置仓库名:请从 Release 下载本脚本,或设置环境变量 TL_REPO=owner/repo";; esac
  say "Downloading $ASSET ..." "下载 $ASSET …"
  dl "$BASE/$ASSET" "$TMP/bin" || die "download failed: $BASE/$ASSET" "下载失败: $BASE/$ASSET"
  dl "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" || die "failed to download SHA256SUMS" "下载 SHA256SUMS 失败"
  want=$(grep " $ASSET\$" "$TMP/SHA256SUMS" | awk '{print $1}')
  got=$(sha_of "$TMP/bin")
  [ -n "$got" ] || die "sha256sum is missing, cannot verify the download" "缺少 sha256sum,无法校验下载内容"
  [ -n "$want" ] && [ "$want" = "$got" ] || die "SHA256 verification failed, aborting" "SHA256 校验失败,已中止"
  say "✔ SHA256 verified" "✔ SHA256 校验通过"
fi
chmod +x "$TMP/bin"

# ---------- Install / 安装 ----------
if [ "$INIT" = systemd ]; then
  $SUDO systemctl stop telemetrylite 2>/dev/null || true   # do not overwrite a running binary
fi
# keep the current binary so a failed update can be rolled back
HAVE_PREV=0
if [ -x "$BIN_PATH" ] && $SUDO cp -p "$BIN_PATH" "$BIN_PATH.prev"; then HAVE_PREV=1; fi
$SUDO install -m 755 "$TMP/bin" "$BIN_PATH"
if [ "$MODE" = upgrade ]; then say "✔ Updated /usr/local/bin/telemetrylite" "✔ 已更新 /usr/local/bin/telemetrylite"
else say "✔ Installed /usr/local/bin/telemetrylite" "✔ 已安装 /usr/local/bin/telemetrylite"; fi

if [ "$INIT" != systemd ] && [ "$MODE" = upgrade ]; then
  say ""
  say "No systemd detected: restart your running telemetrylite process to use the new version. The previous binary is kept as $BIN_PATH.prev." "未检测到 systemd:请重启正在运行的 telemetrylite 进程以使用新版本。上一个二进制保留为 $BIN_PATH.prev。"
  exit 0
fi
if [ "$INIT" != systemd ]; then
  say ""
  say "No systemd detected, run the process under your own supervisor, for example:" "未检测到 systemd,请自行托管进程,例如:"
  say "  mkdir -p /var/lib/telemetrylite && cd /var/lib/telemetrylite && TL_LISTEN=127.0.0.1:$PORT TL_DB_PATH=./telemetrylite.db TL_GEOIP_AUTO_UPDATE=$GEOIP TL_LANG=$L /usr/local/bin/telemetrylite"
  say "On first start it logs the admin password once (or set TL_ADMIN_PASSWORD)." "首次启动会在日志里打印一次管理员密码(或设置 TL_ADMIN_PASSWORD)。"
  exit 0
fi

if [ ! -f "$UNIT" ]; then
  getent passwd telemetrylite >/dev/null 2>&1 || {
    if command -v useradd >/dev/null 2>&1; then $SUDO useradd -r -m -d "$DATA" -s /usr/sbin/nologin telemetrylite
    else $SUDO adduser -S -h "$DATA" -s /sbin/nologin telemetrylite; fi
  }
  $SUDO mkdir -p "$DATA"; $SUDO chown telemetrylite "$DATA"
  gen_unit | $SUDO tee "$UNIT" >/dev/null
fi

if [ "$MODE" = upgrade ] && [ -f "$UNIT" ]; then
  gen_unit > "$TMP/unit.new"
  if [ "$REWRITE" = 1 ]; then
    $SUDO install -m 644 "$TMP/unit.new" "$UNIT"
    say "✔ Service file regenerated ($UNIT)" "✔ 已重新生成 service 文件($UNIT)"
  elif ! cmp -s "$TMP/unit.new" "$UNIT"; then
    say "Note: $UNIT differs from what this installer would write (hand edits or an older installer). It was kept as is. Use --rewrite-unit to regenerate it; for custom settings prefer: systemctl edit telemetrylite" "提示:$UNIT 与当前安装脚本生成的内容不同(手动修改过,或来自旧版脚本),已保持不变。如需重新生成请加 --rewrite-unit;自定义设置建议用 systemctl edit telemetrylite"
  fi
fi
$SUDO systemctl daemon-reload
$SUDO systemctl enable telemetrylite >/dev/null 2>&1
$SUDO systemctl restart telemetrylite

i=0
until dl "http://127.0.0.1:$PORT/" /dev/null 2>/dev/null; do
  i=$((i + 1))
  if [ "$i" -ge 15 ]; then
    $SUDO journalctl -u telemetrylite -n 30 --no-pager || true
    if [ "$MODE" = upgrade ] && [ "$HAVE_PREV" = 1 ]; then
      warn "the new version did not start: rolling back to the previous binary" "新版本未能启动:正在回滚到上一个二进制"
      $SUDO systemctl stop telemetrylite 2>/dev/null || true
      $SUDO cp -p "$BIN_PATH.prev" "$BIN_PATH"
      $SUDO systemctl restart telemetrylite
      sleep 3
      if dl "http://127.0.0.1:$PORT/" /dev/null 2>/dev/null; then
        die "update failed and was rolled back: the previous version is running again (see the log above)" "更新失败,已回滚:上一个版本已重新运行(见上方日志)"
      fi
      die "update failed and the rollback did not bring the service back, see: journalctl -u telemetrylite" "更新失败,回滚后服务仍未恢复,请查看: journalctl -u telemetrylite"
    fi
    die "the service did not become ready within 15 seconds, see the log above" "服务未能在 15 秒内就绪,见上方日志"
  fi
  sleep 1
done
say "✔ Service running (127.0.0.1:$PORT)" "✔ 服务已运行 (127.0.0.1:$PORT)"

# ---------- Reverse proxy / 反向代理 ----------
URL="http://127.0.0.1:$PORT/"
if [ -n "$DOMAIN" ]; then
  case $PROXY in
    nginx)
      if ! grep -rqs 'conf\.d' /etc/nginx/nginx.conf; then
        warn "nginx.conf does not include conf.d, skipped the automatic setup. Add the reverse proxy by hand (see docs/DEPLOY.md)" "nginx.conf 未 include conf.d,已跳过自动配置。请手动加入反代(见 docs/DEPLOY.zh-CN.md)"
      else
        if [ ! -f "$NGINX_CONF" ]; then
          cat > "$TMP/nginx.conf" <<'NGINX_EOF'
limit_req_zone $binary_remote_addr zone=tl_login:1m rate=10r/m;

server {
    listen 80;
    server_name __DOMAIN__;
    client_max_body_size 2m;

    # rate-limit the login endpoint against brute force
    location = /api/login {
        limit_req zone=tl_login burst=5 nodelay;
        proxy_pass http://127.0.0.1:__PORT__;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    location / {
        proxy_pass http://127.0.0.1:__PORT__;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;   # overwrite, never append: stops clients spoofing their IP
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
NGINX_EOF
          sed -i "s/__DOMAIN__/$DOMAIN/; s/__PORT__/$PORT/g" "$TMP/nginx.conf"
          $SUDO install -m 644 "$TMP/nginx.conf" "$NGINX_CONF"
        fi
        if $SUDO nginx -t >/dev/null 2>&1; then
          $SUDO systemctl reload nginx
          URL="http://$DOMAIN/"
          say "✔ nginx configured ($NGINX_CONF)" "✔ nginx 已配置 ($NGINX_CONF)"
          if $SUDO test -d "/etc/letsencrypt/live/$DOMAIN"; then URL="https://$DOMAIN/"
          elif [ "$HAS_CERTBOT" = 1 ]; then
            if [ -n "$EMAIL" ]; then set -- -m "$EMAIL"; else set -- --register-unsafely-without-email; fi
            if $SUDO certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos --redirect "$@"; then URL="https://$DOMAIN/"
            else warn "certbot failed (does the domain resolve to this machine? is port 80 reachable?). Once fixed, run: sudo certbot --nginx -d $DOMAIN" "certbot 失败(域名是否已解析到本机?80 端口是否可达?)。修复后手动运行: sudo certbot --nginx -d $DOMAIN"; fi
          else warn "certbot is not installed, HTTP only for now. Install it and get a certificate: $(pkg_hint) && sudo certbot --nginx -d $DOMAIN" "未安装 certbot,当前仅 HTTP。安装并签发: $(pkg_hint) && sudo certbot --nginx -d $DOMAIN"; fi
        else
          $SUDO rm -f "$NGINX_CONF"
          warn "the nginx config test failed, the generated config was removed. Configure the reverse proxy by hand" "nginx 配置检测未通过,已撤销自动生成的配置,请手动配置反代"
        fi
      fi;;
    caddy)
      say "Caddy detected: add this snippet to your Caddyfile and reload (Caddy handles HTTPS by itself):" "检测到 Caddy,请把下面片段加入 Caddyfile 并 reload(Caddy 会自动处理 HTTPS):"
      say ""
      say "$DOMAIN {"
      say "    request_body { max_size 2MB }"
      say "    reverse_proxy 127.0.0.1:$PORT"
      say "}";;
    none) warn "neither nginx nor caddy found, no reverse proxy configured. Install one first: $(pkg_hint)" "未检测到 nginx 或 caddy,未配置反向代理。请先安装: $(pkg_hint)";;
  esac
fi

# ---------- Done / 收尾 ----------
say ""
say "✔ Done. Admin UI: $URL" "✔ 完成。管理界面: $URL"
if [ "$MODE" = install ]; then
  pw=$($SUDO journalctl -u telemetrylite --no-pager 2>/dev/null | grep -E 'admin password|管理员密码' | tail -n 1 | sed 's/.*: //' || true)
  if [ -n "$pw" ]; then say "Admin password (shown only this once, save it): $pw" "管理员密码(仅此一次显示,请保存): $pw"
  else say "Admin password: sudo journalctl -u telemetrylite | grep -iE 'admin password|管理员密码'" "管理员密码见: sudo journalctl -u telemetrylite | grep -iE 'admin password|管理员密码'"; fi
fi
