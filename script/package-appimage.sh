#!/usr/bin/env bash
# AppImage 组装：AppDir 暂存复用仓库同一份 desktop 与 hicolor 图标
#（兑现 platforms.md「后续复用同一 desktop 和图标目录，不另复制源图」承诺）。
#
# 用法:
#   script/package-appimage.sh --binary <可执行文件> --output <绝对输出目录> \
#     [--linuxdeploy <钉版 linuxdeploy AppImage 路径>] [--version <SemVer>] [--dry-run]
#
# - --linuxdeploy 传入 linuxdeploy AppImage 路径（CI 下载钉版 release 后使用；
#   runner 无 FUSE，必须以 --appimage-extract-and-run 运行，本脚本已内置）。
# - 未提供 --linuxdeploy 或显式 --dry-run 时，只做 AppDir 暂存 + desktop 校验
#   （本地门禁/无工具链环境的降级路径），绝不产出 .AppImage。
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
usage() {
  echo "用法: $0 --binary <可执行文件> --output <绝对输出目录> [--linuxdeploy <路径>] [--version <SemVer>] [--dry-run]" >&2
}

BINARY=""
OUTPUT=""
LINUXDEPLOY=""
VERSION=""
DRY_RUN=0
while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BINARY="${2:-}"; shift 2 ;;
    --output) OUTPUT="${2:-}"; shift 2 ;;
    --linuxdeploy) LINUXDEPLOY="${2:-}"; shift 2 ;;
    --version) VERSION="${2:-}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    *) usage; exit 2 ;;
  esac
done
[ -n "$BINARY" ] && [ -n "$OUTPUT" ] || { usage; exit 2; }
case "$OUTPUT" in /*) ;; *) echo "错误: --output 必须为绝对路径" >&2; exit 1 ;; esac
if [ ! -f "$BINARY" ] || [ ! -x "$BINARY" ]; then
  echo "错误: --binary 必须是可执行的普通文件: $BINARY" >&2
  exit 1
fi

APPDIR="$OUTPUT/LaterTerm.AppDir"
if [ -e "$APPDIR" ]; then
  echo "错误: 拒绝覆盖已有 AppDir: $APPDIR" >&2
  exit 1
fi

# --- AppDir 暂存：desktop 模板本身 Exec=laterterm / Icon=laterterm，
# --- AppRun 会把 usr/bin 注入 PATH，模板无需改写即可复用
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications"
install -m 0755 "$BINARY" "$APPDIR/usr/bin/laterterm"
cp "$ROOT/packaging/linux/laterterm.desktop" "$APPDIR/usr/share/applications/laterterm.desktop"
for SIZE in 16 24 32 48 64 128 256 512 1024; do
  mkdir -p "$APPDIR/usr/share/icons/hicolor/${SIZE}x${SIZE}/apps"
  cp "$ROOT/assets/icons/png/laterterm-${SIZE}.png" \
    "$APPDIR/usr/share/icons/hicolor/${SIZE}x${SIZE}/apps/laterterm.png"
done
# 不合规的 .desktop 会被启动器静默丢弃（LaterMD linux-deb.yml 同款教训），
# 这里当场红；工具来自 desktop-file-utils（ci.yml 已装，22.04 runner 预装）
if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$APPDIR/usr/share/applications/laterterm.desktop"
else
  echo "错误: desktop-file-validate 不可用，无法校验 AppDir 元数据" >&2
  exit 1
fi

if [ "$DRY_RUN" = 1 ] || [ -z "$LINUXDEPLOY" ]; then
  echo "AppDir 暂存完成: $APPDIR（dry-run / 未提供 --linuxdeploy，不产出 .AppImage）"
  exit 0
fi

if [ ! -f "$LINUXDEPLOY" ]; then
  echo "错误: --linuxdeploy 不是文件: $LINUXDEPLOY" >&2
  exit 1
fi
# 相对路径在下方 cd "$OUTPUT" 后会失效，先转绝对（调用方传 $PWD 形态，此处兜底）
case "$LINUXDEPLOY" in
  /*) ;;
  *) LINUXDEPLOY="$(cd "$(dirname "$LINUXDEPLOY")" && pwd)/$(basename "$LINUXDEPLOY")" ;;
esac
chmod +x "$LINUXDEPLOY"
# linuxdeploy 从 AppDir 内的 desktop/图标自行推断入口，无需 -e/-d/-i；
# --appimage-extract-and-run 是无 FUSE runner 上的标准跑法
cd "$OUTPUT"
"$LINUXDEPLOY" --appimage-extract-and-run --appdir "$APPDIR" --output appimage
if [ -n "$VERSION" ]; then
  PRODUCED=$(find "$OUTPUT" -maxdepth 1 -name "*.AppImage" -type f | head -1)
  if [ -n "$PRODUCED" ]; then
    mv "$PRODUCED" "$OUTPUT/laterterm_${VERSION}_x86_64.AppImage"
  else
    echo "错误: linuxdeploy 未产出 .AppImage" >&2
    exit 1
  fi
fi
echo "AppImage 组装完成: $OUTPUT"
