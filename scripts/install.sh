#!/bin/sh
# TelemetryLite 安装脚本(POSIX sh)。USE ON YOUR OWN RISK.
#
#   curl -fsSL https://github.com/bones7456/telemetrylite/releases/latest/download/install.sh | sh -s -- --domain telemetry.example.com
#
# 先探测环境(系统/架构/权限/下载工具/systemd/端口/反向代理),打印部署计划,再按环境选择部署方式。
# 选项:
#   --domain D     配置 nginx 反代并申请 HTTPS 证书(需已装 nginx、certbot,域名已解析到本机)
#   --email E      certbot 注册邮箱(不给则无邮箱注册;使用 --domain 即代你同意 Let's Encrypt 服务条款)
#   --version V    安装指定版本,如 v0.1.0(默认最新)
#   --port N       服务监听 127.0.0.1:N(默认 8888)
#   --no-geoip     不启用 GeoIP 自动下载
#   --bin PATH     使用本地二进制而不是下载(从源码部署时用)
#   --dry-run      只探测环境并打印计划,不做任何修改
#   --yes          跳过开始前的 5 秒等待
#   --uninstall    卸载服务与二进制(保留 /var/lib/telemetrylite 数据)
set -eu

REPO="${TL_REPO:-__REPO__}"
DOMAIN=""; EMAIL=""; VERSION="latest"; PORT=8888; GEOIP=true; BIN=""
DRY=0; YES=0; UNINSTALL=0
UNIT=/etc/systemd/system/telemetrylite.service
NGINX_CONF=/etc/nginx/conf.d/telemetrylite.conf
DATA=/var/lib/telemetrylite

say()  { printf '%s\n' "$*"; }
warn() { printf '! %s\n' "$*" >&2; }
die()  { printf '✘ %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case $1 in
    --domain|--email|--version|--port|--bin) [ $# -ge 2 ] || die "$1 需要一个参数"; v=$2
      case $1 in --domain) DOMAIN=$v;; --email) EMAIL=$v;; --version) VERSION=$v;; --port) PORT=$v;; --bin) BIN=$v;; esac
      shift 2;;
    --no-geoip) GEOIP=false; shift;;
    --dry-run) DRY=1; shift;;
    --yes) YES=1; shift;;
    --uninstall) UNINSTALL=1; shift;;
    -h|--help) sed -n '2,19p' "$0" 2>/dev/null || say "用法见脚本开头的注释"; exit 0;;
    *) die "未知选项: $1";;
  esac
done

# 域名/邮箱/端口会写进配置文件,严格校验
case $DOMAIN in *[!A-Za-z0-9.-]*) die "域名含非法字符";; esac
case $EMAIL  in *[!A-Za-z0-9.@_+-]*) die "邮箱含非法字符";; esac
case $PORT   in ''|*[!0-9]*) die "端口必须是数字";; esac
case $VERSION in *[!A-Za-z0-9._-]*) die "版本号含非法字符";; esac

# ---------- 环境探测 ----------
[ "$(uname -s)" = Linux ] || die "仅支持 Linux(检测到 $(uname -s))"
case $(uname -m) in
  x86_64|amd64)  ARCH=x86_64;;
  aarch64|arm64) ARCH=aarch64;;
  *) die "不支持的 CPU 架构: $(uname -m)(仅 x86_64 / aarch64)";;
esac

if [ "$(id -u)" = 0 ]; then SUDO=""
elif command -v sudo >/dev/null 2>&1; then SUDO="sudo"
else die "需要 root 权限:请用 root 运行,或先安装 sudo"; fi

if command -v curl >/dev/null 2>&1; then dl() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then dl() { wget -qO "$2" "$1"; }
else die "需要 curl 或 wget"; fi

if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then INIT=systemd; else INIT=none; fi

MODE=install
if [ -f "$UNIT" ]; then
  MODE=upgrade
  p=$(sed -n 's/^Environment=TL_LISTEN=.*:\([0-9][0-9]*\)$/\1/p' "$UNIT" | head -n 1)
  [ -z "$p" ] || PORT=$p
fi

port_busy() {
  command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | awk '{print $4}' | grep -q ":$PORT\$"
}
if [ "$MODE" = install ] && [ "$UNINSTALL" = 0 ] && port_busy; then
  die "端口 $PORT 已被占用,请换一个: --port N"
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
    *) echo "用系统包管理器安装 nginx 与 certbot(含 nginx 插件)";;
  esac
}

