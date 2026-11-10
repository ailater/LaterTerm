# Changelog

本文件维护各版本变更。发版链路会把对应版本的小节自动注入 GitHub Release 正文；
发版时把 `Cargo.toml` 的 `workspace.package.version` 提到新版本号，并在顶部追加
`## v<版本> - <日期>` 小节（两者同 PR 提交，合入后 auto-tag 自动打 tag 发布）。

## Unreleased

### 工具面板

- **右侧面板图钉停靠**：面板头部图钉切换「固定 / 自动隐藏」两种模式——固定 = 常驻
  占布局宽度（原行为）；自动隐藏 = 悬浮覆盖终端右侧不挤压布局，点击终端即收起，
  快捷键/标题栏按钮唤出；模式持久化（settings.toml `right_panel_pinned`，默认固定）

## v0.3.0 - 2026-11-09

终端体验大升级：鼠标选区/右键菜单/SFTP 面板/Quake 模式/性能优化/六项基础功能补齐。

### 终端鼠标与选区

- **右键菜单**：复制/粘贴/全选/搜索/清屏/重置/打开 URL/分屏标签项
- **文本选区**：拖拽选择、双击选词、三击选行；选中即复制（松开自动写剪贴板，可关）
- **鼠标协议上报**：vim/tmux 鼠标模式支持（SGR 1006 格点上报）

### 终端交互

- **平滑滚动**：亚行渲染 + 滚动残差累积（触控板/高分辨率滚轮不丢行不跳行）
- **光标样式可配置**：方块/竖线/下划线 + 闪烁门控
- **URL 点击打开**：终端输出中的 http/https/ftp/www 自动检测，点击跳转系统浏览器
- **选中即复制**：拖拽选区松开自动写剪贴板（设置可关）

### 基础功能补齐

- **OSC 52 剪贴板**：远端 SSH 的 tmux set-clipboard / xsel 全线可用
- **Ctrl+滚轮字号缩放**：8-40px 步进 ±1
- **标签重命名**：双击侧栏标签弹输入框，空 = 恢复自动标题
- **搜索增强**：Alt+C 大小写敏感切换，搜索栏 Aa 指示
- **会话恢复**：退出保存标签列表，重启还原（settings.toml `restore_session = true` 开启）
- **Bell → 系统通知**：终端响铃触发桌面通知（notify-send）

### SFTP 双栏文件面板

- 远程目录浏览、上传下载、进度条展示
- 窗口级大覆盖层（不挤右面板）

### Quake 下拉模式

- xdg-desktop-portal GlobalShortcuts 全局热键
- 主窗口顶部滑出（顶部 40% 高度，失焦自动隐藏可选）

### 服务器管理

- **主机分组**：生产/入口层式斜杠分层，组头折叠/重命名/解散
- **Deepin 命令模板一键导入**：冲突快捷键剥绑定保命令
- **设置面板**：主题预览（色板卡片）、关于卡片（GitHub Releases 版本比对）
- 设置单页滚动 + scroll-spy 导航联动

### UI 精修

- 按钮风格统一（ui_button 三语义工厂：Primary/Secondary/Ghost）
- 图标按钮 SVG 化（统一线性图标）
- 标题栏品牌精简 + Ctrl+B 切换左栏
- 首启动引导（默认 shell 与字体探测向导）

### 性能优化

- **绘制期网格快照**：终端锁持有窗口最小化（render.rs 重构）
- **全链路热点优化**：workspace 每帧渲染路径缓存、SFTP 节流、设置 UI 懒构建

### 内部

- laterterm-config 新增 session / servers 模块拆分
- GPUI 本地化（vendor/gpui）携带 Linux 后台任务/帧调度修复
- 三平台打包矩阵（AppImage/deb/msi/dmg）
- Windows CI 编译检查（核心 crate）

### 服务器管理

- **主机分组**：服务器条目支持分组（TOML `group` 字段，允许「生产/入口层」式斜杠分层）
- 右面板服务器页签按分组折叠展示：组头显示计数、点击折叠/展开，组名升序、未分组殿后
- 编辑弹窗「分组」字段进组/改组；组头支持批量重命名（同名并入既有组、清空即解散）
- Deepin 导入的 GroupName 进入分组字段（不再拼进备注），Xshell 目录树分组整串保留
- 手改坏的分组（控制字符/超长）载入时降级为未分组，绝不丢服务器条目

### 快捷命令

- **Deepin 命令模板一键导入**：命令页签新增「导入」按钮，读取本机
  `~/.config/deepin/deepin-terminal/command-config.conf`
- 导入计数全可见：同名跳过 / 无效丢弃 / 解析跳过 / 剥除冲突快捷键
- 导入命令的快捷键与现有绑定冲突时剥绑定保命令，绝不静默改变用户现有按键；
  导入命令与主线一致默认执行前确认
- Deepin 服务器导入顺带迁移命令模板（独立落盘，计数拼进导入提示）

### 内部

- laterterm-config 新增 `normalize_group` / `parse_deepin_commands` / `load_deepin_commands`
  导出；laterterm-app 移除 laterterm-deepin 直接依赖（crate 本体保留为格式真值参照）

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
