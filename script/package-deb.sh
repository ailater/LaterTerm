#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
exec python3 -I "$ROOT/script/package-assets.py" deb "$@"
