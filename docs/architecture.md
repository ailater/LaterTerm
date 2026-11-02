# LaterTerm 架构

> 定案于 2026-10-28，随里程碑演进。

## 一句话

**egui→GPUI 外壳 + alacritty_terminal 仿真 + 会话抽象层**：远程不是"另一个功能"，只是换了 backend 的同一个终端。

## 分层

```
laterterm-ui        外壳：标签/分屏树、侧面板（快捷命令/服务器/SFTP/设置）      [M1+]
laterterm-termview  GPUI 终端网格（内化自 zortax/gpui-terminal，接管维护）      [M0]
laterterm-core      TerminalSession trait、编码(UTF-8/GBK)、trzsz 协议          [M1+]
laterterm-local     本地会话：portable-pty + 线程泵                             [M1+]
laterterm-ssh       russh + russh-sftp + 跳板机链 + keyring 凭据                [M3]
laterterm-config    TOML 配置/主题热加载（notify）                              [M1]
laterterm-secrets   系统密钥环封装                                              [M3]
laterterm-app       main 二进制                                                 [M0]
```

## 关键决策记录

| 决策 | 选择 | 理由摘要 |
|------|------|----------|
| UI 框架 | GPUI（Zed 同款渲染）| 渲染上限最高；gpui_terminal 0.1.0 恰好基于当前 gpui 0.2.2，胶水现成；用户日常用 Zed 已验证本机环境 |
| termview 获取方式 | 内化接管而非依赖 | 上游停更（10 个月无版本），MIT OR Apache-2.0 允许；后续主题/IME/会话改造都在自己手里 |
| VT 仿真 | alacritty_terminal 0.25 | 协议长尾的行业标准答案，禁自造轮子 |
| SSH | russh + russh-sftp（tokio 仅限网络）| 跳板机 keyboard-interactive + ProxyJump 原生；PTY 走线程泵 |
| 会话抽象 | `TerminalSession` trait：recv/write/resize/backend | M1 从 termview 抽出 PTY 管理时落地 |
| 主题 | TOML + ColorPalette builder，Tokyo Night 起步 | 与用户 Later 家族审美一致 |

## 里程碑

M0 骨架 ✅ → M1 交互完善（本阶段，主题热加载/设置已落地）→ M2 快捷命令+OTP → M3 SSH/跳板机/SFTP/GBK → M4 三平台打包。任务明细见 roadmap.md。

深度终端迁移：`command-config.conf` / `server-config.conf` 格式已逆向（见 ~/.config/deepin/deepin-terminal），M3 提供一键导入。

## 快捷键体系

**应用内（已落地）**：统一注册表 `ShortcutBinding`（内置 + 命令 + 服务器 + 动态码四类），
`normalize_shortcut`（laterterm-config）归一化后查重；保存时 `check_shortcut_conflict`
拦截冲突（编辑自身豁免），`on_key_down` 按 命令→服务器→动态码→内置 顺序分发；
设置→快捷键页为全量总览，冲突标红。组合键格式 `ctrl+alt+k` 一类，匹配大小写/空格不敏感。

**系统级（M4 Quake 前置，方案已定）**：Wayland 唯一正道是 xdg-desktop-portal 的
`org.freedesktop.portal.GlobalShortcuts`（ashpd crate）；X11 `XGrabKey` 在 Wayland
合成器下完全无效，禁止走那条路。LaterScreen（姊妹项目）真机踩坑结论，照抄其解法：

1. **首次 bind 会弹系统确认对话框**（用户在合成器里确认后键位才交付）——期间
   `Activated` 不来属正常，不得当成失败处理；
2. **注册阶段（连接/订阅/建会话/绑定）必须加 deadline（LaterScreen 用 8s）**——
   portal 在登录后偶发卡住会让热键永久静默失效；绑定成功后的 Activated 监听
   不设超时，按设计长活；
3. **注册失败自动重试一次（隔 1s）**，重试耗尽则优雅降级为仅应用内快捷键；
4. **先订阅 Activated 信号再 bind**，避免 bind 到订阅之间的触发丢失。

落地形态（M4）：独立线程 `async_io::block_on` 跑注册+监听，触发经 flume 通道回
主线程；与 LaterScreen 不同点：LaterTerm 的全局键默认只保留一个「呼出/隐藏主窗」
（Quake），命令/服务器/动态码绑定仍为应用内——避免占用户全局键位引发冲突投诉。
