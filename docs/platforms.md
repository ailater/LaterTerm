# 品牌资源与跨平台验收

> 2026-11-02。资源接入、原生构建和桌面视觉是不同的验收项；本机为 Linux，未据此宣称 Windows/macOS 已可发布。

## 资源与身份

- 原图：`assets/logo.png`，用户提供的 500×500 RGBA PNG（透明背景版）。保留全部颜色与透明轮廓，不自动抠图或裁剪；图标在小尺寸下按 alpha 边缘缩放。
- Linux：`assets/icons/png/laterterm-<尺寸>.png`，16/24/32/48/64/128/256/512/1024 像素。
- Windows：`assets/icons/laterterm.ico`，16 至 256 像素的多尺寸容器；应用 `build.rs` 为目标 EXE 嵌入 ID 1，与 GPUI 的加载约定一致。
- macOS：`assets/icons/laterterm.icns`，标准尺寸及 Retina 槽位。
- 应用内：标题栏用 20 逻辑像素图片，嵌入 128 像素 PNG，经 `OnceLock<Arc<Image>>` 缓存，不依赖当前工作目录或网络。GPUI 负责系统缩放，不手动重复乘 DPI。
- Linux 窗口身份、desktop 名、图标名固定为 `laterterm`；macOS bundle identifier 为 `io.github.crazykun.LaterTerm`。动态终端标题不改变应用身份。
- Logo 和图标只是品牌资源，不能替代系统任务栏/桌面关联；不使用 emoji 或字体字符冒充 Logo。

## 再生成与校验

开发工具要求 Python 3.11+、Pillow **12.3.0**；应用运行不依赖 Python/Pillow。

```bash
python3 -I script/generate-icons.py
python3 -I script/generate-icons.py --check
python3 -I -m unittest discover -s script/tests -v
desktop-file-validate packaging/linux/laterterm.desktop
```

图标测试使用临时目录，不改仓库输出；`--check` 检测源图、尺寸和输出漂移但不写文件。更新 Logo 时先替换唯一原图，再统一再生成并检查各容器，不手工维护多份 ICO。

## Linux：用户级桌面资源

先构建：

```bash
PATH="$HOME/.cargo/bin:$PATH" cargo build -p laterterm-app --bin laterterm
```

显式安装入口（本轮验收仅在临时目录运行，未执行以下用户级安装）：

```bash
bash script/install-linux-desktop.sh --binary "$PWD/target/debug/laterterm"
```

- 默认复制二进制到 `~/.local/bin/laterterm`，desktop 和 hicolor 图标到 `${XDG_DATA_HOME:-~/.local/share}`；不写 `/usr`，不要求 root。
- `--destdir /绝对/临时目录` 生成标准 `usr/bin` 与 `usr/share` 暂存树；desktop 的 Exec 指向安装后的 `/usr/bin/laterterm`，不包含临时打包根目录。
- 支持路径含空格、引号、美元符号及反斜线，已用私有 DBus 下的 GIO 启动无副作用 fixture 验证；含 `%` 的实际可执行安装路径因 GIO 解析限制明确拒绝。拒绝覆盖已有目标、符号链接目标和父目录。更新已有安装前需明确处理已有文件，脚本不悄悄覆盖。
- 应用启动不自动安装 desktop、不刷新或改写用户启动器；桌面缓存的刷新由实际安装/包管理流程负责。
- Wayland 通过 `app_id`，X11 通过 `WM_CLASS` 匹配 desktop。未安装裸二进制在某些 X11 窗口管理器中仍可能没有任务栏图标，本轮没有向 GPUI 添加 `_NET_WM_ICON` 适配。

### deb 与 AppImage（2026-11-07 落地，CI 侧未实际触发）

二者复用同一份 desktop 与 hicolor 图标目录，不另复制源图。deb 走 `script/package-assets.py deb`：staging `DEBIAN/control` + usr 树（复用 `write_new_files` 的拒绝覆盖/符号链接安全语义）→ `dpkg-deb --build --root-owner-group` 组包 → `dpkg-deb --info` 自校验；AppImage 走 `script/package-appimage.sh`：AppDir 暂存 + `desktop-file-validate`，CI 下载钉版 linuxdeploy（`1-alpha-20251107-1`，追 latest 会在上游换行为时静默翻车）并以 `--appimage-extract-and-run` 运行（runner 无 FUSE）。

本地 dry-run（无 linuxdeploy 环境只验 AppDir 结构与 desktop 契约，不产 `.AppImage`）：

