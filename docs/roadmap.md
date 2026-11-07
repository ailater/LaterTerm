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
M0 骨架 ✅ → M1 交互完善 🚧(验收期) → M2 快捷命令 🚧(待实机走查) → M3 远程 🚧(SSH/服务器已落地) → M4 发布 ⏳
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
> 设置面板重构为单页滚动 + 左导航 scroll-spy 双向联动（2026-11-07）：滚动跟随高亮、
> 点击瞬时跳转、滚到底兜底最矮尾节、内容不超高跳过侦测、重开面板复位滚动与高亮；
> 外观页「终端外观/字体/滚动与主题」三组拆为分区锚点（快捷键节分组降为子标题），
> 控件/卡片/标题层级按 Tokyo Night 令牌收敛（导航选中 accent tint + 指示条，
> 与 LaterScreen 暗色「中性提亮」系有意偏离，留 4K 实机走查）。设置语义与持久化
> 路径不变；迁移函数挂为 workspace 子模块（需访问 WorkspaceView 私有项，顶层
> 平级模块不可见）。门禁：app bins 新增 scroll-spy 纯函数与导航联动 gpui::test。
> 已知体验债「终端锁覆盖 paint；渲染器克隆/测量可再省」处置（2026-11-07）：绘制期
> 改为「可见网格快照短锁」——canvas 每帧唯一一次持锁做 FrameSnapshot::capture
> （可见区逐格复制 + selection.to_range + dims/cursor/display_offset/colors/mode 读取），
> 锁外 paint_frame 与搜索/选区/preedit 覆盖层全部消费同帧快照；快照缓冲每窗格持池
> （帧尾归还、capture_into 复用行容量，稳态帧零行分配）；last_content_origin 的
> entity.update 移出锁窗口；paint 闭包内 renderer.clone() 删除（闭包 move+FnOnce 直接
> 用，省每帧每窗格 font_family String + font_fallbacks Vec + palette 白拷贝）。新增
> LATERTERM_RENDER_TRACE=1 每帧稳态观测（font_trace 只打重测量帧，采不到稳态），
> 字段口径钉死、前后对照只认 paint_lock_hold_ms（锁获取成功→guard 释放：快照前=历史
> 读取+实体写+绘制+三段覆盖层全程，快照后=仅快照复制，另有 snapshot_ms 同值标注）。
> 对照数据（release、隔离测试窗口逐帧 window.draw、228×98=22344 格、交替 4 轮×207
> 稳态帧取中位；**非基准承诺**，非真机 4 窗格协议，debug 构建渲染失真前提见 M3
> 2026-11-04 注记）：锁持有时长 0.531ms→0.114ms（-78.5%）；锁等待双方均 0（主线程
> 串行模型本无竞争）；整帧 paint_closure_ms 0.532→0.597ms（+12.3%，约 +65µs，<1%
> vsync 帧预算）。方案「after≤before×10%」参考线未达：纯 cell 复制本身实测即
> 0.114ms（=21.5%），任何全网格遍历不低于此——该线前提（复制≈50µs）被实测否定，
> 非机制失效。+5% 帧回归线超出：两阶段「先复制后消费」的整网格工作集（≈2×快照
> 体积）相比旧「收集一行→立刻消费一行」的行热模式有固有缓存代价，缓冲池/行解析
> 外提/scratch+swap 三变体实测无改善；方案 risk-1 预案的「锁内布局、锁外整形」形态
> 实测锁窗口 0.29-0.31ms（layout 本身约 0.17ms，比快照差 2.5×）而整帧差异在批次
> 漂移 ±5-6% 内统计不可区分——回退不划算，保留快照形态并把代价如实入账。收益定位
> （诚实口径）：主线程串行与帧调度不变，用户可感知收益为零；真实收益=锁窗口记账
> -78.5% + 为未来第二线程消费者（后台快照/诊断抓帧）留路。API 口径：paint 保持签名
> （薄包装=capture+paint_frame）；layout_row 迭代项有意由 (usize,Cell) 改
> (usize,&Cell)（工作区无其它调用方，本 crate 已内化随本仓库演进）；paint_preedit
> 原签名保留、新增 paint_preedit_frame 快照变体。覆盖层防错行：搜索命中/选区/预编辑
> 坐标换算抽为纯函数 search_match_rect / selection_row_spans / preedit_row 并单测
> （历史行换算/视口外 None/端点裁剪/钳位）；既有 mouse/搜索 gpui::test 只覆盖状态侧
> 不进 paint 路径，paint 侧防护=快照对账单测（OSC 颜色覆盖/SGR/宽字符/滚动/选区/
> 缓冲池复用逐字段与锁内现读对账 + layout 借用版与旧值迭代产出相等）+ 待实机走查
> （滚动中段命中框、拖拽选区逐行高亮、IME 下划线候选框跟随、光标三形状闪烁、
> htop/vim 连笔、4K 缩放）。真机 4 窗格同负载对照（如 seq 负载 20s、≥200 稳态帧）
> 待实机。门禁：workspace lib/bins 407 通过 4 ignored（termview +5 测试：快照对账/
> 池复用/layout 等价/三覆盖层纯函数，+1 bench ignored；clippy 零告警）。
> 主题预览（2026-11-07）：设置→滚动与主题新增色板预览卡（背景/前景/光标三色块 +
> ANSI 8×2 中文名小字标签 + 示例终端行，数据经 palette_preview 纯函数与真实窗格色板
> 同源）；点主题芯片改为「预览未落盘」语义——选中即全部窗格真实换色
> （effective_theme_name 唯一读口，芯片/色板/窗格三处同源防漂移），预览激活时出现
> 「保存主题 / 还原」按钮：保存走 persist_settings 注入路径，且经 themes.get 严格
> 校验（热加载后失效的主题名不落盘，缺失即还原）；关闭面板（scrim/× 与 ⚙/右键菜单
> 两条关闭路径均挂）、恢复默认（含 change_setting 等值早退分支）、外部热加载三处都会
> 收掉预览还原已保存主题——LaterScreen discard 语义移植，其余设置仍即时保存。门禁：
> app bins 137 通过 1 ignored（+5：色板纯函数 1 测 + 应用-恢复/提交落盘/缺失回退/
> 恢复默认早退 4 测）。