# ---------- 卸载 ----------
if [ "$UNINSTALL" = 1 ]; then
  say "将卸载 telemetrylite(保留数据目录 $DATA)"
  [ "$DRY" = 1 ] && exit 0
  if [ "$INIT" = systemd ]; then
    $SUDO systemctl disable --now telemetrylite 2>/dev/null || true
    $SUDO rm -f "$UNIT"; $SUDO systemctl daemon-reload
  fi
  $SUDO rm -f /usr/local/bin/telemetrylite
  if [ -f "$NGINX_CONF" ]; then $SUDO rm -f "$NGINX_CONF"; $SUDO nginx -t >/dev/null 2>&1 && $SUDO systemctl reload nginx || true; fi
  say "✔ 已卸载。数据仍在 $DATA;如需彻底删除: sudo rm -rf $DATA"
  exit 0
fi

# ---------- 部署计划 ----------
say "== TelemetryLite 安装 =="
say "  ⚠ USE ON YOUR OWN RISK:本脚本将以 ${SUDO:-root} 权限修改系统(见下)"
say "环境:Linux/$ARCH · 权限: ${SUDO:-root} · 初始化系统: $INIT · 模式: $MODE"
say "来源:$( [ -n "$BIN" ] && echo "本地文件 $BIN" || echo "github.com/$REPO releases/$VERSION" )"
say "将执行:"
say "  - 安装二进制到 /usr/local/bin/telemetrylite"
if [ "$INIT" = systemd ]; then
  if [ "$MODE" = install ]; then
    say "  - 创建系统用户 telemetrylite、数据目录 $DATA、服务 $UNIT(监听 127.0.0.1:$PORT,GeoIP 自动下载=$GEOIP)"
  else
    say "  - 重启服务(保留现有 service 文件与配置)"
  fi
else
  say "  - 未检测到 systemd:只安装二进制,不会创建服务,结束后给出手动启动方式"
fi
if [ -n "$DOMAIN" ]; then
  case $PROXY in
    nginx) say "  - 写入 nginx 配置 $NGINX_CONF(域名 $DOMAIN,登录接口限速)"
           if [ "$HAS_CERTBOT" = 1 ]; then say "  - 用 certbot 申请 HTTPS 证书(同意 Let's Encrypt 服务条款)"
           else say "  - 未检测到 certbot:跳过证书申请,结束后给出命令"; fi;;
    caddy) say "  - 检测到 Caddy:不会改动你的 Caddyfile,结束后给出配置片段";;
    none)  say "  - 未检测到 nginx/caddy:不配置反向代理(请先安装: $(pkg_hint))";;
  esac
else
  say "  - 未指定 --domain:不配置反向代理与 HTTPS(服务仅监听本机)"
fi
[ "$DRY" = 1 ] && { say "(dry-run:未做任何修改)"; exit 0; }
[ "$YES" = 1 ] || { say "5 秒后开始,Ctrl-C 取消…"; sleep 5; }

# ---------- 取得二进制 ----------
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
if [ -n "$BIN" ]; then
  [ -f "$BIN" ] || die "找不到 $BIN"
  cp "$BIN" "$TMP/bin"
else
  case $REPO in __REPO__) die "未设置仓库名:请从 Release 下载本脚本,或设置环境变量 TL_REPO=owner/repo";; esac
  if [ "$VERSION" = latest ]; then base="https://github.com/$REPO/releases/latest/download"
  else base="https://github.com/$REPO/releases/download/$VERSION"; fi
  asset="telemetrylite-$ARCH-linux"
  say "下载 $asset …"
  dl "$base/$asset" "$TMP/bin" || die "下载失败: $base/$asset"
  dl "$base/SHA256SUMS" "$TMP/SHA256SUMS" || die "下载 SHA256SUMS 失败"
  want=$(grep " $asset\$" "$TMP/SHA256SUMS" | awk '{print $1}')
  if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$TMP/bin" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then got=$(shasum -a 256 "$TMP/bin" | awk '{print $1}')
  else die "缺少 sha256sum,无法校验下载内容"; fi
  [ -n "$want" ] && [ "$want" = "$got" ] || die "SHA256 校验失败,已中止"
  say "✔ SHA256 校验通过"
fi
chmod +x "$TMP/bin"

# ---------- 安装 ----------
if [ "$INIT" = systemd ]; then
  $SUDO systemctl stop telemetrylite 2>/dev/null || true   # 避免覆盖正在运行的二进制
fi
$SUDO install -m 755 "$TMP/bin" /usr/local/bin/telemetrylite
say "✔ 已安装 /usr/local/bin/telemetrylite"

