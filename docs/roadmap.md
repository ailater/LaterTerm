# LaterTerm 路线图

> 完整版规划（README 表格是摘要）。定稿 2026-10-28；随里程碑推进勾选更新。
> 架构分层与选型理由见 [architecture.md](architecture.md)。

## 愿景与范围

**跨平台 GPU 终端**：Deepin 终端的功能 × Tabby 的颜值 × 原生的性能。
为中文开发者打造——IME / CJK / 跳板机 / GBK 是一等公民，不是适配项。

**明确不做**（防范围蔓延，与 LaterMD「明确不做」同纪律）：

- 不做终端复用器——tmux/zellij 是独立产品，LaterTerm 只做好「宿主终端」
- 不做 AI 助手内嵌（Warp 路线）——AI 属于 ZCode/编辑器层
- 不做 WebGL/浏览器外壳——GPUI 原生渲染是立身之本
- 不自造 VT 仿真——永远基于 alacritty_terminal

## 里程碑总览与当前状态

```
M0 骨架 ✅ → M1 交互完善 🚧(本阶段) → M2 快捷命令 ⏳ → M3 远程 ⏳ → M4 发布 ⏳
```

---

## M0 · 骨架 ✅（2026-10-28 完成）

**目标**：单窗口 + 本地 shell + GPU 网格渲染站得住。

| 任务 | 状态 |
|------|------|
| workspace 初始化、仓库结构（app/termview/config） | ✅ 4483f67 |
| termview 内化（自 zortax/gpui-terminal 0.1.0 接管维护） | ✅ |
| 本地 PTY 会话 + 终端工作区 + 滚动交互 | ✅ ea71098 |
| 设置面板与标签生命周期管理 | ✅ c68629b |

**验收**：`cargo run --release` 打开即用；vim / htop / tmux 无残影（AGENTS.md 口径）。

---

## M1 · 交互完善 🚧（本阶段）

**目标**：日常可完全替代现有终端；配置/主题体系成型。

### 已完成

| 任务 | 状态 |
|------|------|
| laterterm-config：TOML 设置持久化 | ✅ bffa421 |
| 主题系统（ColorPalette builder + Tokyo Night）与 notify 热加载 | ✅ 6e2e085 |
| 设置面板 UI（接 config 落盘） | ✅ |
| 回滚搜索（terminal.rs `search_grid` + view.rs SearchState/搜索 UI，含 CJK 宽字符与回滚排序测试——前版路线图误标为「从零做」，勘误） | ✅ 存量已完成 |
| TerminalSession trait 抽出：`laterterm-core`（纯 std 契约：backend/title/take_streams/resize/kill）+ `laterterm-local`（PTY 开启/spawn/回收线程内聚；termview 与 app 摆脱 portable-pty 直依赖，resize 走视图默认回调转发给会话） | ✅ 本次提交 |
| **中文 IME 预编辑**：`preedit_layout` 布局（宽字符 2 列/行尾截断）+ `paint_preedit` 下划线浮层（不写网格）+ `ime_anchor` 视口锚点（滚动钳制）+ `bounds_for_range` 改锚光标（原为视图左上角，候选框不跟随） | ✅ 5cee2aa |
| **分屏树**：Tab/Pane 双层模型（每个标签持有自己的分屏树与焦点）、H/V 嵌套、焦点循环、标题栏按钮 + Ctrl+Shift+E/O/[/]；标签栏 × / pane 角标 × 双级关闭，最后窗格自动补位 | ✅ c27422e |
| **字体配置面**：字体族预设芯片、连字开关（FontFeatures calt）、备选字体链多选（FontFallbacks），即时落盘 + 热加载 | ✅ bb11f57 |

### 剩余（按优先级）

> M1 功能项已清空；进入 M1 验收期（拔掉原终端用一天）后启动 M2。
> 已知体验债：终端锁覆盖 paint（可做可见网格 snapshot）；渲染器克隆/测量可再省。

**验收**：拔掉原终端用一整天无不适；IME/搜索/分屏在 4K 缩放下不糊不跳。

**风险**：GPUI 的 Linux IME/Wayland 支持仍在上游演进——若 Wayland 原生受阻，XWayland 兜底并记录决策（X11 + Wayland 双栈见 AGENTS.md）。

---

## M2 · 快捷命令面板 🚧

> 命令模板系统已完成：持久化、变量解析、安全 shell 转义、真实 IME 参数输入、实时预览/确认、focused pane 目标锁定。当前仍待：模板 CRUD、独立快捷键、TOTP/keyring。

**目标**：Deepin 终端灵魂功能落地——运维的跳板机操作台。

| # | 任务 | 说明 |
|---|------|------|
| 1 | 命令模板系统 | 参数化模板（`ssh {{user}}@{{host}} -p {{port}}`），安全展开 + 预览确认后下发 focused pane | ✅ 当前提交
| 2 | 内建 TOTP | RFC 6238，密钥入系统密钥环；跳板机二次验证一键完成 |
| 3 | 每命令独立快捷键 | 模板绑定全局/应用内快捷键 |
| 4 | 侧面板 UI | laterterm-ui crate 启动：标签/分屏树 + 侧面板（命令/服务器/SFTP/设置）骨架 |

**验收**：真实跳板机登录流程（密码 + 动态码）全程不碰键盘手输验证码。

---

## M3 · 远程与传输 ⏳

**目标**：远程不是"另一个功能"——同一个 termview，换 backend。

| # | 任务 | 说明 |
|---|------|------|
| 1 | laterterm-ssh | russh + russh-sftp；tokio 仅限网络层，PTY 走线程泵（架构既定） |
| 2 | 服务器管理 | 主机分组、连接配置、凭据入 keyring（laterterm-secrets） |
| 3 | 跳板机链 | ProxyJump / keyboard-interrupt 认证链式穿透 |
| 4 | SFTP 面板 | 双栏文件传输，进度与断点续传 |
| 5 | 编码 | GBK 远程会话转码（中文老服务器刚需） |
| 6 | trzsz | rz/sz 兼容协议，ZMODEM 类传输 |
| 7 | Deepin 配置一键导入 | `command-config.conf` / `server-config.conf` 已逆向（见 architecture.md），M3 收官时做迁移工具 |

**验收**：从 LaterTerm 完成一次「跳板机 → 内网机 → GBK 环境 → rz 拉文件」全链路操作。

---

## M4 · 发布打磨 ⏳

| # | 任务 |
|---|------|
| 1 | 三平台打包矩阵：Linux（AppImage + deb）、Windows（msi）、macOS（dmg），CI 发版链照抄 LaterMD 经验（auto-tag + 版本注入 Release 正文） |
| 2 | Quake 下拉模式（全局快捷键顶部滑出） |
| 3 | 广播输入（多会话同步键入） |
| 4 | 动效与无障碍打磨；首启动引导（默认 shell/字体探测） |

**验收**：v0.1.0 发布，README 附真机截图（复用 LaterMD 的无头截图生成思路可选项）。

---

## 依赖与风险台账

| 风险 | 等级 | 缓解 |
|------|------|------|
| GPUI 上游 API 漂移（0.2.x 迭代快） | 中 | 锁版本 + termview 已内化，升级自主可控 |
| GPUI Wayland/IME 成熟度 | 中 | XWayland 兜底；IME 是 M1 硬验收，早暴露 |
| termview 内化后维护负担 | 低 | 8k 行规模可控；改动守 MIT OR Apache-2.0 许可头 |
| alacritty_terminal 协议长尾 | 低 | 行业标准答案，跟版本即可 |
