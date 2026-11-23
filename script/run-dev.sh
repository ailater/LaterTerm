#!/usr/bin/env bash
# 开发期启动器：以 .app bundle 形态运行 target/debug/laterterm。
# cargo run 裸跑二进制时 macOS 找不到 Info.plist/.icns，Dock 只能给默认图标；
# 本脚本组装一个 dev bundle（bundle 标识带 .dev 后缀，与正式安装版互不干扰），
# 二进制用符号链接——改码后重跑本脚本即用最新构建，无需重新组装。
# stdout/stderr 留在终端（直接 exec bundle 内二进制，不经 open）。
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

export PATH="$HOME/.cargo/bin:$PATH"
cargo build

BIN="$ROOT/target/debug/laterterm"
[ -x "$BIN" ] || { echo "构建产物不存在: $BIN" >&2; exit 1; }

VERSION=$(python3 -I -c "import tomllib; print(tomllib.load(open('Cargo.toml','rb'))['workspace']['package']['version'])")

APP="target/dev-app/LaterTerm.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
ln -s "$BIN" "$APP/Contents/MacOS/laterterm"
cp assets/icons/laterterm.icns "$APP/Contents/Resources/laterterm.icns"
sed "s/@VERSION@/$VERSION/; s|<string>io.github.crazykun.LaterTerm</string>|<string>io.github.crazykun.LaterTerm.dev</string>|" \
  packaging/macos/Info.plist.in > "$APP/Contents/Info.plist"

exec "$APP/Contents/MacOS/laterterm" "$@"
