#!/usr/bin/env bash
# LaterTerm 本地门禁 = CI 三件套，口径必须与 .github/workflows/ci.yml 一致，
# 缺任何一件必红 CI（fmt 已两次单独打红：2190671 / 455c0c5）。
#
# 用法：script/gate.sh        （rustup 工具链需 ~/.cargo/bin 前置，脚本已处理）
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
export PATH="$HOME/.cargo/bin:$PATH"

echo "[gate] 1/3 cargo fmt --all --check"
cargo fmt --all --check

echo "[gate] 2/3 cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

echo "[gate] 3/3 cargo test --workspace --lib --bins（隔离 XDG_CONFIG_HOME，与 CI 全新机器同构："
echo "        杜绝本机 ~/.config/laterterm 残留导致机器态相关假红，PR#4/#5 两连红教训）"
GATE_CONFIG_DIR="$(mktemp -d)"
trap 'rm -rf "$GATE_CONFIG_DIR"' EXIT
XDG_CONFIG_HOME="$GATE_CONFIG_DIR" cargo test --workspace --lib --bins

echo "[gate] 全部通过 ✓"