```bash
bash script/package-deb.sh --binary "$PWD/target/release/laterterm" --output /绝对/临时目录/deb
bash script/package-appimage.sh --binary "$PWD/target/release/laterterm" --output /绝对/临时目录/appimg --dry-run
```

边界：deb 的 control 字段与目录树经 `dpkg-deb --info` 本地验证，`apt install ./` 实机安装未验（Depends 首版只写宽松下限，实机验收后再收紧；libasound2 在 Ubuntu 24.04 因 time64 改名，用 `libasound2 | libasound2t64` alternatives 兼容）；AppImage 仅 dry-run 验过 AppDir，linuxdeploy 真打包与产物启动待 CI 实跑。release 构建从 ubuntu-latest 钉到 ubuntu-22.04 压 glibc 下限（2.35，LaterMD 发版链同口径），但 GPUI vendor 依赖在 22.04 的编译未经 CI 裁决——若首跑编不过，回退 24.04 并在 README/Release 正文声明 glibc 下限。

## Windows

- `build.rs` 及资源编译依赖用 `cfg(windows)` 隔离。Linux/macOS 构建不会调用 Windows RC；ICO 来自仓库统一资源，RC 在 OUT_DIR 临时生成。
- 原生 release 构建需要 MSVC、Windows SDK 的 `rc.exe` 和 `fxc.exe`；GPUI 使用 `GPUI_FXC_PATH`。不能把 Linux 上 `cargo clippy --all-targets` 称作 Windows 构建通过。
- 默认 shell 集中在 `laterterm-local::default_shell()`：优先绝对 `COMSPEC`，再 `SystemRoot\\System32\\cmd.exe`，最终 `cmd.exe`；不再无条件寻找 `/bin/bash`。路径允许合法空格，不把可执行文件和参数拼成 shell 命令。
- 本地 PTY 测试使用平台对应的命令，Windows 不运行 Unix 的 `printf`/`sleep` 参数。
- 主线整合的自动 ZMODEM/trzsz 实现依赖 Unix；Windows 下传输包装透明返回原会话流，不拦截或吞掉协议字节，暂不提供自动文件传输。单独 transfer crate 的 Windows GNU 离线编译已在额外 stable 工具链检查过，不代表 MSVC 整个应用或 Windows 原生运行通过。
- 原生构建工作流手动触发，包含 release 构建、EXE 中 `RT_GROUP_ICON` ID 1 校验及本地 PTY 测试；仅添加配置，未在本轮触发。
- Explorer、Alt-Tab、任务栏、100/125/150/200% 缩放及跨屏均待 Windows 原生验收。不把资源嵌入等同于固定 AppUserModelID、MSI 或发布签名。

### msi（2026-11-07：清单已备、未接线，拆后续任务）

`packaging/windows/laterterm.wxs` 为 WiX v4+ 清单（`wix build` 单命令语法；固定 UpgradeCode，测试断言其精确值防误改；注释含完整接线步骤与 signtool 签名占位）。**未接入 release.yml**，硬阻塞在源码层：laterterm-ssh 在 Windows 下不可编译——`crates/laterterm-ssh/src/lib.rs:841` 的 russh `AgentClient::connect_env()` 仅 `#[cfg(unix)]`，文件内无平台守卫，MSVC 下 E0599（v0.2.0 release 的 Windows 构建步实跑失败，被 continue-on-error 吞掉），且 laterterm-app 无条件依赖 laterterm-ssh（`crates/laterterm-app/Cargo.toml:24`）。前置条件 = laterterm-ssh 在 Windows 下编过（cfg 门控 agent 分支），届时再接线 wix（dotnet tool 钉版，不依赖镜像预装的 WiX 3.x——虽然其仍在 runner 镜像内，但按全新未验链处理）并去掉 continue-on-error。Windows 产物（含 zip）未实际构建验证。

## macOS

- 使用原生交通灯，预留左侧安全区域，不再重复绘制 Windows 风格的关闭/最小化/缩放按钮；原生关闭与自绘关闭共用会话清理。
- 原生构建要求 Xcode/Apple SDK 与 Metal 编译工具；使用原生 runner，不能仅靠 Linux 增加 Rust target 证明构建兼容。
- 从已构建二进制组装 `.app`：

```bash
bash script/package-macos.sh \
  --binary "$PWD/target/release/laterterm" \
  --output /绝对/临时输出目录
```

