#!/usr/bin/env bash
# 从源码部署(开发者用): 本机编译 → 上传 → 在服务器上用 install.sh 安装。
#   ./scripts/deploy.sh user@host [域名] [certbot 邮箱]
# 已编译过可加 SKIP_BUILD=1。普通用户请用 Release 里的 install.sh(见 README)。
set -euo pipefail
cd "$(dirname "$0")/.."

HOST=${1:?用法: $0 user@host [域名] [certbot 邮箱]}
DOMAIN=${2:-}
EMAIL=${3:-}

[ -n "${SKIP_BUILD:-}" ] || ./scripts/release.sh
ARGS="--yes --bin /tmp/telemetrylite"
[ -z "$DOMAIN" ] || ARGS="$ARGS --domain '$DOMAIN'"
[ -z "$EMAIL" ] || ARGS="$ARGS --email '$EMAIL'"
scp dist/telemetrylite scripts/install.sh "$HOST":/tmp/
ssh -t "$HOST" "sh /tmp/install.sh $ARGS; rc=\$?; rm -f /tmp/install.sh /tmp/telemetrylite; exit \$rc"
