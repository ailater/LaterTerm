# Changelog

本文件维护各版本变更。发版链路会把对应版本的小节自动注入 GitHub Release 正文；
发版时把 `Cargo.toml` 的 `workspace.package.version` 提到新版本号，并在顶部追加
`## v<版本> - <日期>` 小节（两者同 PR 提交，合入后 auto-tag 自动打 tag 发布）。

## v0.2.0 - 2026-11-03

UI 走查与远程体验大版本：统一快捷键体系、服务器连接弹窗化、终端内文件传输全线上线。

### 终端内文件传输（v0.1.0 的传输层首次接线到终端）

- **ZMODEM（rz/sz）**：远端敲 rz/sz 自动触发，文件落到 `~/Downloads`（可在设置改）
- **trzsz 接收（trz/tsz）**：远端推文件，MD5 全文校验
- **trzsz 上传（R 触发）**：本端推文件到远端
- 所有会话（本地/SSH）统一经过传输装饰器

### 广播输入

- **Ctrl+Shift+B** 多窗格同步键入——分屏后在一个窗格输入的命令同时发到所有窗格
- 标题栏 📢 广播指示

### 终端交互改进

- 分屏可拖拽调比例（悬浮分割条）
- 分屏快捷键改默认 Ctrl+J（左右）/ Ctrl+K（上下），焦点循环 Ctrl+Tab
- 焦点窗格指示（边框高亮）
- 悬浮滚动条
- 标签激活强调线
- 设置页定高滚动（不再溢出）

### 服务器管理

- SSH 连接配置弹窗（窗口级模态，密钥环 5s 超时，凭据掩码，Enter 提交）
- 私钥认证完善（私钥文件路径 + 口令）
- 服务器列表逐字换行修复 + 滚动卡顿修复
- **导入三来源**：XShell Sessions 目录 / deepin-terminal 配置 / LaterTerm TOML 文件
- 导出服务器配置（TOML + 系统文件对话框）

### 快捷键体系

- 统一快捷键绑定（命令模板 / 服务器连接 / 动态码发送共用一套注册系统）
- 快捷键录制器（录入即绑定，冲突检测）
- 应用内快捷键帮助

### 首启引导

- 首次运行显示欢迎覆盖层（快捷键速查 + 开始使用按钮）
- 提示服务器导入入口

### 打包与品牌

- 跨平台图标资源（ICNS / ICO / PNG 全尺寸）
- 打包脚本（Linux / macOS / Windows）
- Windows CI 编译检查（核心 crate）

### 内部

- laterterm-app 模块化启动：server_connection / shortcut_recorder / pane_navigation 拆出
- workspace.rs 瘦身进行中（6448 行，从合并前的 ~2900 行增长，后续继续拆）
- 传输层 TransferWrappedSession 统一注入所有终端

## v0.1.0 - 2026-11-01

LaterTerm 首个可用版本：跨平台 GPU 终端，Rust + GPUI。

### 终端本体

- GPU 网格渲染（egui→GPUI 管线，连字/CJK/emoji/HiDPI 原生支持）
- 多标签 + H/V 嵌套分屏（Tab/Pane 双层模型，焦点循环，Ctrl+Shift+E/O 分屏）
- 中文 IME 预编辑（光标处下划线浮层 + 候选框跟随）