**验收**：拔掉原终端用一整天无不适；IME/搜索/分屏在 4K 缩放下不糊不跳。

**风险**：GPUI 的 Linux IME/Wayland 支持仍在上游演进——若 Wayland 原生受阻，XWayland 兜底并记录决策（X11 + Wayland 双栈见 AGENTS.md）。

---

## M2 · 快捷命令面板 🚧

> 命令模板系统已完成：持久化、变量解析、安全 shell 转义、真实 IME 参数输入、实时预览/确认、focused pane 目标锁定；右侧面板已接入模板 CRUD、删除确认、错误反馈和应用内快捷键。
> TOTP 已落地：RFC 6238（SHA1/256/512，RFC 附录 B 全向量测试）+ base32/otpauth 链接解析（laterterm-config）；密钥环封装 laterterm-secrets（keyring v4 → Linux Secret Service，本机 gnome-keyring 冒烟通过），密钥绝不落 TOML；右面板动态码 CRUD + 倒计时 + 确认发送到焦点窗格，DBus 存取全部走后台线程。
> 剩余：侧面板服务器/SFTP/设置骨架按 M3 节奏演进；本阶段尚未完成 GUI 实机验收。
> 验收期已修复：隐藏窗格/标签的终端退出泄漏（TerminalExited 事件化回收，2538ae5）；本机门禁需 `PATH="$HOME/.cargo/bin:$PATH"` 统一 rustup 1.94 工具链，否则 rustc/clippy 版本错位必报 E0514。
> 字体切换卡顿修复：配置监听过滤读取/打开事件，避免重读 settings.toml 再触发监听的循环；同值外观设置不再重复保存/广播。隔离 XWayland debug 单次配置写入对照：持续 CPU 约 106% 降至约 1%，配置相关 openat 从 12 秒 84,220 次降至 5 次；该数据不是 release 帧率基准。`LATERTERM_FONT_TRACE=1` 可选输出字体首次测量/网格调整/绘制阶段耗时，不记录终端内容。
> 分屏导航：Ctrl+上/下/左/右按当前标签可见窗格实际几何选择邻居，无邻居不绕回；设置/输入弹窗中不触发。组合键会被终端工作区消费，不透传 shell。快捷键修饰键别名/顺序统一归一化，旧配置的无效绑定保留并在设置页提示，方向导航优先于历史同键绑定。
> 体验打磨：命令/动态码/服务器的新建/编辑/补输对话框统一升级为窗口级模态弹窗（遮罩点击/Esc/×均可关闭，错误提示移入弹窗内），输入框获得焦点时显示闪烁光标与高亮边框，弹窗关闭后焦点自动归还终端窗格。
> 快捷键体系统一：内置 + 命令 + 服务器 + 动态码四类绑定进统一注册表，保存时跨类别
> 查重（编辑自身豁免），设置→快捷键页全量总览、冲突标红；Esc 关弹窗、动态码/服务器
> 也可绑键一键直达。
> 快捷键录制：命令/服务器/动态码编辑弹窗共用按键录制控件（清空、Esc 取消、失焦/失活取消、长按隔离），根捕获优先交给录制控件；统一精确匹配修饰键，避免 Ctrl+Alt+T 被 Ctrl+T 误抢。命令/服务器列表独立显示可点击键帽；设置→快捷键可编辑条目绑定和「命令/服务器面板开关」，面板开关默认未绑定、不抢占既有按键。仍是应用内快捷键，不注册 Wayland 全局热键。
> SSH 免登录密码：连接弹窗密码留空显式使用本机私钥（不发送空密码）；私钥路径可空，固定查找 id_ed25519 / id_rsa，支持 `~/`。无口令私钥不访问密钥环；加密私钥补输解锁口令，支持仅本次/连接成功后记住，口令保留首尾空白，独立存 ssh-passphrase 命名空间并兼容旧私钥条目。连接请求可取消、迟到成功会话关闭，TOFU 换键拒绝不变；Agent 认证随主线整合保留；尚未解析 OpenSSH IdentityFile 配置，文件认证仍需匹配的本机私钥。
> 本轮验证：工作区 lib/bins 211 通过、1 个既有真实密钥环测试 ignored；SSH 回环 5 通过，严格 clippy、构建与 diff 检查通过。合成窗口覆盖录制事件隔离、取消迟到结果和 800×460 短窗口表单滚动；尚未完成真实 X11/Wayland 视觉与系统保留组合键验收。
> UI 走查落地（外部评审建议筛选取舍）：分屏线 3px 可拖拽调比例（LayoutNode::Split 带 id+ratio，paint 期缓存容器几何 + 窗口级鼠标监听逐帧重挂）、焦点窗格描边 + 非活动窗格压暗（opacity 0.85）、面板右侧悬浮滚动条（termview 新增 ScrollSummary/scroll 回调，输出变化也会刷新）、标签激活态底部强调线 + 关闭按钮 hover 显形、默认行高 1.2。评审中已有/不做项：内边距与行高调节（设置面板已有）、连字/编程字体（默认 JetBrains Mono + calt）、ANSI 彩色（alacritty + TERM，单色是 shell 别名问题）、Ctrl+Shift+F 搜索（已有）、无边框标题栏（自绘）、提示符美化（属 shell 层 starship）。

