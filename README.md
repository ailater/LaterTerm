<div align="center">

# 🚧 LaterTerm

[![Rust](https://img.shields.io/badge/Rust-1.85+-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20macOS-lightgrey)

**跨平台 GPU 终端 · Rust + GPUI · 快捷命令面板 · 远程服务器管理**

为中文开发者打造的原生终端：Deepin 终端的功能 × Tabby 的颜值 × 原生的性能。

[特性](#特性) · [路线图](#路线图) · [技术栈](#技术栈) · [开发](#开发)

</div>

> 🚧 **项目处于早期开发阶段（M1）**，首个可用版本将在 M1 里程碑后发布。

## 特性（规划）

### 终端本体

- ⚡ **GPU 渲染** — GPUI 文本管线，连字 / CJK / emoji / HiDPI 原生支持
- 🧩 **会话抽象** — 本地 PTY 与 SSH 远程会话共用一套交互，远程不是"另一个功能"
- 🔍 **深度回滚** — 10 万行滚动缓冲 + 全文搜索

### Deepin 功能对齐

- ⌨️ **快捷命令面板** — 参数化命令模板、内建 TOTP（跳板机二次验证一键完成）、每命令独立快捷键
- 🌐 **远程服务器管理** — 主机分组、凭据入系统密钥环、SFTP 文件面板、跳板机链式跳转
- 📦 **rz/sz 传输** — 兼容 trzsz 协议
- 🪟 分屏 / 广播输入 / Quake 下拉模式 / 主题热加载

## 路线图

| 里程碑 | 内容 | 状态 |
|--------|------|------|
| M0 | 窗口 + 本地会话 + 网格渲染，vim / htop / tmux 无残影 | ✅ 完成 |
| M1 | 多标签 ✅ · 主题热加载 ✅ · 设置面板 ✅ · 分屏 / 搜索回滚 / 中文 IME 🚧 | 🚧 进行中 |
| M2 | 快捷命令面板 + OTP + 每命令快捷键 | ⏳ |
| M3 | SSH 远程管理 + 跳板机链 + SFTP + GBK 转码 | ⏳ |
| M4 | 三平台打包、设置界面、动效打磨 | ⏳ |

> 任务清单、验收标准与风险台账见 [docs/roadmap.md](docs/roadmap.md)。

## 技术栈

| 层 | 选型 |
|----|------|
| UI 框架 | [GPUI](https://github.com/zed-industries/zed)（Zed 同款渲染） |
| VT 仿真 | [alacritty_terminal](https://github.com/alacritty/alacritty) |
| PTY | portable-pty |
| SSH / SFTP | russh / russh-sftp |
| 凭据 | 系统密钥环（keyring） |

## 开发

```bash
git clone git@github.com:crazykun/LaterTerm.git
cd LaterTerm
cargo run --release
```

> 架构决策与技术文档见 `docs/`（随 M0 落地）。

## 相关项目

| 项目 | 说明 |
|------|------|
| [LaterScreen](https://github.com/crazykun/LaterScreen) | 跨平台截图标注工具（OCR / 录屏 / 贴图） |
| [LaterMd](https://github.com/ailater/LaterMd) | Markdown 知识工作台 |

## License

[MIT](LICENSE) © 2026 crazykun
