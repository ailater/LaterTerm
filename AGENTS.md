# AGENTS.md — LaterTerm

跨平台 GPU 终端（Rust + GPUI）。目标：Deepin 终端功能 × Tabby 颜值 × 原生性能。

## 工作区

- `crates/laterterm-app` — 主程序二进制 `laterterm`
- `crates/laterterm-termview` — GPUI 终端网格视图（**内化自 zortax/gpui-terminal 0.1.0**，上游停更，由本仓库接管；改动需保持 MIT OR Apache-2.0 许可头）
- `crates/laterterm-config` — 设置与主题持久化（TOML：`~/.config/laterterm/settings.toml` + `themes/*.toml`，自定义主题同名覆盖内置；app 层用 notify+flume 做热加载）
- 后续里程碑再增：`laterterm-core`（TerminalSession 会话抽象/编码/trzsz）、`laterterm-local`、`laterterm-ssh`（russh）、`laterterm-secrets`

## 命令

```bash
cargo check          # 快速校验
cargo run --release  # 运行（需要桌面环境）
cargo clippy --fix   # lint
```

## 约定

- 语言：Rust（edition 2024），注释与 UI 文案用中文
- 提交信息：`类型: 摘要`（如 `feat: xxx`、`fix: xxx`、`init: xxx`）
- 主题配色统一走 `ColorPalette::builder()`，预设与 LaterMD/LaterScreen 家族一致（Tokyo Night 起步）
- VT 仿真层禁止自造轮子：一律基于 alacritty_terminal 的 Grid/Term
- 平台差异代码必须 `#[cfg(target_os)]` 隔离，Linux 优先（X11 + Wayland 双栈）

## 当前阶段

M1（交互完善）：配置持久化、主题热加载、设置面板、回滚搜索、TerminalSession
会话抽象（laterterm-core 契约 + laterterm-local PTY 实现）、中文 IME 预编辑
（浮层+候选框光标跟随）均已落地。
剩余任务按序：分屏树 → 字体配置面。
验收：拔掉原终端用一整天无不适，IME/分屏在 4K 缩放下不糊不跳。
明细见 docs/roadmap.md；测试门禁口径 `cargo test --workspace --lib --bins`
（termview 上游遗留 doctest 需 GPUI 运行时，本机必挂，见 roadmap 注记）。