**目标**：Deepin 终端灵魂功能落地——运维的跳板机操作台。

| # | 任务 | 说明 |
|---|------|------|
| 1 | 命令模板系统 | 参数化模板、安全展开、预览确认、focused pane 目标锁定、右侧面板 CRUD | ✅
| 2 | 内建 TOTP | RFC 6238，密钥入系统密钥环；跳板机二次验证一键完成 | ✅
| 3 | 每命令独立快捷键 | 应用内快捷键、capture 阶段拦截、冲突检测 | ✅
| 4 | 侧面板 UI | 右侧面板页签化（命令/动态码/服务器），三区均已接入；SFTP/设置骨架随 M3 演进 | 🚧

**验收**：真实跳板机登录流程（密码 + 动态码）全程不碰键盘手输验证码（待实机走查）。

---

## M3 · 远程与传输 🚧

> 进度（2026-11-01）：laterterm-ssh 已落地——russh 0.65（密码 / keyboard-interactive /
> 私钥三种认证，PTY+shell want_reply 确认，tokio 仅限网络层，数据/控制走异步泵 +
> flume 桥接同步流），主机键 TOFU 校验（首连记入 `~/.config/laterterm/known_hosts`，
> 换键拒绝并提示行号）；进程内 russh mock 服务器回环测试覆盖认证/回显/resize/kill EOF/
> 换键拒绝。已知 russh 0.65 行为缺口：主动发起 CHANNEL_CLOSE 后会话表即时移除通道，
> 远端回包被丢弃——输出泵以 select 监听本地 shutdown 信号终结（不依赖远端回包）。
> 服务器管理已接入右面板「服务器」页签：TOML 元数据 + 密钥环凭据（ssh/<名称>）+ CRUD
> + 缺凭据补输（可选记住）+ 连接在新标签打开（后台线程连接，结果 render 期消费）。
> 导入/导出已就绪：ServersFile TOML 格式（带版本号，未来版本拒载），系统文件对话框
> （xdg-desktop-portal）选择目标；凭据永不落盘，导入条目首次连接走既有补输流程，
> 同名跳过、无效丢弃并回报计数。导入来源弹窗三选一：LaterTerm TOML / Deepin 终端
> （一键读 ~/.config/deepin/deepin-terminal/server-config.conf，任务 7 服务器部分提前
> 落地，本机 3 台真机验证）/ Xshell 会话（.xsh 多选，UTF-16 BOM 解码，密码绑定原机器
> 加密不迁移）；三类解析器在 laterterm-config::importers，附真机自跳过冒烟测试。

> 右键菜单与文本选区（2026-11-05）：终端左键支持拖拽选区（单击字符/双击词/三击行，alacritty
> Selection 存于 Term.selection，渲染侧逐行画高亮段），右键经 TerminalMenuRequested 事件交工作区
> 渲染 anchored 浮层菜单（复制/粘贴/全选/URL 打开/搜索/清屏/重置/分屏/标签/广播/设置，Esc 与
> 点击外部关闭）。注：应用开启鼠标协议上报（vim/tmux mouse mode）时右键仍弹菜单不上报给应用——
> 该决策随鼠标协议上报落地保持不变，见下一段（2026-11-06）。