版本从 workspace Cargo.toml 读取，生成 Info.plist，包含 executable、bundle ID、ICNS 和高 DPI 元数据。不写 `/Applications`，拒绝覆盖已有 `.app`。脚本只组装，不证明输入二进制为 Mach-O、不收集第三方动态库，也不签名、公证或生成 DMG；这些属于后续发布门禁。

原生手动工作流包含 release 构建、组装和 `plutil -lint`。本机仅验证目录结构与 plist/图标内容，Finder/Dock 图标、Retina、全屏交通灯及 Cmd/Option 行为待原生验收。

### dmg（2026-11-07：分步 hdiutil 落地，产物未实际构建验证）

release.yml 组 `.app` 改走 `script/package-macos.sh`（Info.plist @VERSION@ 注入 + icns，`plutil -lint` 同 packaging.yml 配方），dmg 改分步 hdiutil：固定大小空镜像 → 挂载 → ditto 拷入 → 卸载 → convert 压缩。这是**防御性对齐** LaterMD v0.0.11 的容量踩坑（一步式估算不含 xattr/资源叉），LaterTerm v0.1.0/v0.2.0 一步式实跑成功过——不是修已观察故障，是消除隐患并让每步可独立归因。签名/公证仅注释占位（Developer ID / notarytool 待定）。

## 隔离 GUI 验收与结果边界

本轮先以临时 HOME/XDG、私有 DBus 复测上一轮交互。不要使用真实服务器、默认私钥、Secret Service 或全局桌面注入；不终止用户现有 LaterTerm。

真实像素验收需私有 X11 显示或私有 Wayland compositor。本机未发现 Xvfb、Xephyr、Weston、Cage、Labwc、Sway 或 KWin Wayland，因此未启动当前桌面窗口充作隔离测试。GPUI 合成窗口能检查事件与布局，不能证明 GPU 像素清晰、系统保留按键可达或桌面图标分组正确。

| 项目 | 本轮验收口径 |
|---|---|
| 交互/布局、Logo 固定方形、关闭清理 | GPUI 合成测试 |
| 原图、PNG/ICO/ICNS、来源一致性 | 本地资源校验 |
| Linux desktop/安装布局 | 临时目录测试与 desktop 校验 |
| Linux deb 结构与 control | `dpkg-deb --info` 本地验证；`apt install ./` 实机待验 |
| Linux AppImage | AppDir dry-run + desktop 校验本地验证；linuxdeploy 真打包待 CI |
| macOS bundle 结构 | Linux 临时目录组装；非原生运行 |
| macOS dmg | 分步 hdiutil 步骤落地（yml 契约测试兜底）；产物未实际构建 |
| Windows EXE 资源 | 代码/工作流已接入；原生构建待运行 |
| Windows msi | 清单已备未接线（laterterm-ssh Windows 编译阻塞）；产物未构建 |
| workflow yml/wxs 契约 | pyyaml/XML 解析 + 结构断言进本地门禁 |
| Linux X11/原生 Wayland 视觉 | 待独立显示环境验收 |
| Windows/macOS 视觉与可分发性 | 待原生环境验收 |

完整门禁沿用：`cargo test --workspace --lib --bins`、SSH `loopback`、严格 clippy、格式和构建检查。真实密钥环的 ignored 测试不在自动门禁中；保留 termview 既有 doctest 的运行时限制。

本轮已执行（2026-11-02）：workspace **216 通过、1 ignored**；SSH 回环 **5 通过**；图标测试 **6 通过**、打包测试 **8 通过**；Windows RC 盘符/UNC 路径纯逻辑 **1 通过**；全工作区格式、严格 clippy、构建、desktop/plist、diff 检查均通过。GIO 实际启动仅使用私有 DBus 和临时无副作用 fixture，不是 LaterTerm 桌面视觉验收。

打包矩阵与发版链收尾轮已执行（2026-11-07）：脚本测试 **26 通过**（图标 6、打包 11（含 deb 2 + AppImage 1）、workflow 契约 9）；`cargo test --workspace --lib --bins` **407 通过 / 4 ignored（exit 0）**；全部 workflow yml 经 pyyaml 解析；wxs 经 stdlib XML 解析 well-formed；deb 产物经 `dpkg-deb --info`/`--contents` 独立复核（属主 root/root 证明 `--root-owner-group` 生效）。CI 未实际触发：deb/AppImage 真打包、msi、分步 dmg、CHANGELOG→Release 正文注入的端到端均待真 tag 后实跑；**Windows/macOS 产物未实际构建验证**。
