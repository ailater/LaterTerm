# Changelog

本文件维护各版本变更。发版链路会把对应版本的小节自动注入 GitHub Release 正文；
发版时把 `Cargo.toml` 的 `workspace.package.version` 提到新版本号，并在顶部追加
`## v<版本> - <日期>` 小节（两者同 PR 提交，合入后 auto-tag 自动打 tag 发布）。

## v0.1.0 - 2026-11-01

LaterTerm 首个可用版本：跨平台 GPU 终端，Rust + GPUI。

### 终端本体

- GPU 网格渲染（egui→GPUI 管线，连字/CJK/emoji/HiDPI 原生支持）
- 多标签 + H/V 嵌套分屏（Tab/Pane 双层模型，焦点循环，Ctrl+Shift+E/O 分屏）
- 中文 IME 预编辑（光标处下划线浮层 + 候选框跟随）
- 回滚搜索（10 万行缓冲 + Ctrl+Shift+F 全文搜索，含 CJK 宽字符）
- 主题热加载（TOML 配置，Tokyo Night 起步，与 Later 家族审美一致）

### 远程会话（M3）

- SSH 连接（密码/私钥文件/ssh-agent 三种认证）
- 跳板机链（ProxyJump 语义，端到端加密）
- 主机密钥 TOFU（OpenSSH known_hosts 兼容，变更硬拒绝）
- GBK/GB18030 编码自动转码（遗留中文服务器直连即读）
- SFTP 文件通道（列目录/上传/下载/进度回调）

### 终端内文件传输

- ZMODEM（rz/sz 兼容，lrzsz 检测串自动触发）
- trzsz 接收（trz/tsz 推文件，MD5 全文校验）
- trzsz 上传（R 触发，本端推文件到远端）

### 快捷命令操作台（M2）

- 参数化命令模板（安全 shell 转义 + 预览确认 + focused pane 下发）
- 每命令独立快捷键（应用内绑定 + 冲突检测）
- 内建 TOTP（RFC 6238，密钥入系统密钥环，跳板机二次验证一键完成）

### 服务器管理

- 服务器列表（添加/编辑/连接/删除，密码存密钥环不落盘）
- XShell 会话导入（.xsh 解析 + Sessions 目录树→分组）
- deepin-terminal 配置导入（command/server 全字段）