> 鼠标协议上报（2026-11-06）：应用开启 TermMode 鼠标位（1000/1002/1003 + 1006）时，左键
> down/up/move 与滚轮按 SGR 1006 编码 write_input 上报（复用 mouse_button_report/scroll_report，
> 新增 mouse_reporting_active 统一门控、mouse_motion_report 拖拽(32)/中(33)/右(34)/悬停(35)编码、
> should_report_motion 的 1002/1003 语义判定；上报坐标一律过 alacritty point_to_viewport 换视口
> 行，回滚负行不会产出 `;-1;` 非法序列）。上报接管时本地选区/URL 打开/平滑滚动残差让位，Shift
> 按终端惯例绕过走本地选择与回滚；右键保持本地菜单不上报（既定决策）；1000-only 旧编码（X10/
> UTF-8）应用一律不上报回退本地（顺带修复存量滚轮对其误发 SGR 字节，回落 ALT 平移/本地回滚）；
> 右/中键按住时移动不上报（应用未收到对应 press 不编造事件），1003 移动按（格点+修饰位）去重
> 防逐像素刷 PTY。测试口径：mouse.rs 纯逻辑 + view.rs gpui::test（真实 process_bytes 置位后
> 驱动事件断言写入字节与本地让位）。

> 低危遗留清理（2026-11-06，评审四处逐项修复完毕）：① 跳板链定点清理——JumpDefect 的
> Missing/TooDeep 改携带 referrer（实际断链那一跳的引用方），clear_broken_jump_refs 只断
> 断链跳，链头合法上游不再被级联误清（旧实现 A→B、B→幽灵 会把 A 一并清掉静默降级直连；
> 环臂语义不动，路过环的旁观条目仍保留）；载入期可见回报：sanitize_servers 返回被清条目
> 清单，Settings::load 非空时 eprintln「无效跳板引用已清除 N（…）」与导入侧同口径。②
> 鼠标上报 release 门控补齐——on_mouse_up 的 mouse_down_reported 分支补 mouse_reporting_active
> （与 down/move/scroll 三入口同口径）：应用按住期间单独发 ?1006l 退到 1000-only 时 release
> 绝不再补发 SGR 字节。③ Quake portal 断连分流——事件循环流结束按停止令牌分类
> （WorkerExit::NormalStop/PortalDisconnected），断连发 Failed 让状态行红显「会话已断开…关闭
> 后重新开启恢复」，不再静默归 Stop；不自动重试，恢复=关/开 Quake 开关重新拉起 worker；竞态
> （禁用后快速重开时旧 worker 迟到 Failed 短暂覆盖 AwaitingConsent，新 worker Ready 自愈）
> 为纯观感瞬态，由 enabled 门兜底。真机验收（启用 Quake 后重启 xdg-desktop-portal 应见红色
> 断连提示、关开开关恢复热键）待实机。④ 首启引导 shell 坏值测试恒真修正——坏值从 [appearance]
> 表内移到顶层键：from_str 钉解析落位（shell==Some(坏值)）+ Settings::load 钉 sanitize 生效
> （shell==None 且 font_size 不受影响），测试与 sanitize 路径重新挂钩。门禁：config lib 82、
> termview lib 108、app bins 125 通过 1 ignored（各含 3/1/2 个新增回归测试）。

> 选中即复制（2026-11-06）：左键拖拽/双击选词/三击选行出有效选区后松开自动写入剪贴板
> （Deepin 同款，不只限拖拽——习惯性双击也会覆盖剪贴板）；Appearance/TerminalConfig 增
> copy_on_select 默认开，旧 settings.toml 缺字段 serde 补 true（default_true，sanitize 不钳 bool）；
> 设置→外观「选中即复制」开关经 change_setting 落盘 + apply_appearance（既有窗格热更新）/
> terminal_config_for（新建窗格，本地与 SSH 共用）双链路生效；触发点挂在 on_mouse_up 有效选区
> 支路 finalize 之后（上报接管/无选区/单击清选区三条提前 return 支路天然不触发）——鼠标协议
> 上报接管（vim/tmux）期间本地选区让位不自动复制，Shift 绕过走本地选区照常自动复制；右键
> 菜单「复制」行为不变（开关关闭后仍是唯一复制入口）；PRIMARY 选择区不在本次范围（gpui 写
> 的是 CLIPBOARD）。门禁：config lib 83、termview lib 109、app bins 126 通过 1 ignored（各 +1
> 新增测试：false roundtrip / 拖拽复制+单击不覆盖+开关关闭 / 传播链含新窗格）。

> 主机分组与命令迁移收尾（2026-11-05）：ServerEntry 新增扁平 `group` 分组字段（TOML 可选、
> 允许 `/`、无父子指针类型层面不成环；validate 严格报错 / normalize_group 宽容降级 None 成对，
> 坏分组绝不丢条目，不 bump CURRENT_VERSION 双向兼容）——服务器页签按组折叠展示（组头 ▸/▾+计数，
> 组名升序、未分组殿后，纯未分组保持旧版平铺外观），编辑弹窗「分组」字段进组/改组，组头支持
> 批量重命名（同名并入/清空解散，折叠态随改名迁移，落盘 previous-clone 回滚）；Deepin GroupName
> 导入进 group 字段不再拼备注，Xshell 目录树分组整串保留；command-config.conf 迁移落地——
> config::importers 新增宽容版 parse_deepin_commands/load_deepin_commands（QSettings unescape
> 一并补进 server 解析），命令页签新增「导入」按钮与计数提示（同名跳过/无效丢弃/解析跳过/
> 剥除冲突快捷键全部可见），merge_commands_into 纯函数合并（批内快捷键互撞增量登记剥绑保命令，
> confirm 统一 true），Deepin 服务器导入顺带迁移命令（两次独立落盘、计数拼进 server_notice）；
> app 侧 laterterm-deepin 依赖移除（crate 本体保留为格式真值参照）。

