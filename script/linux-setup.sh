#!/usr/bin/env bash
# LaterTerm Linux 构建依赖（需要 sudo 执行）
# 用法: sudo bash script/linux-setup.sh
set -euo pipefail

apt-get update
apt-get install -y \
    build-essential \
    pkg-config \
    libxkbcommon-dev \
    libfontconfig-dev \
    libfreetype-dev

echo "完成。回到项目目录执行: cargo check"