if [ "$INIT" != systemd ]; then
  say ""
  say "未检测到 systemd,请自行托管进程,例如:"
  say "  mkdir -p /var/lib/telemetrylite && cd /var/lib/telemetrylite && TL_LISTEN=127.0.0.1:$PORT TL_DB_PATH=./telemetrylite.db TL_GEOIP_AUTO_UPDATE=$GEOIP /usr/local/bin/telemetrylite"
  say "首次启动会在日志里打印一次管理员密码(或设置 TL_ADMIN_PASSWORD)。"
  exit 0
fi

if [ "$MODE" = install ]; then
  getent passwd telemetrylite >/dev/null 2>&1 || {
    if command -v useradd >/dev/null 2>&1; then $SUDO useradd -r -m -d "$DATA" -s /usr/sbin/nologin telemetrylite
    else $SUDO adduser -S -h "$DATA" -s /sbin/nologin telemetrylite; fi
  }
  $SUDO mkdir -p "$DATA"; $SUDO chown telemetrylite "$DATA"
  cat <<UNIT_EOF | $SUDO tee "$UNIT" >/dev/null
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
Restart=always
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=$DATA

[Install]
WantedBy=multi-user.target
UNIT_EOF
fi

$SUDO systemctl daemon-reload
$SUDO systemctl enable telemetrylite >/dev/null 2>&1
$SUDO systemctl restart telemetrylite

i=0
until dl "http://127.0.0.1:$PORT/" /dev/null 2>/dev/null; do
  i=$((i + 1))
  if [ "$i" -ge 15 ]; then
    $SUDO journalctl -u telemetrylite -n 30 --no-pager || true
    die "服务未能在 15 秒内就绪,见上方日志"
  fi
  sleep 1
done
say "✔ 服务已运行 (127.0.0.1:$PORT)"

# ---------- 反向代理 ----------
URL="http://127.0.0.1:$PORT/"
if [ -n "$DOMAIN" ]; then
  case $PROXY in
    nginx)
      if ! grep -rqs 'conf\.d' /etc/nginx/nginx.conf; then
        warn "nginx.conf 未 include conf.d,已跳过自动配置。请手动加入反代(见 docs/DEPLOY.md)"
      else
        if [ ! -f "$NGINX_CONF" ]; then
          cat > "$TMP/nginx.conf" <<'NGINX_EOF'
limit_req_zone $binary_remote_addr zone=tl_login:1m rate=10r/m;

server {
    listen 80;
    server_name __DOMAIN__;
    client_max_body_size 2m;

    # 登录接口限速,防暴力破解
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
        proxy_set_header X-Forwarded-For $remote_addr;   # 覆盖而非追加,防止客户端伪造 IP
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
          say "✔ nginx 已配置 ($NGINX_CONF)"
          if $SUDO test -d "/etc/letsencrypt/live/$DOMAIN"; then URL="https://$DOMAIN/"
          elif [ "$HAS_CERTBOT" = 1 ]; then
            if [ -n "$EMAIL" ]; then set -- -m "$EMAIL"; else set -- --register-unsafely-without-email; fi
            if $SUDO certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos --redirect "$@"; then URL="https://$DOMAIN/"
            else warn "certbot 失败(域名是否已解析到本机?80 端口是否可达?)。修复后手动运行: sudo certbot --nginx -d $DOMAIN"; fi
          else warn "未安装 certbot,当前仅 HTTP。安装并签发: $(pkg_hint) && sudo certbot --nginx -d $DOMAIN"; fi
        else
          $SUDO rm -f "$NGINX_CONF"
          warn "nginx 配置检测未通过,已撤销自动生成的配置,请手动配置反代"
        fi
      fi;;
    caddy)
      say "检测到 Caddy,请把下面片段加入 Caddyfile 并 reload(Caddy 会自动处理 HTTPS):"
      say ""
      say "$DOMAIN {"
      say "    request_body { max_size 2MB }"
      say "    reverse_proxy 127.0.0.1:$PORT"
      say "}";;
    none) warn "未检测到 nginx 或 caddy,未配置反向代理。请先安装: $(pkg_hint)";;
  esac
fi

# ---------- 收尾 ----------
say ""
say "✔ 完成。管理界面: $URL"
if [ "$MODE" = install ]; then
  pw=$($SUDO journalctl -u telemetrylite --no-pager 2>/dev/null | grep '管理员密码' | tail -n 1 | sed 's/.*: //' || true)
  if [ -n "$pw" ]; then say "管理员密码(仅此一次显示,请保存): $pw"
  else say "管理员密码见: sudo journalctl -u telemetrylite | grep 管理员密码"; fi
fi