> 跳板机链 ProxyJump（2026-11-05）：`ServerEntry.jump` 按名引用 + `resolve_jump_chain` 环/自引/
> 悬空/超深（>8 跳）全表清理（宁断链不丢条目，载入/导入/保存三路兜底），SshOptions.jump 改
> Vec 逐跳 direct-tcpip 穿透（open_through + tunnels 保活逆序断开 + 超时 15s+10s×跳数双处联动），
> 面板编辑弹窗「跳板」行、列表「· 经 X」标注、删除阻断、改名改写引用、补输重试保留跳板快照；
> 跳板凭据 v1 只认密钥环已保存值（缺失硬错误不补输），回环 mock 两跳/三跳/拒转发负例 8 测试
> 全过，真机多跳验收待实机跳板环境（LATERTERM_SSH_JUMP 支持逗号分隔链）。

> SFTP 双栏面板部分交付（2026-11-05，任务4 保持 🚧）：后端 laterterm-ssh/src/sftp.rs 补齐——
> SftpHandle::spawn 跳板链 Option 单跳改 Vec 多跳（与 shell 同款 open_through 链式穿透 + tunnels
> 保活逆序断开，就绪超时按 15s+10s×跳数联动，spawn 侧再 +1s）；新增 stat/realpath 指令
> （进度条取总量 / 初始目录规范化）；**逐传输取消**走 Download/Upload 携带的 Arc<AtomicBool>
> 标志（传输循环逐块检查，置位即 Interrupted「传输已取消」）——close() 只是排队 Shutdown，
> 打断不了在途传输（russh 0.65 CHANNEL_CLOSE 缺口），文档与回环测试均已按此口径落实；
> 取消后半成品由传输工作线程用专属连接自行清理。app 侧 connect_sftp 复用抽出的
> resolve_credentials（shell/SFTP 同一套凭据解析，establish 注入点与 20 个既有测试零改动），
> SFTP 连接走独立后台函数与结果管道（产物是 SftpHandle 而非 TerminalSession）；UI 为
> 窗口级 SFTP 双栏覆盖层（880×560，settings_overlay 骨架）：本机 ↔ 远端双栏 uniform_list
> 文件列表（目录优先名称升序、5000 条截断提示、上级/刷新/本机目录对话框）、上传/下载
> 串行队列（每条传输一条独立 SSH 连接，浏览通道不被传输阻塞）、进度条（100ms/1% 节流，
> 总量未知降级为文本）与逐条取消、栏内新建目录/重命名/删除（二次确认），补输凭据弹窗与
> shell 连接共用（标题标注 SFTP），Esc 关闭面板。回环测试 8→13（SFTP 直连读列/上传/下载
> 字节级一致 + 进度触发 + stat/realpath、单跳/两跳穿透、取消中途置位→Interrupted + 半成品
> 可清理、拒转发负例），workspace lib/bins 328 通过 2 ignored（新增 sftp_panel 纯逻辑 11 测：
> 路径归一/穿越拒绝、父子推导、排序、截断、节流、状态机幂等、格式化）。**defer：断点续传**
> （russh-sftp open_with_flags + File start_seek 技术上已具备，待后续）；目录递归传输、
> 拖拽、右面板第四页签不做（范围控制）；远端文件名恒按 UTF-8（SFTP 协议规范，
> SshEndpoint.encoding 仅 shell 消费，GBK 老服务器 SFTP 名字不做转码）；真机（含跳板链）
> 传中文文件名与中途取消验收待实机。

> 启动光标错位修复（2026-11-04）：PTY/网格初始尺寸改为按窗口内容区估算（cell 7.83×19.2 @13px 实测系数），
> 消除「启动后 resize → bash readline 发 \r 把光标拉回提示符行首」的错位；隔离实测估算 193×42 与
> 首帧实测一致（零 resize）。reader 数据链增加 GPUI 定时器保活轮询（100ms，独立于 flume 唤醒通道），
> 并将字节处理/重绘通知移入窗口上下文（update_window + window.refresh），避免后台任务通知不触发重绘。
> 诊断开关：LATERTERM_PTY_TRACE（数据批次）、LATERTERM_GPUI_TRACE 需 vendor 分支（已移除每帧读取）。
> 注意：cargo run 为 debug 构建，GPUI 渲染在 debug 下明显偏慢，性能验证请用 cargo run --release。
> 主线整合（2026-11-02）：保留 SSH Agent、ProxyJump 后端（面板跳板配置待接线）、GBK/GB18030、SFTP、ZMODEM/trzsz、广播与首启引导；服务器旧文件一次性迁移到 settings.toml，保留当前同名条目，删除后不再复活。主机键按应用目录→旧应用路径→OpenSSH 路径逐主机兼容，已有记录不匹配或读取失败立即拒绝；连接建立/认证设置超时为 15s，不给长会话设置 15s 截止。广播复用真实控制键/文本/IME 提交回调，目标写入不回调以防环；Windows 自动传输暂为透明旁路，不吞数据，不宣称协议支持。

