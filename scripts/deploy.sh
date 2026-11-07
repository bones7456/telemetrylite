#!/usr/bin/env bash
# Deploy from source (for developers): build locally -> upload -> install on the server with install.sh.
# 从源码部署(开发者用): 本机编译 -> 上传 -> 在服务器上用 install.sh 安装。
#   ./scripts/deploy.sh user@host [domain] [certbot email]
# Add SKIP_BUILD=1 if the binary is already built. Everyone else should use install.sh from the Release (see README).
# 已编译过可加 SKIP_BUILD=1。普通用户请用 Release 里的 install.sh(见 README)。
set -euo pipefail
cd "$(dirname "$0")/.."

# Output language follows the local locale (English by default) / 输出语言跟随本机语言(默认英文)
LANG_OPT=en
case "${LC_ALL:-${LC_MESSAGES:-${LANG:-}}}" in zh*) LANG_OPT=zh;; esac
if [ $# -lt 1 ]; then
  if [ "$LANG_OPT" = zh ]; then echo "用法: $0 user@host [域名] [certbot 邮箱]" >&2; else echo "Usage: $0 user@host [domain] [certbot email]" >&2; fi
  exit 1
fi
HOST=$1
DOMAIN=${2:-}
EMAIL=${3:-}

[ -n "${SKIP_BUILD:-}" ] || ./scripts/release.sh
ARGS="--yes --lang $LANG_OPT --bin /tmp/telemetrylite"
[ -z "$DOMAIN" ] || ARGS="$ARGS --domain '$DOMAIN'"
[ -z "$EMAIL" ] || ARGS="$ARGS --email '$EMAIL'"
scp dist/telemetrylite scripts/install.sh "$HOST":/tmp/
ssh -t "$HOST" "sh /tmp/install.sh $ARGS; rc=\$?; rm -f /tmp/install.sh /tmp/telemetrylite; exit \$rc"
