#!/usr/bin/env bash
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
PORT="${PORT:-15162}"
exec node "$DIR/demergi/bin/demergi.js" \
  -A "127.0.0.1:${PORT}" \
  -H linux.do \
  -l info \
  --dns-mode plain \
  --dns-ip-overrides "$DIR/linuxdo_dpi_overrides.json" \
  --https-clienthello-size 40 \
  --https-clienthello-tlsv 1.3