**目标**：远程不是"另一个功能"——同一个 termview，换 backend。

| # | 任务 | 说明 |
|---|------|------|
| 1 | laterterm-ssh | russh + russh-sftp；tokio 仅限网络层，PTY 走线程泵（架构既定）；SSH 会话与 SFTP 通道（多跳穿透/取消/stat）均已落地 | ✅
| 2 | 服务器管理 | 主机分组、连接配置、凭据入 keyring（laterterm-secrets） | ✅
| 3 | 跳板机链 | ProxyJump 多跳链式穿透（keyboard-interactive 凭跳板条目自身认证复用，逐跳生效） | ✅ |
| 4 | SFTP 面板 | 双栏文件传输，进度与取消已落地（2026-11-05）；断点续传 defer（open_with_flags + seek 已具备，待后续） | 🚧 |
| 5 | 编码 | GBK 远程会话转码（中文老服务器刚需；SFTP 路径恒 UTF-8 不转码） |
| 6 | trzsz | rz/sz 兼容协议，ZMODEM 类传输 |
| 7 | Deepin 配置一键导入 | `command-config.conf` / `server-config.conf` 已逆向（见 architecture.md）；server-config 导入已提前落地（含 Xshell .xsh 来源），命令模板迁移已落地 | ✅

**验收**：从 LaterTerm 完成一次「跳板机 → 内网机 → GBK 环境 → rz 拉文件」全链路操作。

---

## M4 · 发布打磨 ⏳

