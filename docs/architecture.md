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

M0 骨架（本阶段）→ M1 多标签/IME/主题 → M2 快捷命令+OTP → M3 SSH/跳板机/SFTP/GBK → M4 三平台打包。

深度终端迁移：`command-config.conf` / `server-config.conf` 格式已逆向（见 ~/.config/deepin/deepin-terminal），M3 提供一键导入。