> 品牌与平台资源（2026-11-02）：用户原始 Logo 已入仓，标题栏使用编译期嵌入 PNG 与缓存对象，新增 9 档 PNG、多尺寸 ICO、标准 ICNS/Retina 槽位及可重复生成校验。Linux app_id / WM_CLASS / desktop / icon 统一为 laterterm；显式用户级安装或临时暂存，不在应用启动时改系统。Windows build.rs 嵌入图标 ID 1、默认 shell 按平台选择；macOS 提供 Info.plist 与未签名 .app 组装，原生交通灯安全区和关闭清理已接入。资源静态检查加入 Linux CI，Windows/macOS release/资源验证为手动原生工作流；本轮未触发远程 CI。详见 [跨平台与品牌资源](platforms.md)。
> 本轮资源与兼容门禁：隔离 HOME/XDG + 私有 DBus 下 workspace lib/bins 216 通过、1 个既有真实密钥环测试 ignored；SSH 回环 5 通过；图标测试 6、打包测试 8、Windows RC 路径纯逻辑测试 1 通过；严格 clippy、全工作区格式、构建、desktop/plist 与 diff 检查通过。Windows/macOS 原生工作流未触发，不能视为目标平台构建已验。
> 验收边界：隔离 HOME/XDG 与私有 DBus 的交互测试、图标/容器/临时安装布局检查不等于真实像素验收。本机无私有 X11/Wayland compositor，未对当前桌面注入按键；Windows/macOS 原生构建、Dock/任务栏与 DPI 视觉仍待验。deb/AppImage/DMG 打包链与自动发版已于 2026-11-07 落地但未经真 tag 实跑；MSI 因 laterterm-ssh 的 Windows 编译阻塞拆为后续任务；签名、公证仍未开始。
> Quake 下拉模式代码落地（2026-11-06，任务2 转 🚧）：portal GlobalShortcuts worker + 状态机 + 设置卡片 + 失焦收起轮询，落地差异（activate/minimize 无 unmap、几何仅启动生效、Wayland activate 可能被拒、ashpd 锁 0.11 关 default）见 architecture.md「系统级」段注记。workspace lib/bins 门禁 358 通过 / 2 ignored（新增 quake 状态机 10 测 + config roundtrip 1 测）；真机验收（桌面端授权弹窗、热键呼出/收起、失焦收起、portal 缺失降级文案）待 X11/Wayland 实机，热键注入不在本机能力内。
> 关于与检查更新落地（2026-11-07，任务5）：设置面板新增「关于」「更新」两节（SettingsSection 6→8 变体，ALL/label/锚标题/debug_key 无通配 match/滚动容器 children/手写 selectors 测试清单六处同步，漏加编译失败或测试断言兜底）；关于卡 = 40px 编译期嵌入 logo（branding::about_logo，标题栏 20px 版不动）+ v{CARGO_PKG_VERSION} 芯片 + 一句话简介 + 仓库链接（xdg-open）+ MIT OR Apache-2.0 许可行。选型：手动触发、不做后台自动轮询/启动即查——静默外连对用户是黑盒行为（隐私与离线可用性），用户点了才发一次请求；网络走 ureq 3 + rustls——gpui 内置 cx.http_client() 默认 NullHttpClient（vendor/gpui/src/app.rs）且未接线，不可用，ureq 同步 API 免 async runtime，`features=["rustls"]` 实为 rustls-no-provider + _ring + webpki-roots 组合（自带 ring provider），`cargo tree -e features -i rustls` 实测依赖树仅有 ring、无 aws-lc-rs/CMake 增量，根证书走 webpki-roots 内嵌数据包（非系统证书库，rustls-native-certs 不参与）；锁解析 3.5.1（`version="3"` 语义化取新，LaterScreen 参照锁 3.4.2，API 同型）。三态与离线文案：Idle「当前 v0.2.0」、UpToDate「当前 v0.2.0，已是最新」、失败一律「未能确认：{msg}」静默行不弹窗，绝不把失败说成「已是最新」；仓库无 release 时该端点 404 特判「仓库还没有发布版」；版本比较按点分段补 0 对齐、预发布后缀不猜，本地 CARGO_PKG_VERSION 与远端 tag 解析失败的错误文案分开报（防未来 auto-tag 注入带后缀版本时张冠李戴）。8s 全局超时封顶（同步 getaddrinfo 在 DNS 极端挂死下无法被超时中断，是本选型已知边界，LaterScreen 同款口径）。未认证 GitHub API 限额 60 次/时/IP，手动触发远达不到；共享出口 IP 撞 403/429 归入「GitHub 返回 {status}」静默文案，不做 token 支持与退避重试（手动再点即重试）。本轮自查：cargo check --workspace 通过；cargo test -p laterterm-app --bins 148 通过 / 1 ignored（新增 update_check 纯函数/状态机 10 测 + 设置面板三态离线渲染 1 测）；clippy -p laterterm-app 零告警。门禁 `cargo test --workspace --lib --bins` + 全工作区 clippy 由外层脚本统一执行。点击按钮的真实网络三路（200 已是最新 / Newer / 离线与 403）与 4K 缩放下新节渲染待真机验证。
> 打包矩阵与发版链收尾（2026-11-07，任务1 转 🚧）：Linux（tar.xz + deb + AppImage）、macOS（分步 hdiutil dmg）与版本读取健壮化（tomllib + SemVer，auto-tag/release 同步换掉依赖行序的 grep 法，照抄 LaterMD 实跑配方）已落地。落地差异与评审修正：Windows msi 拆为后续任务——硬阻塞在源码层（laterterm-ssh lib.rs:841 的 russh connect_env 仅 #[cfg(unix)] 且文件内无平台守卫，MSVC 下 E0599，v0.2.0 实跑失败被 continue-on-error 吞掉；app 无条件依赖该 crate），wxs 清单（WiX v4、固定 UpgradeCode、UI 序列/签名占位注释）已备未接线；ci.yml 补钉版 pyyaml==6.0.3（setup-python 发行版不含，缺装则 yml 契约测试首跑即红）；build-linux 钉 ubuntu-22.04 的同时加 setup-python 3.12（22.04 系统 python3.10 无 tomllib）；auto-tag checkout 钉 workflow_run.head_sha（防并发合入时 tag 指向未验证 HEAD）；linuxdeploy 钉 1-alpha-20251107-1 且以 --appimage-extract-and-run 跑（runner 无 FUSE，追 latest 会静默翻车）；release 收集通配预留 *.msi；分步 hdiutil 是防御性对齐 LaterMD 容量踩坑（LaterTerm 一步式 v0.1.0/v0.2.0 实跑成功过，非修已观察故障）。本地验证：脚本测试 26 通过（图标 6 + 打包 11 + workflow 契约 9，deb control 断言与 Cargo.toml 版本一致 + dpkg-deb --info 独立复核 + AppDir dry-run 过 desktop-file-validate + wxs XML well-formed）；cargo test --workspace --lib --bins 407 通过 / 4 ignored（exit 0）。边界：**CI 未实际触发，Windows/macOS 产物未实际构建验证**——deb 实机 apt 安装、AppImage 真打包、dmg 分步 hdiutil、CHANGELOG→Release 正文注入端到端均待真 tag 后实跑。预演口径：勿对旧 tag workflow_dispatch（checkout 取旧 ref 上的旧文件，且 gh release create 无 clobber 语义必撞已存在 Release）；正确做法 = 合并后在 main 推一次性 tag（push: tags 取 tag ref 上的新文件），预演重跑前先 gh release delete。三步实跑全绿（packaging.yml 原生配方 → 一次性 tag 预演 → 下版本 auto-tag 全自动链）方可转 ✅。

| # | 任务 |
|---|------|
| 1 | 三平台打包矩阵 🚧——2026-11-07 链路落地（**CI 未实际触发，Windows/macOS 产物未实际构建验证**）：Linux（tar.xz + deb + AppImage，钉 ubuntu-22.04 + 钉版 linuxdeploy）、macOS（script/package-macos.sh 组 .app + 分步 hdiutil dmg）、版本读取 tomllib+SemVer 健壮化（auto-tag + release，含 head_sha 钉定）均落地；Windows msi 拆为后续任务（前置 = laterterm-ssh 在 Windows 下编过，lib.rs:841 connect_env 无平台守卫 E0599；wxs 已备未接线）；签名/公证仅占位。边界与预演口径见上方 2026-11-07 注记 |
| 2 | Quake 下拉模式 🚧 ——2026-11-06 代码落地（真机验收待 X11/Wayland 实机）：设置→快捷键「系统级热键（Quake）」卡片（enabled + auto_hide_on_blur 持久化，热加载联动 worker）；Linux portal GlobalShortcuts worker（独立线程 async_io::block_on，先订阅 Activated 再 bind、注册 8s deadline + 1s 重试一次后降级、授权等待期发「等待桌面端授权确认」状态，bind 返回的 trigger_description 经 ShortcutsChanged 流跟随改绑，应用侧永不持有键位真值）。落地形态与 gpui 能力边界：呼出/收起 = activate/minimize 切换（gpui 无公开 unmap，初始 Shown 故首次热键为收起；任务栏保留图标），失焦收起走 is_window_active 200ms 轮询 + 600ms 宽限 + seen_focused 门（防 Wayland activate 被拒时呼出即收起死循环），顶部几何（全屏宽 × 50% 高）仅创建时生效——开关切换后下次启动生效，X11 定位可靠、Wayland 由合成器摆位，无逐帧滑出动画。非 Linux 平台无此卡片。ashpd 锁 0.11 且关 default-features（防 tokio flavor 混入破坏 block_on），详见 architecture.md 落地差异注记 |
| 3 | 广播输入（多会话同步键入） |
| 4 | 动效与无障碍打磨；首启动引导（默认 shell/字体探测）；终端光标样式可配置（方块/竖线/下划线 + 闪烁）——2026-11-06 落地：闪烁随 M1 完成，形状三选一（设置面板 + apply_appearance 热更新 + serde 旧配置兼容）已实现；Quake 全局热键走 portal GlobalShortcuts（方案与 LaterScreen 四坑结论见 architecture.md）；首启动引导 2026-11-06 落地：welcome 扩展为欢迎+引导一体覆盖层（默认 Shell / 终端字体芯片区 + 完成/跳过），跳过口径沿用「settings.toml 存在即跳过」（与现行 welcome_visible 同源，存量用户零打扰），探测收敛 onboarding.rs 纯函数（/etc/shells 解析 + fc-list :mono 探测：逗号别名取首、Emoji/SignWriting 按名过滤、预设命中标推荐置前，失败回退 FONT_PRESETS），结果经 flume 泵单发闩注入（测试零真机依赖），settings.shell 持久化（load 期 sanitize 坏值归 None）、选中「当前」芯片保持 None 不固化 $SHELL，shell 接线 spawn_tab/split_focused（仅对新标签/分屏生效）；本轮 workspace lib/bins 门禁 371 通过 / 3 ignored。边界：Windows/macOS 字体探测降级 FONT_PRESETS 静态列表、shell 探测沿用 COMSPEC/SystemRoot 不做原生枚举；已开窗格不随引导设置重启；首启弹层 4K 缩放不糊与选 shell 后新标签生效的实机验收待真机 |
| 5 | 关于与检查更新 ✅——2026-11-07 代码落地（真实网络三路待真机验证）：设置面板新增「关于」「更新」两节，关于卡（40px 编译期嵌入 logo + v{CARGO_PKG_VERSION} 芯片 + 仓库链接 + 许可行），检查更新三态（Idle 当前版本 / Newer 横幅紧贴按钮 + 去下载·知道了 / 失败「未能确认：{msg}」静默行不弹窗，绝不呈现为「已是最新」）；手动触发不后台轮询，ureq 3 + rustls/ring + webpki-roots 内嵌根证书（gpui 内置 http 通道为 NullHttpClient 不可用），8s 全局超时，GitHub 404 特判「仓库还没有发布版」，未认证限额 60 次/时/IP；本地/远端版本解析失败文案分开报。落地取舍与门禁数字见上方 2026-11-07 注记 |

**验收**：v0.1.0 发布，README 附真机截图（复用 LaterMD 的无头截图生成思路可选项）。

---

## 依赖与风险台账

| 风险 | 等级 | 缓解 |
|------|------|------|
| GPUI 上游 API 漂移（0.2.x 迭代快） | 中 | 锁版本 + termview 已内化，升级自主可控 |
| GPUI Wayland/IME 成熟度 | 中 | XWayland 兜底；IME 是 M1 硬验收，早暴露 |
| termview 内化后维护负担 | 低 | 8k 行规模可控；改动守 MIT OR Apache-2.0 许可头 |
| alacritty_terminal 协议长尾 | 低 | 行业标准答案，跟版本即可 |
