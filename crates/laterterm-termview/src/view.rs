//! Main terminal view component for GPUI.
//!
//! This module provides [`TerminalView`], the primary component for embedding terminals
//! in GPUI applications. It manages:
//!
//! - **I/O Streams**: Accepts arbitrary [`Read`]/[`Write`]
//!   streams, allowing integration with any PTY implementation
//! - **Event Handling**: Keyboard and mouse input, with configurable callbacks
//! - **Rendering**: Efficient canvas-based rendering via [`TerminalRenderer`]
//! - **Configuration**: Font, colors, dimensions, and padding via [`TerminalConfig`]
//!
//! # Architecture
//!
//! The terminal uses a push-based async I/O architecture:
//!
//! 1. A background thread reads bytes from the PTY stdout in 4KB chunks
//! 2. Bytes are sent through a [flume](https://docs.rs/flume) channel to an async task
//! 3. The async task processes bytes through the VTE parser and calls `cx.notify()`
//! 4. GPUI repaints the terminal with the updated grid
//!
//! This approach ensures the terminal only wakes when data arrives, avoiding polling.
//!
//! # Thread Safety
//!
//! - [`TerminalView`] itself is not `Send` (it contains GPUI handles)
//! - The stdin writer is wrapped in `Arc<parking_lot::Mutex<>>` for thread-safe writes
//! - Callbacks ([`ResizeCallback`], [`KeyHandler`]) must be `Send + Sync`
//!
//! # Example
//!
//! ```ignore
//! use gpui::{Context, Edges, px};
//! use gpui_terminal::{ColorPalette, TerminalConfig, TerminalView};
//! use laterterm_core::TerminalSession;
//!
//! // In a GPUI window context（session 由 laterterm-local / laterterm-ssh 提供）:
//! let terminal = cx.new(|cx| {
//!     TerminalView::new(session, TerminalConfig::default(), cx)
//!         .with_exit_callback(|_, cx| {
//!             cx.quit();
//!         })
//! });
//!
//! // Focus the terminal to receive keyboard input
//! terminal.read(cx).focus_handle().focus(window);
//! ```

use crate::colors::ColorPalette;
use crate::event::{GpuiEventProxy, TerminalEvent};
use crate::input::keystroke_to_bytes;
use crate::render::{FrameSnapshot, TerminalRenderer, search_match_rect, selection_row_spans};
use crate::terminal::TerminalState;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side};
use alacritty_terminal::selection::{
    Selection as AlacSelection, SelectionType as AlacSelectionType,
};
use alacritty_terminal::vte::ansi::{ClearMode, Handler as AnsiHandler};
use gpui::{
    Edges, ElementInputHandler, EntityInputHandler, UTF16Selection, prelude::FluentBuilder, *,
};
use laterterm_core::{BackendKind, TerminalSession};
use std::io::{Read, Write};
use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

// 诊断只在进程启动时开启；不记录字体名或终端内容。
fn font_trace_requested(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

fn font_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| font_trace_requested(std::env::var_os("LATERTERM_FONT_TRACE").as_deref()))
}

fn font_trace_reason(
    enabled: bool,
    needs_measurement: bool,
    font_changed: bool,
) -> Option<&'static str> {
    if enabled && needs_measurement {
        Some(if font_changed {
            "font_change"
        } else {
            "initial"
        })
    } else {
        None
    }
}

/// 绘制期性能观测：`LATERTERM_RENDER_TRACE=1` 时每帧输出一行锁窗口与绘制耗时。
/// 与 font_trace 的分工：font_trace 只在重测量帧打印，采不到稳态帧；本通道专供
/// 「锁持有时长」前后对照采集（快照优化前=整段持锁绘制，优化后=仅快照复制）。
/// 字段口径钉死（改造前后同名同义，对照只认 paint_lock_hold_ms）：
/// - paint_lock_wait_ms  等待 paint 锁的时长
/// - paint_lock_hold_ms  锁持有时长＝锁获取成功→guard 释放的完整窗口
///   （快照前＝历史行读取+实体写+网格绘制+三段覆盖层全程；
///   快照后＝仅可见网格快照复制）
/// - snapshot_ms         快照复制本体时长（快照后恒等于 paint_lock_hold_ms；
///   快照前的代码无此字段）
/// - paint_ms            渲染器 paint(_frame) 本体时长（不含覆盖层）
/// - overlay_ms          搜索/选区/预编辑三段覆盖层时长
/// - paint_closure_ms    canvas paint 闭包整段时长
/// - cells               本帧绘制覆盖的可见格数（行×列）
fn render_trace_requested(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

fn render_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        render_trace_requested(std::env::var_os("LATERTERM_RENDER_TRACE").as_deref())
    })
}

/// 终端光标形状。与 laterterm-config 的同名枚举一一对应，由 app 层映射
/// （两 crate 不互依，各自持一份定义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorStyle {
    /// 方块（默认，占满整格）。
    Block,
    /// 竖线（光标列左缘的细竖条）。
    Bar,
    /// 下划线（贴格底的横条）。
    Underline,
}

/// Configuration for terminal creation and runtime updates.
///
/// This struct defines the terminal's appearance and behavior, including
/// grid dimensions, font settings, scrollback buffer, and color scheme.
///
/// # Default Values
///
/// | Field | Default |
/// |-------|---------|
/// | `cols` | 80 |
/// | `rows` | 24 |
/// | `font_family` | "monospace" |
/// | `font_size` | 14px |
/// | `scrollback` | 10000 |
/// | `line_height_multiplier` | 1.0 |
/// | `padding` | 0px all sides |
/// | `colors` | Default palette |
///
/// # Example
///
/// ```ignore
/// use gpui::{Edges, px};
/// use gpui_terminal::{ColorPalette, TerminalConfig};
///
/// let config = TerminalConfig {
///     cols: 120,
///     rows: 40,
///     font_family: "JetBrains Mono".into(),
///     font_size: px(13.0),
///     scrollback: 50000,
///     line_height_multiplier: 1.0,
///     padding: Edges::all(px(10.0)),
///     colors: ColorPalette::builder()
///         .background(0x1a, 0x1a, 0x1a)
///         .foreground(0xe0, 0xe0, 0xe0)
///         .build(),
/// };
/// ```
///
/// # Runtime Updates
///
/// Configuration can be updated at runtime via [`TerminalView::update_config`].
/// This is useful for implementing features like dynamic font sizing:
///
/// ```ignore
/// terminal.update(cx, |terminal, cx| {
///     let mut config = terminal.config().clone();
///     config.font_size += px(1.0);
///     terminal.update_config(config, cx);
/// });
/// ```

#[derive(Clone, Debug)]
pub struct TerminalConfig {
    /// Number of columns (character width) in the terminal
    pub cols: usize,

    /// Number of rows (lines) in the terminal
    pub rows: usize,

    /// Font family name (e.g., "Fira Code", "JetBrains Mono")
    pub font_family: String,

    /// Font size in pixels
    pub font_size: Pixels,

    /// Maximum number of scrollback lines to keep in history
    pub scrollback: usize,

    /// Multiplier for line height to accommodate tall glyphs (e.g., nerd fonts)
    /// Default is 1.0 (no extra height)
    pub line_height_multiplier: f32,

    /// Padding around the terminal content (top, right, bottom, left)
    /// The padding area renders with the terminal's background color
    pub padding: Edges<Pixels>,

    /// Color palette for terminal colors (16 ANSI colors, 256 extended colors,
    /// foreground, background, and cursor colors)
    pub colors: ColorPalette,

    /// 是否启用连字（calt）。默认启用；关闭后 `->` 等序列按单字符渲染。
    pub ligatures: bool,

    /// 备选字体链：主字体缺字时按序回退（如 CJK 字体）；空表示系统默认回退。
    pub font_fallbacks: Vec<String>,

    /// 光标闪烁。默认开启；关闭后光标常显。
    pub cursor_blink: bool,

    /// 光标形状：方块 / 竖线 / 下划线。默认方块。
    pub cursor_style: CursorStyle,

    /// 平滑滚动：滚轮/触控板按像素累积连续滚动，而不是一次跳一整行。
    /// 默认开启。
    pub smooth_scrolling: bool,

    /// 选中即复制：左键拖拽/双击/三击出有效选区后松开，自动写入剪贴板
    /// （Deepin 终端同款体验）。默认开启。
    pub copy_on_select: bool,

    /// 终端 bell 时发送系统桌面通知。默认关闭（bell 走视觉提示，测试环境不外发）。
    pub bell_notification: bool,

    /// kitty keyboard protocol 支持开关（默认 false）。开启后允许应用经
    /// `CSI > 1u` / `CSI ? u` 协商启用 disambiguate 层编码（Esc 与修饰文本键
    /// 转 CSI u，见 input.rs）。默认关闭：本端编码表是手写风险面，未经真实
    /// 应用（fish/tmux）验证前不默认开启——对齐 alacritty_terminal Config
    /// 的默认（term/mod.rs `kitty_keyboard: Default::default()` = false）。
    pub kitty_keyboard: bool,

    /// 终端背景不透明度（0.15~1.0）。1.0 = 完全不透明（历史行为，default
    /// 底色满幅绘制）；<1.0 时整幅 default 底色跳绘、单元格背景 quad 按
    /// alpha 缩放，窗口底色由宿主层（如 LaterTerm 工作区根容器）承载。
    pub background_opacity: f32,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            font_family: "monospace".into(),
            font_size: px(14.0),
            scrollback: 10000,
            line_height_multiplier: 1.0,
            padding: Edges::all(px(0.0)),
            colors: ColorPalette::default(),
            ligatures: true,
            font_fallbacks: Vec::new(),
            cursor_blink: true,
            cursor_style: CursorStyle::Block,
            smooth_scrolling: true,
            copy_on_select: true,
            bell_notification: false,
            kitty_keyboard: false,
            background_opacity: 1.0,
        }
    }
}

/// Callback type for PTY resize notifications.
///
/// This callback is invoked when the terminal grid dimensions change,
/// typically due to window resizing. The callback receives the new
/// column and row counts.
///
/// # Arguments
///
/// * `cols` - New number of columns (characters wide)
/// * `rows` - New number of rows (lines tall)
///
/// # Thread Safety
///
/// This callback must be `Send + Sync` as it may be called from the render thread.
///
/// # Example
///
/// ```ignore
/// // 默认回调已把 resize 转发给 TerminalSession；仅特殊后端才需要覆盖：
/// terminal.with_resize_callback(move |cols, rows| {
///     custom_backend.resize(cols, rows);
/// });
/// ```
pub type ResizeCallback = Box<dyn Fn(usize, usize) + Send + Sync>;

/// Callback type for key event interception.
///
/// This callback is invoked before the terminal processes a key event,
/// allowing you to intercept and handle specific key combinations.
///
/// # Arguments
///
/// * `event` - The key down event from GPUI
///
/// # Returns
///
/// * `true` - Consume the event (terminal will not process it)
/// * `false` - Let the terminal handle the event normally
///
/// # Thread Safety
///
/// This callback must be `Send + Sync`.
///
/// # Example
///
/// ```ignore
/// terminal.with_key_handler(|event| {
///     let keystroke = &event.keystroke;
///
///     // Intercept Ctrl++ for font size increase
///     if keystroke.modifiers.control && (keystroke.key == "+" || keystroke.key == "=") {
///         // Handle font size increase
///         return true; // Consume the event
///     }
///
///     // Intercept Ctrl+- for font size decrease
///     if keystroke.modifiers.control && keystroke.key == "-" {
///         // Handle font size decrease
///         return true;
///     }
///
///     false // Let terminal handle all other keys
/// });
/// ```
pub type KeyHandler = Box<dyn Fn(&KeyDownEvent) -> bool + Send + Sync>;

/// 实际用户输入写入成功后的观察口（控制键与文本/IME 提交共用，预编辑不触发）。
pub type InputCallback = Box<dyn Fn(&[u8]) + Send + Sync>;

/// Callback for terminal bell events.
///
/// This callback is invoked when the terminal bell is triggered (BEL character,
/// ASCII 0x07), allowing you to play a sound or show a visual indicator.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
///
/// # Example
///
/// ```ignore
/// terminal.with_bell_callback(|window, cx| {
///     // Option 1: Visual bell (flash the window or show an indicator)
///     // Option 2: Play a sound
///     // Option 3: Notify the user via system notification
/// });
/// ```
pub type BellCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

/// Callback for terminal title changes.
///
/// This callback is invoked when the terminal title changes via escape sequences
/// (OSC 0, OSC 2), allowing you to update the window or tab title.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
/// * `title` - The new title string
///
/// # Example
///
/// ```ignore
/// terminal.with_title_callback(|window, cx, title| {
///     // Update the window title
///     // Or update a tab label in a tabbed interface
///     println!("Terminal title changed to: {}", title);
/// });
/// ```
pub type TitleCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

/// Callback for clipboard store requests.
///
/// This callback is invoked when the terminal wants to store data to the clipboard
/// via OSC 52 escape sequence. Applications like tmux and vim can use this to
/// copy text to the system clipboard.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
/// * `text` - The text to store in the clipboard
///
/// # Example
///
/// ```ignore
/// use gpui_terminal::Clipboard;
///
/// terminal.with_clipboard_store_callback(|window, cx, text| {
///     if let Ok(mut clipboard) = Clipboard::new() {
///         clipboard.copy(text).ok();
///     }
/// });
/// ```
pub type ClipboardStoreCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

/// 复制成功通知回调。
///
/// 选区复制成功（选中即复制路径）后触发，携带写入剪贴板的字符数，
/// 供宿主弹「已复制 N 字符」提示。
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
/// * `chars` - 实际写入剪贴板的字符数
///
/// # Example
///
/// ```ignore
/// terminal.with_copy_notice_callback(|window, cx, chars| {
///     // 在窗口右下角弹 toast：已复制 chars 个字符
/// });
/// ```
pub type CopyNoticeCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, usize)>;

/// 粘贴请求回调。
///
/// 用户在终端视图内按下粘贴快捷键（Ctrl+Shift+V / Shift+Insert）时触发，
/// 由宿主决定读取剪贴板与多行确认流程；搜索打开期间不触发（按键被搜索拦截）。
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
///
/// # Example
///
/// ```ignore
/// terminal.with_paste_request_callback(|window, cx| {
///     // 读剪贴板 → 多行则弹确认 → 写 PTY
/// });
/// ```
pub type PasteRequestCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

/// Callback for terminal exit events.
///
/// This callback is invoked when the terminal process exits (e.g., shell exits,
/// process terminates). This is detected when the PTY reader reaches EOF.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
///
/// # Example
///
/// ```ignore
/// terminal.with_exit_callback(|window, cx| {
///     // Option 1: Quit the application
///     cx.quit();
///
///     // Option 2: Close this terminal tab/pane
///     // terminal_manager.close_terminal(terminal_id);
///
///     // Option 3: Show an exit message
///     // show_notification("Terminal exited");
/// });
/// ```
pub type ExitCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

/// The main terminal view component for GPUI applications.
///
/// `TerminalView` is a GPUI entity that implements the [`Render`] trait,
/// providing a complete terminal emulator that can be embedded in any GPUI application.
///
/// # Responsibilities
///
/// - **Terminal State**: Manages the grid, cursor, and colors via [`TerminalState`]
/// - **I/O Streams**: Reads from PTY stdout and writes to PTY stdin
/// - **Event Handling**: Processes keyboard, mouse, and resize events
/// - **Rendering**: Paints text, backgrounds, and cursor via [`TerminalRenderer`]
/// - **Callbacks**: Dispatches events to user-provided callbacks
///
/// # Creating a Terminal
///
/// Use [`TerminalView::new`] within a GPUI entity context:
///
/// ```ignore
/// let terminal = cx.new(|cx| {
///     TerminalView::new(writer, reader, config, cx)
///         .with_resize_callback(resize_callback)
///         .with_exit_callback(|_, cx| cx.quit())
/// });
/// ```
///
/// # Focus
///
/// The terminal must be focused to receive keyboard input:
///
/// ```ignore
/// terminal.read(cx).focus_handle().focus(window);
/// ```
///
/// # Callbacks
///
/// Configure behavior through builder methods:
///
/// - [`with_resize_callback`](Self::with_resize_callback) - PTY size changes
/// - [`with_exit_callback`](Self::with_exit_callback) - Process exit
/// - [`with_key_handler`](Self::with_key_handler) - Key event interception
/// - [`with_bell_callback`](Self::with_bell_callback) - Terminal bell
/// - [`with_title_callback`](Self::with_title_callback) - Title changes
/// - [`with_clipboard_store_callback`](Self::with_clipboard_store_callback) - Clipboard writes
///
/// # Thread Safety
///
/// `TerminalView` is not `Send` as it contains GPUI handles. The stdin writer
/// is internally wrapped in `Arc<parking_lot::Mutex<>>` for safe concurrent access.
pub struct TerminalView {
    /// The terminal state managing the grid and VTE parser
    state: TerminalState,

    /// 会话后端类别（构造时从 `session.backend()` 定格）：cwd 继承只对本地
    /// 会话有意义——SSH 会话即便远端 shell 上报 OSC 7，路径也属远端，
    /// 不能喂给本地 spawn（见 [`Self::cwd`]）。
    backend: BackendKind,

    /// The renderer for drawing terminal content
    renderer: TerminalRenderer,

    /// 标记下一次字体度量对应配置变更而非首次渲染。
    font_trace_changed: bool,

    /// Focus handle for keyboard event handling
    focus_handle: FocusHandle,

    /// Writer for sending input to the terminal process
    stdin_writer: Arc<parking_lot::Mutex<Box<dyn Write + Send>>>,

    /// Receiver for terminal events from the event proxy
    event_rx: mpsc::Receiver<TerminalEvent>,

    /// Configuration used to create this terminal
    config: TerminalConfig,

    /// Async task that reads bytes and notifies the view (push-based)
    #[allow(dead_code)]
    _reader_task: Task<()>,
    /// 光标闪烁任务（可配置开关；关闭时不产生重绘）
    _blink_task: Task<()>,
    /// 数据保活任务：兜底唤醒丢失时强制刷新（每 200ms 检查一次数据版本）
    _sync_task: Task<()>,
    /// 已渲染的数据版本号（主线程侧），与 state.data_epoch() 对账。
    rendered_epoch: u64,
    /// 光标闪烁当前相位（true=可见）。
    blink_on: Arc<std::sync::atomic::AtomicBool>,
    /// 平滑滚动残差：不足一行的像素滚动累积于此（单位：行，可为小数）。
    scroll_residual: f32,

    /// 绘制期快照缓冲池：帧尾归还 [`FrameSnapshot`]，下帧 canvas 闭包取出并用
    /// [`FrameSnapshot::capture_into`] 复用 rows/row 容量（稳态帧零行分配）。
    /// 只有主线程 canvas 闭包经此互斥体取还，实际无竞争。
    snapshot_cache: Arc<parking_lot::Mutex<Option<FrameSnapshot>>>,

    /// Callback to notify the PTY about size changes
    resize_callback: Option<Arc<ResizeCallback>>,
    /// 已同步到会话的首帧尺寸；会话初始尺寸不一定等于网格配置。
    session_size: Arc<parking_lot::Mutex<Option<(usize, usize)>>>,

    /// Optional callback to intercept key events before terminal processing
    key_handler: Option<Arc<KeyHandler>>,

    /// 用户输入提交观察口；广播目标 write_input 不触发，避免递归广播。
    input_callback: Option<InputCallback>,

    /// Callback for terminal bell events
    bell_callback: Option<BellCallback>,

    /// 最近一次绘制的内容原点（padding 内侧）；URL 点击检测用
    pub(crate) last_content_origin: Option<Point<Pixels>>,

    /// 鼠标悬停命中的 URL：`(url, 屏幕行, 起始列, 结束列)`（结束列开区间）。
    /// 悬停显示下划线 + 小手光标；Ctrl+左键才跳转。
    url_hover: Option<(String, usize, usize, usize)>,

    /// Callback for terminal title changes
    title_callback: Option<TitleCallback>,

    /// Callback for clipboard store requests
    clipboard_store_callback: Option<ClipboardStoreCallback>,

    /// 复制成功通知回调（选中即复制路径触发，携带字符数）
    copy_notice_callback: Option<CopyNoticeCallback>,

    /// 粘贴请求回调（终端视图内粘贴快捷键触发，由宿主走确认流程）
    paste_request_callback: Option<PasteRequestCallback>,

    /// Callback for terminal exit events
    exit_callback: Option<ExitCallback>,

    /// 滚动位置变化回调（供外部绘制滚动条指示器等）。
    scroll_callback: Option<ScrollCallback>,

    /// 上次上报的滚动摘要；与当前值不同才触发回调，防止 notify 风暴。
    last_scroll: Option<ScrollSummary>,
    /// 防抖在途标记：输出洪峰时合并为每 150ms 至多一次宿主刷新。
    scroll_report_scheduled: bool,

    /// 已按 SGR 1006 上报给应用的按下键（目前仅左键）；
    /// 用于拖拽移动的按钮编码与 release 配对，take() 释放防悬挂标记。
    mouse_down_reported: Option<MouseButton>,

    /// 最近一次移动上报的去重键（视口列, 视口行, 修饰位）；
    /// 同格且修饰位未变的连续 move 不重发（按 cell 上报语义，防逐像素刷 PTY）。
    last_motion_report: Option<(usize, usize, u8)>,

    /// Text currently being composed by the system input method.
    marked_text: String,

    /// 回滚搜索状态；打开时终端按键被拦截为搜索输入，不再写入 PTY。
    search: Option<SearchState>,

    /// 所属窗口句柄；首帧 render 时记录，PTY 数据路径用它把 notify
    /// 绑定到窗口上下文，避免后台任务通知不触发窗口重绘。
    window_handle: Option<AnyWindowHandle>,
}

/// 搜索栏状态。
#[derive(Clone)]
struct SearchState {
    query: String,
    matches: Vec<crate::terminal::SearchMatch>,
    /// 当前命中的下标（在 matches 中循环移动）。
    current: usize,
    /// 大小写敏感（搜索栏 Aa 按钮切换）。
    case_sensitive: bool,
    /// 正则模式（Alt+R / .* 钮切换；开启时全字禁用——互斥语义，
    /// 正则用户自行写 \b）。
    regex: bool,
    /// 全字匹配（Alt+W / W 钮切换；正则开启时置灰无效）。
    whole_word: bool,
    /// 非法正则的内联报错（Some 时计数区红字「正则无效：<msg>」替代，
    /// 上次有效结果保留不动不滚动）。
    regex_error: Option<String>,
    /// 查询/选项变化后的防抖搜索在途标记（150ms 合并连击；
    /// Enter 在此标记置位时同步提交挂起的搜索）。
    search_pending: bool,
}

/// 搜索栏计数文案：空查询 0/0 / 无命中 / n/m 三分支；非法正则时
/// 「正则无效：<msg>」红字替代计数（与计数共用一个槽位）。
fn search_counter_text(search: &SearchState) -> String {
    if let Some(error) = &search.regex_error {
        format!("正则无效：{error}")
    } else if search.query.is_empty() {
        "0/0".to_string()
    } else if search.matches.is_empty() {
        "无命中".to_string()
    } else {
        format!("{}/{}", search.current + 1, search.matches.len())
    }
}

/// 搜索栏 24px 方钮工厂（Aa / .* / W / ▲ / ▼）。
///
/// termview 本地实现：ui_button / icon_button 是 app 层（workspace.rs）私有
/// 函数，termview 反向依赖成环，故按 ui_button 体系（描边 + hover 底色）
/// 对齐做 24px 紧凑版；色值沿用本搜索栏既定的 Tokyo Night 内联值。
/// 所有钮 on_mouse_down 首句 `cx.stop_propagation()`（先例 workspace.rs
/// 右键菜单项）：点击不落入底层网格建选区；禁用钮只吞点击不动作。
fn search_bar_button(
    cx: &mut Context<TerminalView>,
    label: &'static str,
    active: bool,
    disabled: bool,
    on_click: impl Fn(&mut TerminalView, &mut Context<TerminalView>) + 'static,
) -> Div {
    // 开启 = 主题黄描边高亮；关闭 = 暗色可点；禁用 = 更暗无 hover
    let (text, border) = if disabled {
        (rgb(0x363B54), rgb(0x24283B))
    } else if active {
        (rgb(0xE0AF68), rgb(0xE0AF68))
    } else {
        (rgb(0x565A78), rgb(0x363B54))
    };
    let base = div()
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .text_size(px(11.0))
        .text_color(text)
        .child(label);
    if disabled {
        base.on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| cx.stop_propagation()),
        )
    } else {
        base.hover(|style| style.bg(rgb(0x2F3147))).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                on_click(this, cx);
            }),
        )
    }
}

/// 会话退出事件；不依赖终端是否可见。
pub struct TerminalExited;

/// 右键菜单请求：宿主（工作区）据此在窗口坐标处渲染上下文菜单。
#[derive(Clone, Debug)]
pub struct TerminalMenuRequested {
    /// 右键发生时的窗口坐标，作为菜单锚点。
    pub position: Point<Pixels>,
    /// 右键处的 URL（若命中），供「在浏览器打开」使用。
    pub url: Option<String>,
}

/// 滚动位置变化回调：调用方按需读取 [`TerminalView::scroll_summary`]，
/// 并用传入的上下文通知宿主刷新（如工作区的滚动条指示器）。
pub type ScrollCallback = Box<dyn Fn(&mut Context<TerminalView>)>;

/// 滚动状态摘要，用于外部绘制滚动条指示器。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollSummary {
    /// 显示区相对底部向上偏移的行数（0 = 位于底部）。
    pub display_offset: usize,
    /// 屏幕可见行数。
    pub screen_lines: usize,
    /// 屏幕行数 + 回滚历史行数。
    pub total_lines: usize,
}

impl ScrollSummary {
    /// 是否有可滚动的历史（总行数超出屏幕行数）。
    pub fn scrollable(&self) -> bool {
        self.total_lines > self.screen_lines
    }

    /// 滑块长度占比与顶端位置占比（0.0–1.0，占满可用轨道）。
    /// 无历史时返回 None。
    pub fn thumb(&self) -> Option<(f32, f32)> {
        if !self.scrollable() {
            return None;
        }
        let scrollable = (self.total_lines - self.screen_lines) as f32;
        let thumb = (self.screen_lines as f32 / self.total_lines as f32).clamp(0.08, 1.0);
        // display_offset=0 表示在底部，历史顶部对应最大 offset。
        let traveled = 1.0 - (self.display_offset as f32 / scrollable).clamp(0.0, 1.0);
        let top = traveled * (1.0 - thumb);
        Some((thumb, top))
    }
}

impl EventEmitter<TerminalExited> for TerminalView {}

impl EventEmitter<TerminalMenuRequested> for TerminalView {}

impl TerminalView {
    /// Create a new terminal bound to a [`TerminalSession`].
    ///
    /// 字节流在构造时从会话一次性取出（`take_streams`），读泵与写端全部
    /// 由视图接管；resize 默认回调转发给会话（本地 = PTY resize，
    /// SSH = 窗口变更转义序列），可用 [`with_resize_callback`] 覆盖。
    ///
    /// # Arguments
    ///
    /// * `session` - 会话契约（本地 PTY / SSH 远程共用）
    /// * `config` - Terminal configuration (dimensions, font, etc.)
    /// * `cx` - GPUI context for this view
    ///
    /// # Returns
    ///
    /// A new `TerminalView` instance ready to be rendered.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // In a GPUI window context:
    /// let terminal = cx.new(|cx| {
    ///     TerminalView::new(session, TerminalConfig::default(), cx)
    /// });
    /// ```
    pub fn new(
        session: Arc<dyn TerminalSession>,
        config: TerminalConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        // 取出会话字节流：只允许一次，双读者会分裂字节流
        let streams = session.take_streams().expect("会话字节流已被取走");
        let stdin_writer: Box<dyn Write + Send> = streams.writer;
        let stdout_reader = streams.reader;
        // 后端类别构造时定格（cwd 继承的本地/远端判别依据）
        let backend = session.backend();

        // Create event channel for terminal events
        let (event_tx, event_rx) = mpsc::channel();

        // Clone event_tx for the reader task to send Exit event when PTY closes
        let exit_event_tx = event_tx.clone();

        // Create event proxy for alacritty
        let event_proxy = GpuiEventProxy::new(event_tx.clone());

        // Create terminal state
        let mut state = TerminalState::new(
            config.cols,
            config.rows,
            config.scrollback,
            config.kitty_keyboard,
            event_proxy,
        );
        // 注入桌面通知通道：OSC 9/777 经 TerminalState 直发 DesktopNotice
        state.set_notice_tx(event_tx);

        // Create renderer with font settings and color palette
        let mut renderer = TerminalRenderer::new(
            config.font_family.clone(),
            config.font_size,
            config.line_height_multiplier,
            config.colors.clone(),
            config.ligatures,
            config.font_fallbacks.clone(),
            config.cursor_style,
        );
        // 首帧即按配置的不透明度绘制（update_config 同口径钳制，见其注释）
        renderer.background_opacity = config.background_opacity.clamp(0.15, 1.0);

        // Create focus handle
        let focus_handle = cx.focus_handle();

        // Wrap stdin writer in Arc<Mutex> for thread-safe access
        let stdin_writer = Arc::new(parking_lot::Mutex::new(stdin_writer));

        // Create async channel for bytes (push-based notification)
        // Using flume instead of smol::channel because flume is executor-agnostic
        // and properly wakes GPUI's async executor when data arrives
        let (bytes_tx, bytes_rx) = flume::unbounded::<Vec<u8>>();

        // Spawn background thread to read from stdout
        // This thread sends bytes through the async channel
        thread::spawn(move || {
            Self::read_stdout_blocking(stdout_reader, bytes_tx);
        });

        // Spawn async task that awaits on the channel and notifies the view
        // This is push-based: the task blocks until bytes arrive, then immediately notifies
        let reader_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                // 等待字节；用 GPUI 定时器与 flume 唤醒竞争——GPUI Linux 后台任务的
                // 唤醒在部分场景下会丢失(实测 reader_task 曾永久饿死, 网格停在空首帧)，
                // 定时器走独立的 dispatch_after 通道，保证数据最多延迟 100ms 必被处理。
                let wakeup = futures::future::select(
                    bytes_rx.recv_async(),
                    cx.background_executor().timer(Duration::from_millis(100)),
                )
                .await;
                match wakeup {
                    futures::future::Either::Left((result, _timer)) => match result {
                        Ok(bytes) => {
                            // Process bytes and notify the view.
                            // 优先走窗口上下文: 后台任务的 cx.notify() 只标记实体 dirty,
                            // 不保证平台请求新帧(X11 由 60Hz 定时器检查 is_dirty, Wayland 靠
                            // 合成器帧回调); 必须在窗口上下文里显式 window.refresh() 才能让
                            // 终端输出及时上屏 —— 否则出现「打开后只有左上角光标, 提示符
                            // 停留在网格里不上屏」的现象。
                            let window_handle =
                                this.update(cx, |view, _| view.window_handle).ok().flatten();
                            let result = match window_handle {
                                Some(handle) => cx.update_window(handle, |_root, window, app| {
                                    this.update(app, |view, cx| {
                                        view.state.process_bytes(&bytes);
                                        cx.notify();
                                        window.refresh();
                                    })
                                }),
                                None => Ok(this.update(
                                    cx,
                                    |view: &mut Self, cx: &mut Context<Self>| {
                                        view.state.process_bytes(&bytes);
                                        cx.notify();
                                    },
                                )),
                            };
                            if result.is_err() {
                                // View was dropped, exit
                                break;
                            }
                        }
                        Err(_) => {
                            // Channel closed - PTY has finished, send Exit event
                            let _ = exit_event_tx.send(TerminalEvent::Exit);
                            // 生命周期事件必须在绘制之外派发，隐藏终端也能被父视图回收。
                            let _ = this.update(cx, |_view, cx: &mut Context<Self>| {
                                cx.emit(TerminalExited);
                                cx.notify();
                            });
                            break;
                        }
                    },
                    futures::future::Either::Right(_) => {
                        // 保活定时器到期：无数据则继续等待。
                    }
                }
            }
        });

        // 光标闪烁：每 530ms 翻转相位；关闭闪烁时不产生多余重绘。
        let blink_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(530))
                    .await;
                let changed = this
                    .update(cx, |view, cx| {
                        let enabled = view.config.cursor_blink;
                        let next = if enabled {
                            !view.blink_on.load(std::sync::atomic::Ordering::Relaxed)
                        } else {
                            true
                        };
                        let changed = view
                            .blink_on
                            .swap(next, std::sync::atomic::Ordering::Relaxed)
                            != next;
                        if changed {
                            cx.notify();
                        }
                        changed
                    })
                    .unwrap_or(false);
                let _ = changed;
            }
        });

        // 数据保活：每 200ms 对账数据版本与已渲染版本，不一致则强制刷新。
        // 兜底 GPUI Linux 后台任务唤醒丢失的场景（实测会导致启动提示符不上屏）。
        let sync_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(200))
                    .await;
                if this
                    .update(cx, |view, cx| {
                        let data = view.state.data_epoch();
                        let stale = data != view.rendered_epoch;
                        if stale {
                            view.rendered_epoch = data;
                            cx.notify();
                        }
                        stale
                    })
                    .unwrap_or(false)
                {
                    // 实体已释放
                    break;
                }
            }
        });

        Self {
            state,
            backend,
            renderer,
            font_trace_changed: false,
            focus_handle,
            stdin_writer,
            event_rx,
            config,
            _reader_task: reader_task,
            _blink_task: blink_task,
            _sync_task: sync_task,
            scroll_residual: 0.0,
            snapshot_cache: Arc::new(parking_lot::Mutex::new(None)),
            rendered_epoch: 0,
            blink_on: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            // 默认 resize 回调转发给会话；with_resize_callback 可整体覆盖
            resize_callback: {
                let session_for_resize = Arc::clone(&session);
                Some(Arc::new(Box::new(move |cols: usize, rows: usize| {
                    if let Err(error) = session_for_resize.resize(cols as u16, rows as u16) {
                        eprintln!("会话 resize 失败: {error}");
                    }
                })))
            },
            session_size: Arc::new(parking_lot::Mutex::new(None)),
            key_handler: None,
            input_callback: None,
            bell_callback: None,
            last_content_origin: None,
            url_hover: None,
            title_callback: None,
            clipboard_store_callback: None,
            copy_notice_callback: None,
            paste_request_callback: None,
            exit_callback: None,
            scroll_callback: None,
            last_scroll: None,
            scroll_report_scheduled: false,
            mouse_down_reported: None,
            last_motion_report: None,
            marked_text: String::new(),
            search: None,
            window_handle: None,
        }
    }

    /// Set a callback to be invoked when the terminal is resized.
    ///
    /// This callback should resize the underlying PTY to match the new dimensions.
    /// The callback receives (cols, rows) as arguments.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with (cols, rows) on resize
    pub fn with_resize_callback(
        mut self,
        callback: impl Fn(usize, usize) + Send + Sync + 'static,
    ) -> Self {
        self.resize_callback = Some(Arc::new(Box::new(callback)));
        self
    }

    /// Set a callback to intercept key events before terminal processing.
    ///
    /// The callback receives the key event and should return `true` to consume
    /// the event (prevent the terminal from processing it), or `false` to allow
    /// normal terminal processing.
    ///
    /// # Arguments
    ///
    /// * `handler` - A function that receives key events and returns whether to consume them
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_key_handler(|event| {
    ///     // Handle Ctrl++ to increase font size
    ///     if event.keystroke.modifiers.control && event.keystroke.key == "+" {
    ///         // Handle the event
    ///         return true; // Consume the event
    ///     }
    ///     false // Let terminal handle it
    /// })
    /// ```
    pub fn with_key_handler(
        mut self,
        handler: impl Fn(&KeyDownEvent) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.key_handler = Some(Arc::new(Box::new(handler)));
        self
    }

    /// 观察实际写入成功的控制键、普通文本与 IME 提交，用于广播输入。
    /// 回调在释放写端锁后执行；预编辑、搜索及 write_input 广播目标不触发。
    pub fn with_input_callback(mut self, callback: impl Fn(&[u8]) + Send + Sync + 'static) -> Self {
        self.input_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal bell is triggered.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// allowing you to play a sound or show a visual indicator.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called when the bell is triggered
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_bell_callback(|window, cx| {
    ///     // Play a sound or flash the screen
    /// })
    /// ```
    pub fn with_bell_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.bell_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal title changes.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// along with the new title string.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with the new title
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_title_callback(|window, cx, title| {
    ///     // Update window title or tab title
    /// })
    /// ```
    pub fn with_title_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.title_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal wants to store data to the clipboard.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// along with the text to store. This is typically triggered by OSC 52 escape sequences.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with the text to store
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_clipboard_store_callback(|window, cx, text| {
    ///     // Store text to system clipboard
    /// })
    /// ```
    pub fn with_clipboard_store_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.clipboard_store_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when a selection copy succeeds.
    ///
    /// 回调在选中即复制路径写完剪贴板后触发，携带写入的字符数，
    /// 供宿主显示「已复制 N 字符」提示。
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_copy_notice_callback(|window, cx, chars| {
    ///     // 弹 toast 提示
    /// })
    /// ```
    pub fn with_copy_notice_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, usize) + 'static,
    ) -> Self {
        self.copy_notice_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the user presses a paste shortcut
    /// (Ctrl+Shift+V / Shift+Insert) inside the terminal view.
    ///
    /// 回调触发后按键不再透传 PTY；搜索打开期间快捷键被搜索拦截，不触发本回调。
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_paste_request_callback(|window, cx| {
    ///     // 读剪贴板 → 多行确认 → 写 PTY
    /// })
    /// ```
    pub fn with_paste_request_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.paste_request_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal process exits.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// allowing you to close the terminal view or show an exit message.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called when the process exits
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_exit_callback(|window, cx| {
    ///     // Close the terminal tab or show exit message
    /// })
    /// ```
    pub fn with_exit_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.exit_callback = Some(Box::new(callback));
        self
    }

    /// Background thread that reads from stdout.
    ///
    /// This function runs in a background thread, continuously reading bytes
    /// from the stdout reader and sending them through the async channel.
    /// The async channel allows the main async task to be woken up immediately
    /// when data arrives (push-based).
    fn read_stdout_blocking<R: Read + Send + 'static>(
        mut stdout_reader: R,
        bytes_tx: flume::Sender<Vec<u8>>,
    ) {
        let mut buffer = [0u8; 4096];

        loop {
            match stdout_reader.read(&mut buffer) {
                Ok(0) => {
                    // EOF - channel will be dropped, signaling completion
                    break;
                }
                Ok(n) => {
                    // Send bytes to the async task
                    let bytes = buffer[..n].to_vec();
                    if bytes_tx.send(bytes).is_err() {
                        break; // Channel closed
                    }
                }
                Err(_) => {
                    // Read error
                    break;
                }
            }
        }
    }

    /// Handle keyboard input events.
    ///
    /// Converts GPUI keystrokes to terminal escape sequences and writes them
    /// to the stdin writer. If a key handler is set and returns true, the event
    /// is consumed and not sent to the terminal.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Check if key handler wants to consume this event
        if let Some(ref handler) = self.key_handler
            && handler(event)
        {
            return; // Event consumed by handler
        }

        let keystroke = &event.keystroke;

        // Ctrl+Shift+F 打开回滚搜索
        if keystroke.modifiers.control
            && keystroke.modifiers.shift
            && !keystroke.modifiers.alt
            && !keystroke.modifiers.platform
            && !keystroke.modifiers.function
            && keystroke.key.eq_ignore_ascii_case("f")
        {
            self.open_search(cx);
            return;
        }

        // 提示符跳转（OSC 133 标记）：Alt+Shift+Up/Down（iTerm2 同款）。
        // 避让既有占用：窗格导航 Ctrl+方向（workspace）、搜索栏 Alt+C/R/W；
        // 无论命中与否都消费按键——未命中标记时也不把组合键透传 PTY，
        // 防止多余的 ESC 序列泄进 shell。
        if keystroke.modifiers.alt
            && keystroke.modifiers.shift
            && !keystroke.modifiers.control
            && !keystroke.modifiers.platform
            && !keystroke.modifiers.function
        {
            match keystroke.key.as_str() {
                "up" => {
                    self.jump_prompt_previous(cx);
                    return;
                }
                "down" => {
                    self.jump_prompt_next(cx);
                    return;
                }
                _ => {}
            }
        }

        // 搜索打开时拦截所有按键，一律不透传 PTY
        if self.search.is_some() {
            match keystroke.key.as_str() {
                "escape" => self.close_search(cx),
                "enter" | "return" => {
                    // Enter 双分支（语义钉死）：
                    // - 有挂起防抖（150ms 窗口内「刚改完查询就按 Enter」）：
                    //   同步提交挂起的搜索（落首命中）并消费本次按键——
                    //   消除「在滞后结果上跳 → 防抖刷新又被重置回首命中」
                    //   的双重定位；
                    // - 无挂起防抖：与历史行为逐字一致——在既有 matches
                    //   上循环跳转，不触发搜索。
                    if self
                        .search
                        .as_ref()
                        .is_some_and(|search| search.search_pending)
                    {
                        // 防抖窗口内先出结果（落首命中）；Shift+Enter 再从
                        // 首命中循环往上（wrap 到末条）——方向语义不丢。
                        self.run_pending_search(cx);
                        if keystroke.modifiers.shift {
                            self.search_select_prev(cx);
                        }
                    } else if keystroke.modifiers.shift {
                        self.search_select_prev(cx);
                    } else {
                        self.search_select_next(cx);
                    }
                }
                "backspace" => self.search_backspace(cx),
                // ↑/↓ 始终在既有（可能滞后的）matches 上跳，随后防抖刷新
                // 重置回首命中——150ms 窗口的瞬态取舍，与 Enter 的 flush
                // 语义（先出结果）不同，刻意不为方向键做同步搜索。
                "up" => self.search_select_prev(cx),
                "down" => self.search_select_next(cx),
                "c" if keystroke.modifiers.alt => {
                    // Alt+C 切换大小写敏感（与 Aa 钮共用路径）
                    self.toggle_search_case(cx);
                }
                "r" if keystroke.modifiers.alt => {
                    // Alt+R 切换正则；开关切换即时刷新（唯一绕过防抖的路径）
                    self.toggle_search_regex(cx);
                }
                // Alt+W 切全字；正则开启时臂卫失败落入 _ 分支吞掉（无效），
                // 与 W 钮置灰所见即所得
                "w" if keystroke.modifiers.alt
                    && !self.search.as_ref().is_some_and(|search| search.regex) =>
                {
                    if let Some(search) = &mut self.search {
                        search.whole_word = !search.whole_word;
                    }
                    self.refresh_search(cx);
                }
                _ => {
                    // 可打印字符走 InputHandler 提交路径追加查询；
                    // 其余组合键（Ctrl+C 等）在搜索态直接吞掉。
                }
            }
            return;
        }

        // 粘贴快捷键（Ctrl+Shift+V / Shift+Insert）：交给宿主回调走
        // 多行确认流程；搜索态在上面分支已被吞（口径同上），不在此触发。
        if paste_shortcut_matches(keystroke) {
            if let Some(callback) = &self.paste_request_callback {
                callback(window, cx);
            }
            return;
        }

        let is_plain_text = !keystroke.modifiers.control
            && !keystroke.modifiers.alt
            && (keystroke.key_char.is_some() || keystroke.key.len() == 1);
        if is_plain_text {
            // Printable text is delivered through GPUI's InputHandler so IME commit
            // text and physical keyboard text share the same path.
            return;
        }

        if let Some(bytes) = keystroke_to_bytes(keystroke, self.state.mode()) {
            self.commit_input(&bytes);
        }
    }

    /// 真实用户输入的统一提交口；先写本端，再在锁外观察，失败不广播。
    fn commit_input(&self, bytes: &[u8]) {
        let result = {
            let mut writer = self.stdin_writer.lock();
            writer.write_all(bytes).and_then(|_| writer.flush())
        };
        match result {
            Ok(()) => {
                if let Some(callback) = &self.input_callback {
                    callback(bytes);
                }
            }
            Err(error) => eprintln!("写入终端输入失败: {error}"),
        }
    }

    /// 会话写端克隆（广播输入用）：把字节写入底层 PTY/SSH 通道。
    pub fn write_input(&self, bytes: &[u8]) {
        let mut writer = self.stdin_writer.lock();
        if let Err(error) = writer.write_all(bytes).and_then(|_| writer.flush()) {
            eprintln!("广播写入失败: {error}");
        }
    }

    /// 把窗口像素坐标换算为终端网格坐标。
    ///
    /// 返回的 `Line` 是 alacritty 网格坐标：可见行数减去 display_offset，
    /// 回滚历史为负行；越界钳制到网格内。
    fn grid_point_at(&self, position: Point<Pixels>) -> Option<AlacPoint> {
        let origin = self.last_content_origin?;
        let (columns, screen_lines, display_offset) = self.state.with_term(|term| {
            let grid = term.grid();
            (
                grid.columns(),
                grid.screen_lines(),
                grid.display_offset() as i32,
            )
        });
        let cell = crate::mouse::pixel_to_cell(
            position,
            origin,
            self.renderer.cell_width,
            self.renderer.cell_height,
        );
        let row = (cell.line.0 as usize).min(screen_lines.saturating_sub(1));
        let column = cell.column.0.min(columns.saturating_sub(1));
        Some(AlacPoint::new(
            Line(row as i32 - display_offset),
            Column(column),
        ))
    }

    /// 鼠标上报统一取点：像素坐标 → 网格点（含 display_offset）→ SGR 视口坐标。
    ///
    /// 网格负行（回滚历史）必须经 alacritty 的 `point_to_viewport` 换算成非负
    /// 视口行再上报（SGR 坐标 1-based，直接对网格点 +1 会产出 `;-1;` 非法序列）；
    /// 换算失败回退 (0,0)。首帧渲染前（无内容原点）返回 `None`，调用方回落
    /// 本地路径，与既有选区行为一致。
    fn mouse_report_point(&self, position: Point<Pixels>) -> Option<AlacPoint> {
        let grid_point = self.grid_point_at(position)?;
        let display_offset = self.state.with_term(|term| term.grid().display_offset());
        Some(
            alacritty_terminal::term::point_to_viewport(display_offset, grid_point)
                .map(|viewport| AlacPoint::new(Line(viewport.line as i32), viewport.column))
                .unwrap_or_else(|| AlacPoint::new(Line(0), Column(0))),
        )
    }

    /// Handle mouse down events.
    ///
    /// 左键负责 URL 打开与文本选区（单击字符 / 双击词 / 三击行），
    /// 右键交给 [`Self::on_right_click`]。
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Request focus when clicking the terminal
        window.focus(&self.focus_handle);
        // 点击后光标立即回到可见相位，避免刚点完就停在熄灭态。
        self.blink_on
            .store(true, std::sync::atomic::Ordering::Relaxed);
        cx.notify();

        // SGR 鼠标上报接管：应用开启 1000/1002/1003 + 1006 且未按 Shift 时，
        // 左键按下按 SGR 1006 编码 write_input 上报（鼠标坐标属单窗格，不走
        // commit_input 广播），本地 URL 打开与选区让位。Shift 绕过是终端惯例；
        // 1000-only 旧编码应用由 mouse_reporting_active 拦下回退本地。
        let mode = self.state.mode();
        if event.button == MouseButton::Left
            && crate::mouse::mouse_reporting_active(mode, event.modifiers.shift)
            && let Some(point) = self.mouse_report_point(event.position)
        {
            let modifiers =
                crate::mouse::encode_modifiers(false, event.modifiers.alt, event.modifiers.control);
            if let Some(bytes) =
                crate::mouse::mouse_button_report(MouseButton::Left, true, point, modifiers, mode)
            {
                self.write_input(&bytes);
                self.mouse_down_reported = Some(MouseButton::Left);
                // 以按下点初始化移动去重：同格的首个 move 不重发。
                self.last_motion_report = Some((point.column.0, point.line.0 as usize, modifiers));
                return;
            }
            // 编码被拒（模式中途翻转等）：回落本地路径。
        }
        // 网格点不可得（首帧渲染前）：回落本地路径。

        // URL 点击：仅 Ctrl+左键才用系统浏览器打开（悬停仅下划线 + 小手光标，
        // 不直接跳，避免误触跳转）。Ctrl+点击命中 URL 时不建立选区。
        if event.modifiers.control
            && let Some(origin) = self.last_content_origin
        {
            let cell = crate::mouse::pixel_to_cell(
                event.position,
                origin,
                self.renderer.cell_width,
                self.renderer.cell_height,
            );
            let row = cell.line.0.max(0) as usize;
            let col = cell.column.0;
            // OSC 8 显式超链接优先，回落 URL 文本自动检测
            if let Some(url) = self.resolve_url_at(row, col) {
                open_url(&url);
                return;
            }
        }

        // 文本选区：单击字符、双击词（semantic）、三击整行。
        // `Term.selection` 存 Selection 构造器本体，渲染/复制时按需换算。
        let Some(point) = self.grid_point_at(event.position) else {
            return;
        };
        let selection_type = match event.click_count {
            0 | 1 => AlacSelectionType::Simple,
            2 => AlacSelectionType::Semantic,
            _ => AlacSelectionType::Lines,
        };
        let selection = AlacSelection::new(selection_type, point, Side::Left);
        self.state
            .with_term_mut(|term| term.selection = Some(selection));
        cx.notify();
    }

    /// Handle mouse up events: 结束拖拽；未成形的单击选区直接清除。
    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 上报期间的 release：与已上报的 press 配对发给应用。take() 立即清理
        // 按下标记；期间不存在本地选区，直接返回。门控与 down/move/scroll
        // 三入口同口径——模式中途翻转（按住期间应用单独发 ?1006l 退到
        // 1000-only）时 release 被拦下，绝不以 SGR 字节发给旧编码应用，
        // 静默丢弃、无悬挂标记、不落本地选区。
        if let Some(button) = self.mouse_down_reported.take() {
            self.last_motion_report = None;
            let mode = self.state.mode();
            if crate::mouse::mouse_reporting_active(mode, event.modifiers.shift)
                && let Some(point) = self.mouse_report_point(event.position)
            {
                let modifiers = crate::mouse::encode_modifiers(
                    false,
                    event.modifiers.alt,
                    event.modifiers.control,
                );
                if let Some(bytes) =
                    crate::mouse::mouse_button_report(button, false, point, modifiers, mode)
                {
                    self.write_input(&bytes);
                }
            }
            return;
        }

        let had_selection = self.state.with_term(|term| term.selection.is_some());
        if !had_selection {
            return;
        }
        self.state.with_term_mut(|term| {
            // to_range 对空选区返回 None（单击未拖拽），据此保留或清除。
            let finalized = term
                .selection
                .take()
                .filter(|selection| selection.to_range(term).is_some());
            term.selection = finalized;
        });
        cx.notify();
        // 选中即复制：只挂在有效选区支路（前两条支路已提前 return——上报接管
        // 期间无本地选区、单击清选区路径无有效选区），finalize 后 has_selection
        // 恰好等价于 to_range 非空；copy_selection 自带空文本保护。
        // 复制成功后再通知宿主（「已复制 N 字符」toast 由宿主决定显隐）。
        if self.config.copy_on_select
            && self.has_selection()
            && let Some(chars) = self.copy_selection(cx)
            && let Some(callback) = &self.copy_notice_callback
        {
            callback(window, cx, chars);
        }
    }

    /// Handle mouse move events: 按住左键拖拽时更新选区终点。
    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 门控必须先于本地按键 early-return：1003 悬停（无按键）也要上报，
        // 否则移动上报是死路。
        let mode = self.state.mode();
        if !crate::mouse::mouse_reporting_active(mode, event.modifiers.shift) {
            // URL 悬停探测（本地路径）：无按键悬停时更新下划线与小手光标。
            if event.pressed_button.is_none() {
                self.update_url_hover(event.position, cx);
            }
            if event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            let Some(point) = self.grid_point_at(event.position) else {
                return;
            };
            self.state.with_term_mut(|term| {
                if let Some(selection) = term.selection.as_mut() {
                    selection.update(point, Side::Right);
                }
            });
            cx.notify();
            return;
        }

        // 上报接管：只编「已上报的左键拖拽(32)」与「无按键悬停(35)」。
        // 右/中键按住时应用从未收到对应 press，编按钮码或 35 都是编造事件，
        // 一律跳过；左键按住但 press 未上报（Shift 起拖后松开 Shift）同理跳过，
        // 保证按钮码与已上报的 press/release 严格配对。
        let pressed = match event.pressed_button {
            None => None,
            Some(MouseButton::Left) if self.mouse_down_reported == Some(MouseButton::Left) => {
                Some(MouseButton::Left)
            }
            Some(_) => return,
        };
        if !crate::mouse::should_report_motion(mode, pressed.is_some()) {
            return;
        }
        let Some(point) = self.mouse_report_point(event.position) else {
            return;
        };
        let modifiers =
            crate::mouse::encode_modifiers(false, event.modifiers.alt, event.modifiers.control);
        // 同格去重：同一 cell 且修饰位未变的连续移动不重发。
        let dedupe_key = (point.column.0, point.line.0 as usize, modifiers);
        if self.last_motion_report == Some(dedupe_key) {
            return;
        }
        if let Some(bytes) = crate::mouse::mouse_motion_report(pressed, point, modifiers, mode) {
            self.write_input(&bytes);
            self.last_motion_report = Some(dedupe_key);
        }
    }

    /// 当前格的 URL 解析统一口径：OSC 8 显式超链接优先（应用声明语义，
    /// 展示文本可以不是 URL 形态），回落 URL 文本自动检测。Ctrl+点击 /
    /// 悬停 / 右键三条入口共用，保证跳转、下划线与右键菜单同一目标。
    ///
    /// 有意不做：超链接段非悬停时常显下划线——需要 layout_row 按超链接
    /// 分 run 渲染（run 碎片化，代价大于收益），悬停态下划线已覆盖发现路径。
    fn resolve_url_at(&self, row: usize, col: usize) -> Option<String> {
        self.state
            .hyperlink_span_at(row, col)
            .or_else(|| self.state.extract_url_at(row, col))
    }

    /// 同 [`Self::resolve_url_at`] 但附带段列范围（悬停下划线绘制用）。
    fn resolve_url_span_at(&self, row: usize, col: usize) -> Option<(String, usize, usize)> {
        self.state
            .hyperlink_span_extent_at(row, col)
            .or_else(|| self.state.extract_url_span_at(row, col))
    }

    /// 更新 URL 悬停状态：命中时记录范围并切小手光标 + 重绘下划线；离开清空。
    fn update_url_hover(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let span = self.last_content_origin.and_then(|origin| {
            let cell = crate::mouse::pixel_to_cell(
                position,
                origin,
                self.renderer.cell_width,
                self.renderer.cell_height,
            );
            let row = cell.line.0.max(0) as usize;
            // OSC 8 显式超链接优先，回落 URL 文本自动检测（悬停下划线
            // 覆盖层与小手光标管道原样复用）
            self.resolve_url_span_at(row, cell.column.0)
                .map(|(url, start, end)| (url, row, start, end))
        });
        // 比较位置元组（String 不参与比较）；同格重复 move 不重绘
        let changed = match (&span, &self.url_hover) {
            (None, None) => false,
            (Some(a), Some(b)) => a.1 != b.1 || a.2 != b.2 || a.3 != b.3,
            _ => true,
        };
        if changed {
            self.url_hover = span;
            cx.notify();
        }
    }

    /// 右键：向宿主发菜单请求（锚点 + 命中的 URL），菜单由工作区渲染。
    fn on_right_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        window.focus(&self.focus_handle);
        let url = self.last_content_origin.and_then(|origin| {
            let cell = crate::mouse::pixel_to_cell(
                event.position,
                origin,
                self.renderer.cell_width,
                self.renderer.cell_height,
            );
            let row = cell.line.0.max(0) as usize;
            // OSC 8 显式超链接优先，回落 URL 文本自动检测
            self.resolve_url_at(row, cell.column.0)
        });
        cx.emit(TerminalMenuRequested {
            position: event.position,
            url,
        });
    }

    /// Handle scroll events.
    ///
    /// Currently a placeholder for future scrollback support.
    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Ctrl+滚轮：字号缩放（终端标配）——不进滚动逻辑
        if event.modifiers.control {
            let delta: f32 = match event.delta {
                ScrollDelta::Lines(point) => point.y,
                ScrollDelta::Pixels(point) => {
                    let py: f32 = point.y.into();
                    py * 0.01
                }
            };
            if delta.abs() < 0.1 {
                return;
            }
            let step = if delta > 0.0 { 1.0 } else { -1.0 };
            let current: f32 = self.config.font_size.into();
            let new_size = (current + step).clamp(8.0, 40.0);
            if (new_size - current).abs() > f32::EPSILON {
                self.config.font_size = gpui::px(new_size);
                self.renderer.font_size = self.config.font_size;
                self.renderer.invalidate_measurement();
                cx.notify();
            }
            return;
        }

        let cell_height: f32 = self.renderer.cell_height.into();
        let delta_y: f32 = match event.delta {
            ScrollDelta::Lines(point) => point.y,
            ScrollDelta::Pixels(point) => {
                if cell_height <= 0.0 {
                    return;
                }
                point.y / gpui::px(cell_height)
            }
        };
        // 当前桌面使用自然滚动：保持滚轮方向与内容移动方向一致。
        // 平滑滚动：小数行残差累积到累计器，够一整行才滚动——触控板/高分辨率
        // 滚轮的微小位移也能连续生效，不丢行也不跳行。
        let lines = if self.config.smooth_scrolling {
            let total = self.scroll_residual + delta_y;
            let whole = total.trunc();
            self.scroll_residual = total - whole;
            whole as i32
        } else {
            self.scroll_residual = 0.0;
            delta_y.round() as i32
        };
        if lines == 0 {
            cx.notify();
            return;
        }

        let mode = self.state.mode();

        // SGR 鼠标模式（1000/1002/1003 + 1006，无 Shift）：滚轮按真实格点
        // 上报（替代旧恒 1;1），平滑滚动残差作废——滚轮已让位给应用。
        if crate::mouse::mouse_reporting_active(mode, event.modifiers.shift)
            && let Some(point) = self.mouse_report_point(event.position)
        {
            let modifiers =
                crate::mouse::encode_modifiers(false, event.modifiers.alt, event.modifiers.control);
            if let Some(bytes) = crate::mouse::scroll_report(lines, point, modifiers, mode) {
                self.scroll_residual = 0.0;
                self.write_input(&bytes);
                cx.notify();
                return;
            }
            // 网格点不可得（首帧渲染前）：回落下方老路（1;1 点位），行为不回归。
        }

        // 老路：1000-only（X10/UTF-8 旧编码）的应用绝不收 SGR 滚轮字节——
        // 剥掉鼠标模式位后交给 scroll_report，让其回落 ALT 平移或本地回滚；
        // SGR 已协商时保留原样（含 Shift 按住时的存量转发行为）。
        let point = alacritty_terminal::index::Point::new(
            alacritty_terminal::index::Line(0),
            alacritty_terminal::index::Column(0),
        );
        let fallback_mode = if mode.contains(alacritty_terminal::term::TermMode::SGR_MOUSE) {
            mode
        } else {
            mode - (alacritty_terminal::term::TermMode::MOUSE_REPORT_CLICK
                | alacritty_terminal::term::TermMode::MOUSE_MOTION
                | alacritty_terminal::term::TermMode::MOUSE_DRAG)
        };
        if let Some(bytes) = crate::mouse::scroll_report(lines, point, 0, fallback_mode) {
            let mut writer = self.stdin_writer.lock();
            if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
                eprintln!("写入滚轮事件失败: {error}");
            }
        } else {
            self.state.with_term_mut(|term| {
                term.scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
            });
        }
        cx.notify();
    }

    /// Process pending terminal events.
    ///
    /// This method drains all available events from the event receiver
    /// and handles them appropriately. Note: bytes are processed in the
    /// async reader task, not here.
    fn process_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Process terminal events (from alacritty event proxy)
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                TerminalEvent::Wakeup => {
                    // Terminal has new content - already handled by async task
                }
                TerminalEvent::Bell => {
                    if let Some(ref callback) = self.bell_callback {
                        callback(window, cx);
                    } else if self.config.bell_notification {
                        // 显式开启才发系统通知（长命令跑完提醒）
                        send_desktop_notification("LaterTerm", "终端 bell 响铃");
                    } else {
                        // 默认视觉提示：请求一帧重绘（闪烁指示由宿主自行强化），不外发系统通知。
                        cx.notify();
                    }
                }
                TerminalEvent::Title(title) => {
                    if let Some(ref callback) = self.title_callback {
                        callback(window, cx, &title);
                    }
                }
                TerminalEvent::ClipboardStore(text) => {
                    if let Some(ref callback) = self.clipboard_store_callback {
                        callback(window, cx, &text);
                    } else {
                        // OSC 52 默认行为：无回调时直接写系统剪贴板
                        let _ = crate::clipboard::Clipboard::new()
                            .and_then(|mut clipboard| clipboard.copy(&text));
                    }
                }
                TerminalEvent::ClipboardLoad => {
                    // OSC 52 读：把系统剪贴板内容写入终端输入（等价粘贴）
                    if let Ok(text) = crate::clipboard::Clipboard::new()
                        .and_then(|mut clipboard| clipboard.paste())
                        && !text.is_empty()
                    {
                        let mut writer = self.stdin_writer.lock();
                        let _ = writer
                            .write_all(text.as_bytes())
                            .and_then(|_| writer.flush());
                    }
                }
                TerminalEvent::Exit => {
                    if let Some(callback) = self.exit_callback.take() {
                        // 兼容旧回调接口，不能在 render 中直接修改父布局。
                        cx.defer_in(window, move |_, window, cx| callback(window, cx));
                    }
                }
                TerminalEvent::PtyWrite(text) => {
                    // 应用侧查询应答（kitty ?u / DECRQM / DA）直接写回本端
                    // PTY。刻意不走 commit_input：后者会触发 input_callback
                    // 把字节广播给其他窗格，协议应答不是用户输入，不该扩散。
                    self.write_input(text.as_bytes());
                }
                TerminalEvent::DesktopNotice { title, body } => {
                    // OSC 9/777 桌面通知：沿用 bell_notification 门控（显式开启
                    // 才外发，默认静默——应用输出不可信，通知频率与 bell 同水位，
                    // v1 不节流；若后续需要可加 1s 合并窗口）。
                    if self.config.bell_notification {
                        let title = if title.is_empty() {
                            "LaterTerm".to_owned()
                        } else {
                            title
                        };
                        send_desktop_notification(&title, &body);
                    }
                }
            }
        }
    }

    /// Get the current terminal dimensions.
    ///
    /// # Returns
    ///
    /// A tuple of (columns, rows).
    pub fn dimensions(&self) -> (usize, usize) {
        (self.state.cols(), self.state.rows())
    }

    /// 按比例设置滚动位置：0.0 = 最老历史，1.0 = 底部。
    /// 用于宿主滚动条点击/拖拽；变化会立即请求终端重绘。
    pub fn set_scroll_fraction(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let fraction = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else if fraction.is_sign_negative() {
            0.0
        } else {
            1.0
        };
        let changed = self.state.with_term_mut(|term| {
            let grid = term.grid();
            let scrollable = grid.total_lines().saturating_sub(grid.screen_lines());
            if scrollable == 0 {
                return false;
            }
            let target = ((1.0 - fraction) * scrollable as f32).round() as i32;
            let current = grid.display_offset() as i32;
            if target == current {
                return false;
            }
            term.scroll_display(alacritty_terminal::grid::Scroll::Delta(target - current));
            true
        });
        self.scroll_residual = 0.0;
        if changed {
            cx.notify();
            self.schedule_scroll_report(cx);
        }
    }

    /// 滚动状态摘要（display_offset / 可见行数 / 总行数），供外部画滚动条。
    pub fn scroll_summary(&self) -> ScrollSummary {
        self.state.with_term(|term| {
            let grid = term.grid();
            ScrollSummary {
                display_offset: grid.display_offset(),
                screen_lines: grid.screen_lines(),
                total_lines: grid.total_lines(),
            }
        })
    }

    /// Set a callback to be invoked when the scroll position or history size changes.
    pub fn with_scroll_callback(
        mut self,
        callback: impl Fn(&mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.scroll_callback = Some(Box::new(callback));
        self
    }

    /// 滚动摘要变化时触发一次 scroll 回调；150ms 防抖合并输出洪峰，
    /// 避免高频输出把宿主（整个工作区）拖进逐批次重渲染。
    fn schedule_scroll_report(&mut self, cx: &mut Context<Self>) {
        if self.scroll_callback.is_none() || self.scroll_report_scheduled {
            return;
        }
        self.scroll_report_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.scroll_report_scheduled = false;
                let summary = this.scroll_summary();
                if this.last_scroll != Some(summary) {
                    this.last_scroll = Some(summary);
                    if let Some(callback) = &this.scroll_callback {
                        callback(cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Resize the terminal to new dimensions.
    ///
    /// This method should be called when the terminal view size changes.
    /// It updates the internal grid and notifies the terminal process of the new size.
    ///
    /// # Arguments
    ///
    /// * `cols` - New number of columns
    /// * `rows` - New number of rows
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        self.state.resize(cols, rows);
        if let Some(callback) = &self.resize_callback {
            callback(cols, rows);
            *self.session_size.lock() = Some((cols, rows));
        }
    }

    /// Send text directly to the PTY, used by trusted quick-command UI actions.
    pub fn send_text(&mut self, text: &str) {
        let mut writer = self.stdin_writer.lock();
        if let Err(error) = writer
            .write_all(text.as_bytes())
            .and_then(|_| writer.flush())
        {
            eprintln!("写入快捷命令失败: {error}");
        }
    }

    /// 打开回滚搜索（Ctrl+Shift+F），终端保持焦点但按键转为搜索输入。
    pub fn open_search(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() {
            self.search = Some(SearchState {
                query: String::new(),
                matches: Vec::new(),
                current: 0,
                case_sensitive: false,
                regex: false,
                whole_word: false,
                regex_error: None,
                search_pending: false,
            });
            cx.notify();
        }
    }

    /// 关闭搜索并恢复终端输入。
    pub fn close_search(&mut self, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            self.marked_text.clear();
            cx.notify();
        }
    }

    /// 是否存在文本选区。
    pub fn has_selection(&self) -> bool {
        self.state.with_term(|term| term.selection.is_some())
    }

    /// 复制选中文本到剪贴板；无选区或空文本时不动作。
    ///
    /// 返回 `Some(字符数)` 表示已写入剪贴板（供宿主弹「已复制 N 字符」提示），
    /// `None` 表示无有效内容。
    pub fn copy_selection(&mut self, cx: &mut Context<Self>) -> Option<usize> {
        let text = self.state.with_term(|term| term.selection_to_string());
        if let Some(text) = text
            && !text.is_empty()
        {
            let chars = text.chars().count();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            Some(chars)
        } else {
            None
        }
    }

    /// 粘贴剪贴板文本到终端（原样写入 PTY，换行由应用自行处理）。
    pub fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
            && !text.is_empty()
        {
            self.write_input(text.as_bytes());
        }
    }

    /// 全选：覆盖可见区与全部回滚历史。
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.state.with_term_mut(|term| {
            let grid = term.grid();
            let mut selection = AlacSelection::new(
                AlacSelectionType::Lines,
                AlacPoint::new(term.topmost_line(), Column(0)),
                Side::Left,
            );
            selection.update(
                AlacPoint::new(
                    Line(grid.screen_lines() as i32 - 1),
                    Column(grid.columns().saturating_sub(1)),
                ),
                Side::Right,
            );
            term.selection = Some(selection);
        });
        cx.notify();
    }

    /// 清屏（等价 shell `clear`：清除可见区，回滚历史保留）。
    pub fn clear_screen(&mut self, cx: &mut Context<Self>) {
        self.state
            .with_term_mut(|term| term.clear_screen(ClearMode::All));
        cx.notify();
    }

    /// 重置终端状态（颜色、样式、终端模式全部恢复默认）。
    pub fn reset_terminal(&mut self, cx: &mut Context<Self>) {
        self.state.with_term_mut(|term| term.reset_state());
        // reset_state 会 grid.reset() 清掉全部历史内容：绝对行号所指内容
        // 不复存在，提示符标记一并作废（与 resize 作废同一失效类别）。
        self.state.invalidate_prompt_marks();
        cx.notify();
    }

    /// 跳到上一条提示符标记（OSC 133；Alt+Shift+Up / 右键菜单）。
    /// 返回是否成跳（无标记 / 备用屏时为 false）。
    pub fn jump_prompt_previous(&mut self, cx: &mut Context<Self>) -> bool {
        let jumped = self
            .state
            .jump_prompt(crate::osc::PromptDirection::Previous);
        if jumped {
            cx.notify();
        }
        jumped
    }

    /// 跳到下一条提示符标记（OSC 133；Alt+Shift+Down / 右键菜单）。
    pub fn jump_prompt_next(&mut self, cx: &mut Context<Self>) -> bool {
        let jumped = self.state.jump_prompt(crate::osc::PromptDirection::Next);
        if jumped {
            cx.notify();
        }
        jumped
    }

    /// 提示符标记快照（滚动淘汰修正后；右键菜单可用性判定等宿主侧消费）。
    pub fn prompt_marks(&self) -> Vec<crate::osc::PromptMark> {
        self.state.prompt_marks()
    }

    /// 最近一条已结束命令是否失败（OSC 133;D exit≠0；标签栏红点数据源）。
    pub fn last_command_failed(&self) -> bool {
        self.state.last_command_failed()
    }

    /// 当前是否有命令运行中（OSC 133;C 与 D 之间窗态；v1 只暴露数据）。
    pub fn command_running(&self) -> bool {
        self.state.command_running()
    }

    /// 当前目录（OSC 7 上报，percent 解码后的绝对路径）。
    ///
    /// 仅本地会话返回：SSH 会话即使远端 shell 上报了 OSC 7，路径也属远端
    /// 文件系统，不能用于本地会话的 cwd 继承——非 Local 一律 None。
    pub fn cwd(&self) -> Option<String> {
        // TODO: SSH 远端 cwd 待 laterterm-ssh 接管后另行暴露（远端路径的
        // 转义/映射语义与本地不同，不塞进本入口）。
        if self.backend != BackendKind::Local {
            return None;
        }
        self.state.cwd().map(str::to_owned)
    }

    /// 最近一次 OSC 9;4 进度上报（ConEmu；state=0 移除后为 None）。
    /// v1 仅暴露数据——进度 UI 暂不呈现（设计阶段随 osc133 角标统一决定）。
    pub fn progress(&self) -> Option<crate::osc::ProgressState> {
        self.state.progress()
    }

    /// 搜索查询追加分本（来自 InputHandler 提交或按键）。
    fn search_push_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.query.push_str(text);
            self.schedule_search(cx);
        }
    }

    /// 删除查询的最后一个字符。
    fn search_backspace(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search
            && search.query.pop().is_some()
        {
            self.schedule_search(cx);
        }
    }

    /// 查询变化后的 150ms 防抖搜索（先例 schedule_scroll_report）：合并
    /// 快速连击，洪峰期只跑最后一次。挂起期间按 Enter 会同步提交（见
    /// on_key_down 的 Enter 双分支），届时本任务醒来发现无挂起直接丢弃。
    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        if self
            .search
            .as_ref()
            .is_some_and(|search| search.search_pending)
        {
            return;
        }
        if let Some(search) = &mut self.search {
            search.search_pending = true;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |this, cx| {
                // 防抖期间搜索已关闭、或已被 Enter 同步提交：直接丢弃
                if this
                    .search
                    .as_ref()
                    .is_some_and(|search| search.search_pending)
                {
                    this.run_pending_search(cx);
                }
            });
        })
        .detach();
    }

    /// 执行挂起的搜索（防抖任务与 Enter 提交共用）。
    ///
    /// 完整 refresh 语义：更新 matches、current=0、滚到首命中；非法正则
    /// 只写 regex_error（红字内联报错），上次有效结果保留不动不滚动。
    fn run_pending_search(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.search_pending = false;
            let query = search.query.clone();
            let options = crate::terminal::SearchOptions {
                case_sensitive: search.case_sensitive,
                regex: search.regex,
                whole_word: search.whole_word,
            };
            match self.state.search_with_options(&query, options) {
                Ok(matches) => {
                    search.matches = matches;
                    search.current = 0;
                    search.regex_error = None;
                    self.scroll_to_current_match();
                }
                Err(message) => {
                    // 非法正则：内联报错，matches/current 不动
                    search.regex_error = Some(message);
                }
            }
        }
        cx.notify();
    }

    /// 立即按当前查询与选项执行搜索（Alt+C/R/W 开关切换路径，唯一绕过
    /// 防抖的入口）：清掉挂起标记后走 run_pending_search 完整语义。
    fn refresh_search(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.search_pending = false;
        }
        self.run_pending_search(cx);
    }

    /// Alt+R / .* 钮：切换正则模式；开启瞬间关闭全字（互斥语义：
    /// 正则用户自行写 \b），并清掉上一次的非法正则报错。
    fn toggle_search_regex(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.regex = !search.regex;
            if search.regex {
                search.whole_word = false;
            }
            search.regex_error = None;
        }
        self.refresh_search(cx);
    }

    /// Alt+C / Aa 钮：切换大小写敏感。
    fn toggle_search_case(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.case_sensitive = !search.case_sensitive;
        }
        self.refresh_search(cx);
    }

    /// Alt+W / W 钮：切换全字匹配。正则开启时整段无效（键臂卫 + 按钮置灰
    /// 同一语义），不做任何动作也不触发搜索。
    fn toggle_search_whole_word(&mut self, cx: &mut Context<Self>) {
        if self.search.as_ref().is_some_and(|search| search.regex) {
            return;
        }
        if let Some(search) = &mut self.search {
            search.whole_word = !search.whole_word;
        }
        self.refresh_search(cx);
    }

    /// 跳到下一个命中（回环）。
    fn search_select_next(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search
            && !search.matches.is_empty()
        {
            search.current = (search.current + 1) % search.matches.len();
            self.scroll_to_current_match();
            cx.notify();
        }
    }

    /// 跳到上一个命中（回环）。
    fn search_select_prev(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search
            && !search.matches.is_empty()
        {
            search.current = (search.current + search.matches.len() - 1) % search.matches.len();
            self.scroll_to_current_match();
            cx.notify();
        }
    }

    /// 滚动网格使当前命中大致落在视口上部三分之一处。
    fn scroll_to_current_match(&mut self) {
        let Some(match_) = self
            .search
            .as_ref()
            .and_then(|search| search.matches.get(search.current).copied())
        else {
            return;
        };
        self.state.with_term_mut(|term| {
            let grid = term.grid();
            let history = grid.total_lines() - grid.screen_lines();
            let desired = history as i64 + (grid.screen_lines() as i64 / 3) - match_.line as i64;
            let desired = desired.clamp(0, history as i64) as i32;
            let delta = desired - grid.display_offset() as i32;
            if delta != 0 {
                term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
            }
        });
    }

    /// Get the current terminal configuration.
    ///
    /// # Returns
    ///
    /// A reference to the current configuration.
    pub fn config(&self) -> &TerminalConfig {
        &self.config
    }

    /// Get the focus handle for this terminal view.
    ///
    /// # Returns
    ///
    /// A reference to the focus handle.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Update the terminal configuration.
    ///
    /// This method updates the terminal's configuration, including font settings,
    /// padding, and color palette. Changes take effect on the next render.
    ///
    /// # Arguments
    ///
    /// * `config` - The new configuration to apply
    /// * `cx` - The context for triggering a repaint
    pub fn update_config(&mut self, config: TerminalConfig, cx: &mut Context<Self>) {
        // Update the terminal grid and PTY when dimensions change.
        if config.cols != self.config.cols || config.rows != self.config.rows {
            self.resize(config.cols, config.rows);
        }
        if config.scrollback != self.config.scrollback {
            self.state.set_scrollback(config.scrollback);
        }
        if config.kitty_keyboard != self.config.kitty_keyboard {
            // kitty 协议开关运行期切换：Term::set_options 会清空协议模式栈
            // 并摘除 KITTY_KEYBOARD_PROTOCOL 位（alacritty 权威语义）
            self.state.update_term_config(config.kitty_keyboard);
        }
        // 光标闪烁开关变化时立即恢复常显，避免停留在熄灭相位。
        if config.cursor_blink != self.config.cursor_blink && config.cursor_blink {
            self.blink_on
                .store(true, std::sync::atomic::Ordering::Relaxed);
            cx.notify();
        }

        if !config.smooth_scrolling {
            self.scroll_residual = 0.0;
        }
        let font_changed = self.renderer.font_family != config.font_family
            || self.renderer.font_size != config.font_size
            || self.renderer.line_height_multiplier != config.line_height_multiplier
            || self.renderer.font_fallbacks != config.font_fallbacks;
        self.renderer.font_family = config.font_family.clone();
        self.renderer.font_size = config.font_size;
        self.renderer.line_height_multiplier = config.line_height_multiplier.max(0.1);
        self.renderer.palette = config.colors.clone();
        // 连字不影响单元格度量，无需重新测量
        self.renderer.ligatures = config.ligatures;
        self.renderer.font_fallbacks = config.font_fallbacks.clone();
        // 光标形状只改绘制几何，同样无需重新测量
        self.renderer.cursor_style = config.cursor_style;
        // 背景不透明度同样不影响单元格度量，不参与 font_changed 重测判定；
        // 钳制与配置层 sanitize（0.15~1.0）同口径
        self.renderer.background_opacity = config.background_opacity.clamp(0.15, 1.0);
        if font_changed {
            self.renderer.invalidate_measurement();
            self.font_trace_changed = true;
        }

        // Store the new config
        self.config = config;

        // Trigger a repaint - cell dimensions will be recalculated via measure_cell()
        cx.notify();
    }

    /// Calculate terminal dimensions from pixel bounds and cell size.
    ///
    /// Helper method to determine how many columns and rows fit in the given bounds.
    #[allow(dead_code)]
    fn calculate_dimensions(&self, bounds: Bounds<Pixels>) -> (usize, usize) {
        let width_f32: f32 = bounds.size.width.into();
        let height_f32: f32 = bounds.size.height.into();
        let cell_width_f32: f32 = self.renderer.cell_width.into();
        let cell_height_f32: f32 = self.renderer.cell_height.into();

        let cols = ((width_f32 / cell_width_f32) as usize).max(1);
        let rows = ((height_f32 / cell_height_f32) as usize).max(1);
        (cols, rows)
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let utf16: Vec<u16> = self.marked_text.encode_utf16().collect();
        if range.start <= range.end && range.end <= utf16.len() {
            adjusted_range.replace(range.clone());
            String::from_utf16(&utf16[range]).ok()
        } else {
            Some(String::new())
        }
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let cursor = self.marked_text.encode_utf16().count();
        Some(UTF16Selection {
            range: cursor..cursor,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        if self.marked_text.is_empty() {
            None
        } else {
            Some(0..self.marked_text.encode_utf16().count())
        }
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked_text.clear();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 搜索打开时输入提交文本转为搜索查询（IME 提交与物理键盘共用此路径）
        if self.search.is_some() {
            self.search_push_text(text, cx);
            return;
        }
        self.marked_text.clear();
        self.commit_input(text.as_bytes());
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text.clear();
        self.marked_text.push_str(new_text);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 锚定光标单元格：输入法候选框跟随光标，而不是停在视图左上角
        let (row, col) = self.state.ime_anchor();
        let x = element_bounds.origin.x
            + self.config.padding.left
            + self.renderer.cell_width * col as f32;
        let y = element_bounds.origin.y
            + self.config.padding.top
            + self.renderer.cell_height * row as f32;
        Some(Bounds {
            origin: Point { x, y },
            size: Size {
                width: self.renderer.cell_width,
                height: self.renderer.cell_height,
            },
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(0)
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 记录所属窗口句柄: PTY 数据任务依赖它把重绘绑定到本窗口。
        if self.window_handle.is_none() {
            self.window_handle = Some(window.window_handle());
        }
        // 本帧已渲染到当前数据版本
        self.rendered_epoch = self.state.data_epoch();
        // Process any pending events
        self.process_events(window, cx);
        // 滚动位置/历史行数变化时通知宿主（防抖定时器，不在 render 期直接 notify）
        self.schedule_scroll_report(cx);

        // Get terminal state and renderer for rendering
        let state_arc = self.state.term_arc();
        let needs_measurement = self.renderer.needs_measurement();
        let trace = font_trace_reason(
            font_trace_enabled(),
            needs_measurement,
            self.font_trace_changed,
        )
        .map(|reason| {
            (
                reason,
                Instant::now(),
                cx.entity_id(),
                window.scale_factor(),
            )
        });
        if needs_measurement {
            self.renderer.measure_cell(window);
            self.font_trace_changed = false;
        }
        let trace = trace.map(|(reason, started, entity_id, scale)| {
            (reason, started, entity_id, scale, started.elapsed())
        });
        // metrics 已在实体 render 阶段测量完成；canvas 只捕获一次不可变快照，
        // 避免每个标签切换/每帧再 clone 后重复测量。
        let renderer = self.renderer.clone();
        let resize_callback = self.resize_callback.clone();
        let session_size = self.session_size.clone();
        // 快照缓冲池句柄：canvas 闭包帧尾归还、下帧复用（见 snapshot_cache 字段注记）
        let snapshot_cache = self.snapshot_cache.clone();
        let focus_handle = self.focus_handle.clone();
        // 光标可见性快照：canvas 闭包在绘制阶段读取（闪烁相位由 blink 任务驱动）
        let cursor_visible =
            !self.config.cursor_blink || self.blink_on.load(std::sync::atomic::Ordering::Relaxed);
        let entity = cx.entity();
        let scroll_offset = if self.config.smooth_scrolling {
            self.scroll_residual
        } else {
            0.0
        };
        let padding = self.config.padding;
        // 搜索快照：canvas 闭包在绘制阶段执行，不能借用 self
        let search_open = self.search.is_some();
        let current_match = self
            .search
            .as_ref()
            .and_then(|search| search.matches.get(search.current).copied());
        // IME 预编辑快照（同上，绘制阶段读取）
        let marked_text = self.marked_text.clone();
        // URL 悬停快照（canvas 闭包绘制下划线用）
        let url_hover = self.url_hover.clone();
        let search_bar = self.search.as_ref().map(|search| {
            let counter_text = search_counter_text(search);
            // 非法正则时计数槽位换红字报错（0xF7768E = 主题 red）
            let counter_color = if search.regex_error.is_some() {
                rgb(0xF7768E)
            } else {
                rgb(0xE0AF68)
            };
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(30.0))
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .bg(gpui::black().opacity(0.8))
                .text_size(px(12.0))
                .text_color(rgb(0xC0CAF5))
                .child(div().text_color(rgb(0x7DCFFF)).child("搜索"))
                .child(div().child(format!("{}▏", search.query)))
                // Aa / .* / W 三开关钮 + ▲▼ 跳转钮（24px 方钮，见
                // search_bar_button：点击 stop_propagation 不建选区）
                .child(search_bar_button(
                    cx,
                    "Aa",
                    search.case_sensitive,
                    false,
                    |this, cx| this.toggle_search_case(cx),
                ))
                .child(search_bar_button(
                    cx,
                    ".*",
                    search.regex,
                    false,
                    |this, cx| this.toggle_search_regex(cx),
                ))
                .child(search_bar_button(
                    cx,
                    "W",
                    search.whole_word,
                    search.regex, // 正则开启时全字置灰（互斥语义）
                    |this, cx| this.toggle_search_whole_word(cx),
                ))
                .child(search_bar_button(cx, "▲", false, false, |this, cx| {
                    this.search_select_prev(cx)
                }))
                .child(search_bar_button(cx, "▼", false, false, |this, cx| {
                    this.search_select_next(cx)
                }))
                .child(
                    div()
                        .ml_auto()
                        .text_color(counter_color)
                        .child(counter_text),
                )
                .child(
                    div()
                        .text_color(rgb(0x565A78))
                        .child("Enter↓ Shift+Enter↑ Alt+C 大小写 Alt+R 正则 Alt+W 全字 Esc 关闭"),
                )
        });

        // 透明模式（background_opacity<1）该 filler 改画全透明：它被下方
        // canvas 满覆盖，保留不透明色会把窗口透明像素盖死（alpha 承载单点
        // 在宿主层，如 LaterTerm 工作区根容器的主题 background 原色@o）。
        let root_bg: gpui::Fill = if self.config.background_opacity < 1.0 {
            gpui::transparent_black().into()
        } else {
            rgb(0x1e1e1e).into()
        };
        let root = div()
            .size_full()
            .bg(root_bg)
            .track_focus(&self.focus_handle)
            // 悬停 URL 时小手光标（命中才有下划线与跳转语义，否则保持箭头）
            .when(url_hover.is_some(), |root| {
                root.cursor(gpui::CursorStyle::PointingHand)
            })
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_click))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, _window, _cx| bounds,
                    move |bounds, _, window, cx| {
                        let rt = render_trace_enabled();
                        let rt_closure_started = rt.then(Instant::now);
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                        use alacritty_terminal::grid::Dimensions;

                        // 测量已在上面的实体 render 阶段完成；闭包是 move+FnOnce，
                        // 直接使用 move 进来的 renderer。注意：上方 `let renderer =
                        // self.renderer.clone()`（font_family String + font_fallbacks
                        // Vec + palette ~4.4KB 白拷贝）本体仍在每帧路径——量级 µs 级
                        // 非瓶颈，独立复核 2026-11-07 确认；如需消除可改 Arc 共享。

                        // Calculate available space after padding, clamping tiny windows safely.
                        let available_width: f32 =
                            (bounds.size.width - padding.left - padding.right).into();
                        let available_height: f32 =
                            (bounds.size.height - padding.top - padding.bottom).into();
                        let cell_width_f32: f32 = renderer.cell_width.into();
                        let cell_height_f32: f32 = renderer.cell_height.into();

                        let cols =
                            ((available_width.max(0.0) / cell_width_f32.max(1.0)) as usize).max(1);
                        let rows = ((available_height.max(0.0) / cell_height_f32.max(1.0))
                            as usize)
                            .max(1);

                        // Helper struct implementing Dimensions for resize
                        struct TermSize {
                            cols: usize,
                            rows: usize,
                        }
                        impl Dimensions for TermSize {
                            fn total_lines(&self) -> usize {
                                self.rows
                            }
                            fn screen_lines(&self) -> usize {
                                self.rows
                            }
                            fn columns(&self) -> usize {
                                self.cols
                            }
                            fn last_column(&self) -> alacritty_terminal::index::Column {
                                alacritty_terminal::index::Column(self.cols.saturating_sub(1))
                            }
                            fn bottommost_line(&self) -> alacritty_terminal::index::Line {
                                alacritty_terminal::index::Line(self.rows as i32 - 1)
                            }
                            fn topmost_line(&self) -> alacritty_terminal::index::Line {
                                alacritty_terminal::index::Line(0)
                            }
                        }

                        // Resize the grid first, then notify the PTY without holding its lock.
                        let resize_lock_started = trace.map(|_| Instant::now());
                        let (needs_resize, old_cols, old_rows, resize_lock_wait, resize_elapsed) = {
                            let mut term = state_arc.lock();
                            let resize_lock_wait = resize_lock_started.map(|started| started.elapsed());
                            let current_cols = term.columns();
                            let current_rows = term.screen_lines();
                            if cols != current_cols || rows != current_rows {
                                let resize_started = trace.map(|_| Instant::now());
                                term.resize(TermSize { cols, rows });
                                (
                                    true,
                                    current_cols,
                                    current_rows,
                                    resize_lock_wait,
                                    resize_started.map(|started| started.elapsed()),
                                )
                            } else {
                                (false, current_cols, current_rows, resize_lock_wait, None)
                            }
                        };
                        let callback_elapsed = if needs_resize || *session_size.lock() != Some((cols, rows)) {
                            if let Some(ref callback) = resize_callback {
                                let callback_started = trace.map(|_| Instant::now());
                                callback(cols, rows);
                                *session_size.lock() = Some((cols, rows));
                                callback_started.map(|started| started.elapsed())
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        // 唯一一次持锁：捕获同帧可见网格快照（cells/selection/cursor/
                        // display_offset 同帧一致），随取随放——锁窗口从「整段绘制」缩到
                        // 「快照复制」，为未来的第二线程消费者（后台快照/诊断抓帧）留路。
                        // 快照缓冲自缓冲池取用（复用行容量，稳态帧零分配），帧尾归还。
                        let mut frame = snapshot_cache.lock().take().unwrap_or_default();
                        let paint_lock_started = trace.map(|_| Instant::now());
                        let rt_lock_started = rt.then(Instant::now);
                        let paint_lock_wait;
                        let rt_lock_wait;
                        let rt_lock_hold;
                        let rt_snapshot;
                        let history_lines;
                        let rt_cells;
                        {
                            let term = state_arc.lock();
                            paint_lock_wait = paint_lock_started.map(|started| started.elapsed());
                            rt_lock_wait = rt_lock_started.map(|started| started.elapsed());
                            // 锁获取成功：paint_lock_hold 计时起点（口径见 render_trace_enabled 注记）
                            let rt_hold_started = rt.then(Instant::now);
                            let rt_capture_started = rt.then(Instant::now);
                            FrameSnapshot::capture_into(&term, &mut frame);
                            // guard 释放前一刻：hold 终点（本实现锁窗口＝快照复制本体）
                            rt_lock_hold = rt_hold_started.map(|started| started.elapsed());
                            rt_snapshot = rt_capture_started.map(|started| started.elapsed());
                            history_lines = trace.map(|_| frame.total_lines - frame.screen_lines);
                            rt_cells = rt.then(|| frame.screen_lines * frame.columns);
                        }

                        // Paint the terminal with measured dimensions（锁外：数据源=快照）
                        let paint_started = trace.map(|_| Instant::now());
                        let rt_paint_started = rt.then(Instant::now);
                        // 记录内容原点（URL 点击检测用：padding 内侧的字符区起点；无需锁）
                        entity.update(cx, |view: &mut Self, _| {
                            view.last_content_origin = Some(Point {
                                x: bounds.origin.x + padding.left,
                                y: bounds.origin.y + padding.top,
                            });
                            if needs_resize {
                                // canvas 直连 Term::resize 的第二条 resize 路径：
                                // 列变化 reflow 重排历史行、绝对行号整体失效，
                                // 提示符标记整体作废（与 TerminalState::resize
                                // 同一决策，见 crate::osc 模块注记）
                                view.state.invalidate_prompt_marks();
                            }
                        });
                        renderer.paint_frame(
                            bounds,
                            padding,
                            &frame,
                            window,
                            cx,
                            cursor_visible,
                            scroll_offset,
                        );
                        let paint_elapsed = paint_started.map(|started| started.elapsed());
                        let rt_paint_elapsed = rt_paint_started.map(|started| started.elapsed());
                        let rt_overlay_started = rt.then(Instant::now);

                        // URL 悬停下划线：网格行/列范围 → 格底 1px 线（accent 色），随滚动移动
                        if let Some((_, row, start_col, end_col)) = &url_hover {
                            let origin_x = bounds.origin.x + padding.left;
                            let origin_y = bounds.origin.y + padding.top;
                            let x = origin_x + renderer.cell_width * (*start_col as f32);
                            let y = origin_y
                                + renderer.cell_height * (*row as f32 + 1.0)
                                - px(1.0);
                            let width = renderer.cell_width * ((end_col - start_col) as f32);
                            window.paint_quad(gpui::quad(
                                Bounds {
                                    origin: Point { x, y },
                                    size: Size {
                                        width,
                                        height: px(1.0),
                                    },
                                },
                                px(0.0),
                                gpui::Hsla::from(gpui::rgb(0x7AA2F7)).opacity(0.9),
                                Edges::<Pixels>::default(),
                                gpui::transparent_black(),
                                gpui::BorderStyle::default(),
                            ));
                        }

                        let trace_report = trace.map(|(reason, started, entity_id, scale, measure_elapsed)| {
                            (reason, entity_id, scale, measure_elapsed, started.elapsed())
                        });

                        // 当前搜索命中的高亮覆盖层（网格坐标锚定，随滚动移动；数据源=快照）
                        if search_open
                            && let Some(match_) = current_match
                            && let Some(rect) = search_match_rect(
                                match_.line,
                                match_.column,
                                match_.len,
                                frame.display_offset,
                                frame.total_lines - frame.screen_lines,
                                frame.screen_lines,
                                Point {
                                    x: bounds.origin.x + padding.left,
                                    y: bounds.origin.y + padding.top,
                                },
                                renderer.cell_width,
                                renderer.cell_height,
                            )
                        {
                            window.paint_quad(gpui::quad(
                                rect,
                                px(0.0),
                                gpui::Hsla::from(gpui::rgb(0xE0AF68)).opacity(0.35),
                                Edges::<Pixels>::default(),
                                gpui::transparent_black(),
                                gpui::BorderStyle::default(),
                            ));
                        }
                        // 文本选区高亮：逐行画选区段覆盖（数据源=快照，与 cells 同帧）
                        if let Some(selection) = frame.selection {
                            let origin_x = bounds.origin.x + padding.left;
                            let origin_y = bounds.origin.y + padding.top;
                            for (screen_row, start_col, end_col) in selection_row_spans(
                                selection,
                                frame.display_offset,
                                frame.columns,
                                frame.screen_lines,
                            ) {
                                let x = origin_x + renderer.cell_width * start_col as f32;
                                let y = origin_y + renderer.cell_height * screen_row as f32;
                                window.paint_quad(gpui::quad(
                                    Bounds {
                                        origin: Point { x, y },
                                        size: Size {
                                            width: renderer.cell_width
                                                * (end_col - start_col) as f32,
                                            height: renderer.cell_height,
                                        },
                                    },
                                    px(0.0),
                                    gpui::Hsla::from(gpui::rgb(0x7AA2F7)).opacity(0.30),
                                    Edges::<Pixels>::default(),
                                    gpui::transparent_black(),
                                    gpui::BorderStyle::default(),
                                ));
                            }
                        }
                        // IME 预编辑浮层：光标处起排、带下划线，画在网格与搜索层之上
                        if !marked_text.is_empty() {
                            let origin = Point {
                                x: bounds.origin.x + padding.left,
                                y: bounds.origin.y + padding.top,
                            };
                            renderer.paint_preedit_frame(
                                &frame,
                                origin,
                                window,
                                cx,
                                &marked_text,
                                scroll_offset,
                            );
                        }
                        let rt_overlay_elapsed = rt_overlay_started.map(|started| started.elapsed());
                        // 快照块内 guard 已释放；帧尾把快照归还缓冲池（下帧复用行容量），
                        // 诊断 I/O 不占任何锁、也不持快照。
                        *snapshot_cache.lock() = Some(frame);
                        if rt {
                            // 诊断输出在锁与快照释放之后：I/O 不进锁窗口（口径见 render_trace_enabled 注记）
                            let as_ms = |duration: Option<Duration>| {
                                duration.map_or(f64::NAN, |value| value.as_secs_f64() * 1000.0)
                            };
                            let closure_ms = rt_closure_started
                                .map(|started| started.elapsed().as_secs_f64() * 1000.0)
                                .unwrap_or(f64::NAN);
                            eprintln!(
                                "[render-trace] grid={cols}x{rows} cells={} paint_lock_wait_ms={:.3} paint_lock_hold_ms={:.3} snapshot_ms={:.3} paint_ms={:.3} overlay_ms={:.3} paint_closure_ms={:.3}",
                                rt_cells.unwrap_or_default(),
                                as_ms(rt_lock_wait),
                                as_ms(rt_lock_hold),
                                as_ms(rt_snapshot),
                                as_ms(rt_paint_elapsed),
                                as_ms(rt_overlay_elapsed),
                                closure_ms,
                            );
                        }
                        if let Some((reason, entity_id, scale, measure_elapsed, total_elapsed)) = trace_report {
                            let as_ms = |duration: Option<Duration>| {
                                duration.map(|value| value.as_secs_f64() * 1000.0)
                            };
                            eprintln!(
                                "[font-trace] phase=termview entity={entity_id} reason={reason} scale={scale:.2} old_grid={old_cols}x{old_rows} grid={cols}x{rows} history_lines={} cell_px={:.2}x{:.2} measure_ms={:.3} resize_lock_ms={:?} resize_ms={:?} callback_ms={:?} paint_lock_ms={:?} paint_ms={:?} render_to_paint_ms={:.3}",
                                history_lines.unwrap_or_default(),
                                cell_width_f32,
                                cell_height_f32,
                                measure_elapsed.as_secs_f64() * 1000.0,
                                as_ms(resize_lock_wait),
                                as_ms(resize_elapsed),
                                as_ms(callback_elapsed),
                                as_ms(paint_lock_wait),
                                as_ms(paint_elapsed),
                                total_elapsed.as_secs_f64() * 1000.0,
                            );
                        }
                    },
                )
                .size_full(),
            );
        if let Some(bar) = search_bar {
            root.child(bar)
        } else {
            root
        }
    }
}

/// 用系统默认浏览器打开 URL。
/// 用系统默认浏览器打开 URL（跨平台：open / cmd start / xdg-open）。
pub fn open_url(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/c", "start", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(program).args(args).spawn();
}

/// 发送系统桌面通知（终端 bell → 通知，长命令跑完提醒）。
///
/// 触发器（audit.rs 泵）与 OSC 9/777 桌面通知复用同一出口：spawn 后台进程
/// 即返回，不阻塞调用线程。文本属不可信输入：先截断再按目标解释器的引号
/// 语法转义（osascript 双引号串 / PowerShell 单引号串），防参数注入。
pub fn send_desktop_notification(title: &str, body: &str) {
    let title = truncate_notification_text(title);
    let body = truncate_notification_text(body);
    let (program, args): (&str, Vec<String>) = if cfg!(target_os = "macos") {
        // AppleScript 双引号字符串：\ 与 " 逐字转义（先 \ 后 "，顺序不能反）
        let title = escape_applescript_string(&title);
        let body = escape_applescript_string(&body);
        (
            "osascript",
            vec![
                "-e".to_string(),
                format!("display notification \"{body}\" with title \"{title}\""),
            ],
        )
    } else if cfg!(target_os = "windows") {
        // PowerShell 单引号字符串：' 翻倍（''）——漏转义可被
        // `'); 命令; ('` 逃逸成任意命令执行
        let title = title.replace('\'', "''");
        let body = body.replace('\'', "''");
        (
            "powershell",
            vec![
                "-c".to_string(),
                format!("New-BurnToastNotification -Text '{title}','{body}' 2>$null"),
            ],
        )
    } else {
        // notify-send 走参数数组不经 shell，引号无特殊语义，无需转义
        (
            "notify-send",
            vec![
                "-a".to_string(),
                title.to_string(),
                title.to_string(),
                body.to_string(),
            ],
        )
    };
    let _ = std::process::Command::new(program).args(args).spawn();
}

/// 通知文本截断 + 换行拍平（128 字符上限：桌面通知是单行短文案，
/// OSC 9/777 的正文是应用输出，长度不受我方控制）。
fn truncate_notification_text(text: &str) -> String {
    const MAX_CHARS: usize = 128;
    text.chars()
        .take(MAX_CHARS)
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect()
}

/// AppleScript 双引号字符串转义：`\` → `\\`、`"` → `\"`（必须先转义
/// 反斜杠，否则会先给转义符再加引号产生 `\"` 歧义序列）。
fn escape_applescript_string(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 粘贴快捷键判定：Ctrl+Shift+V 或 Shift+Insert。
///
/// 纯函数无视图状态，供 `on_key_down` 在搜索拦截分支之后匹配；
/// 命中后按键交宿主 [`PasteRequestCallback`] 处理，不再透传 PTY。
pub fn paste_shortcut_matches(keystroke: &Keystroke) -> bool {
    let m = &keystroke.modifiers;
    if m.alt || m.platform || m.function {
        return false;
    }
    // Ctrl+Shift+V：三个修饰位全亮才认（避免 Ctrl+V 0x16 透传终端惯例被误伤）
    if m.control && m.shift && keystroke.key.eq_ignore_ascii_case("v") {
        return true;
    }
    // Shift+Insert：笔记本无菜单键场景的主粘贴键；Ctrl/Alt 附加时不认
    !m.control && m.shift && keystroke.key == "insert"
}

#[cfg(test)]
mod tests {
    use super::{TerminalConfig, TerminalExited, TerminalSession, TerminalView};
    use alacritty_terminal::grid::Dimensions;
    use gpui::{AppContext, MouseButton, MouseDownEvent, TestAppContext};
    use laterterm_core::{BackendKind, SessionStreams};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FinishedSession;
    impl TerminalSession for FinishedSession {
        fn backend(&self) -> BackendKind {
            BackendKind::Local
        }
        fn title(&self) -> String {
            String::new()
        }
        fn take_streams(&self) -> std::io::Result<SessionStreams> {
            Ok(SessionStreams {
                reader: Box::new(std::io::empty()),
                writer: Box::new(std::io::sink()),
            })
        }
        fn resize(&self, _: u16, _: u16) -> std::io::Result<()> {
            Ok(())
        }
        fn kill(&self) {}
    }

    /// SSH 后端测试替身：cwd 继承的本地/远端判别依赖 backend() 定格。
    struct SshSession;
    impl TerminalSession for SshSession {
        fn backend(&self) -> BackendKind {
            BackendKind::Ssh
        }
        fn title(&self) -> String {
            String::new()
        }
        fn take_streams(&self) -> std::io::Result<SessionStreams> {
            Ok(SessionStreams {
                reader: Box::new(std::io::empty()),
                writer: Box::new(std::io::sink()),
            })
        }
        fn resize(&self, _: u16, _: u16) -> std::io::Result<()> {
            Ok(())
        }
        fn kill(&self) {}
    }

    /// 搜索态按键直驱（与 on_right_click 直调先例同口径）。
    fn press_key(
        terminal: &mut TerminalView,
        key: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<TerminalView>,
    ) {
        terminal.on_key_down(
            &gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse(key).unwrap(),
                is_held: false,
            },
            window,
            cx,
        );
    }

    #[test]
    fn font_trace_is_opt_in_and_only_covers_remeasurement() {
        use super::{font_trace_reason, font_trace_requested};
        use std::ffi::OsStr;

        assert!(!font_trace_requested(None));
        assert!(!font_trace_requested(Some(OsStr::new("0"))));
        assert!(!font_trace_requested(Some(OsStr::new("true"))));
        assert!(font_trace_requested(Some(OsStr::new("1"))));
        assert_eq!(font_trace_reason(false, true, true), None);
        assert_eq!(font_trace_reason(true, false, false), None);
        assert_eq!(font_trace_reason(true, false, true), None);
        assert_eq!(font_trace_reason(true, true, false), Some("initial"));
        assert_eq!(font_trace_reason(true, true, true), Some("font_change"));
    }

    #[gpui::test]
    fn first_layout_synchronizes_session_even_if_grid_size_matches(cx: &mut TestAppContext) {
        use std::sync::Mutex;
        struct RecordingSession(Arc<Mutex<Vec<(u16, u16)>>>);
        impl TerminalSession for RecordingSession {
            fn backend(&self) -> BackendKind {
                BackendKind::Local
            }
            fn title(&self) -> String {
                String::new()
            }
            fn take_streams(&self) -> std::io::Result<SessionStreams> {
                Ok(SessionStreams {
                    reader: Box::new(std::io::empty()),
                    writer: Box::new(std::io::sink()),
                })
            }
            fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
                self.0.lock().unwrap().push((cols, rows));
                Ok(())
            }
            fn kill(&self) {}
        }
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let captured = sizes.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(
                Arc::new(RecordingSession(captured)),
                TerminalConfig::default(),
                cx,
            )
        });
        cx.run_until_parked();
        let grid = terminal.read_with(cx, |view, _| {
            view.state.with_term(|term| {
                use alacritty_terminal::grid::Dimensions;
                (term.columns() as u16, term.screen_lines() as u16)
            })
        });
        sizes.lock().unwrap().clear();
        let captured = sizes.clone();
        let (_second, cx) = cx.add_window_view(|_, cx| {
            let config = TerminalConfig {
                cols: grid.0 as usize,
                rows: grid.1 as usize,
                ..TerminalConfig::default()
            };
            TerminalView::new(Arc::new(RecordingSession(captured)), config, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            sizes.lock().unwrap().last().copied(),
            Some(grid),
            "首次布局必须同步会话网格"
        );
    }

    #[gpui::test]
    fn grid_point_at_maps_visible_row_through_display_offset(cx: &mut TestAppContext) {
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update(cx, |view, _| {
            // 显式固定小网格，避免依赖测试窗口首帧测量出的尺寸
            view.resize(10, 5);
            // 先灌出回滚历史，否则空网格无法滚动
            for i in 0..20 {
                view.state.process_bytes(format!("line {i}\r\n").as_bytes());
            }
            view.last_content_origin = Some(gpui::Point {
                x: gpui::px(0.0),
                y: gpui::px(0.0),
            });
            view.renderer.cell_width = gpui::px(10.0);
            view.renderer.cell_height = gpui::px(20.0);
            // 向上滚 2 行后，可见首行对应网格 Line(-2)
            view.state.with_term_mut(|term| {
                term.scroll_display(alacritty_terminal::grid::Scroll::Delta(2));
            });
            let point = view
                .grid_point_at(gpui::point(gpui::px(5.0), gpui::px(10.0)))
                .expect("网格点");
            assert_eq!(
                point.line.0, -2,
                "可见首行在 display_offset=2 时应映射为 Line(-2)"
            );
            assert_eq!(point.column.0, 0);
        });
    }

    #[gpui::test]
    fn mouse_selection_roundtrips_text_and_clears_on_plain_click(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseMoveEvent, MouseUpEvent};

        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update_in(cx, |view, window, cx| {
            view.state.process_bytes(b"hello selection\r\n$ ");
            view.last_content_origin = Some(gpui::Point {
                x: gpui::px(0.0),
                y: gpui::px(0.0),
            });
            view.renderer.cell_width = gpui::px(10.0);
            view.renderer.cell_height = gpui::px(20.0);

            let down = |position, click_count| MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            };
            let move_to = |position| MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            let up = |position| MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            };

            // 拖拽：down(0,0) → move(0,4) → up，应选中 "hello"
            view.on_mouse_down(
                &down(gpui::point(gpui::px(5.0), gpui::px(10.0)), 1),
                window,
                cx,
            );
            view.on_mouse_move(
                &move_to(gpui::point(gpui::px(45.0), gpui::px(10.0))),
                window,
                cx,
            );
            view.on_mouse_up(&up(gpui::point(gpui::px(45.0), gpui::px(10.0))), window, cx);
            assert!(view.has_selection(), "拖拽后应有选区");
            let text = view.state.with_term(|term| term.selection_to_string());
            assert_eq!(text.as_deref(), Some("hello"));

            // 复制公开 API 不 panic（剪贴板写入走 gpui）
            view.copy_selection(cx);

            // 单击（未拖拽）→ up 后选区清空
            view.on_mouse_down(
                &down(gpui::point(gpui::px(5.0), gpui::px(10.0)), 1),
                window,
                cx,
            );
            view.on_mouse_up(&up(gpui::point(gpui::px(5.0), gpui::px(10.0))), window, cx);
            assert!(!view.has_selection(), "单击未拖拽不应留下选区");
        });
    }

    #[gpui::test]
    fn copy_on_select_auto_copies_valid_selection_on_mouse_up(cx: &mut TestAppContext) {
        use gpui::{ClipboardItem, Modifiers, MouseMoveEvent, MouseUpEvent};

        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update_in(cx, |view, window, cx| {
            view.state.process_bytes(b"hello selection\r\n$ ");
            view.last_content_origin = Some(gpui::Point {
                x: gpui::px(0.0),
                y: gpui::px(0.0),
            });
            view.renderer.cell_width = gpui::px(10.0);
            view.renderer.cell_height = gpui::px(20.0);

            let down = |position, click_count| MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            };
            let move_to = |position| MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            let up = |position| MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            };

            // 拖拽松开（默认开）→ 自动写入剪贴板
            view.on_mouse_down(
                &down(gpui::point(gpui::px(5.0), gpui::px(10.0)), 1),
                window,
                cx,
            );
            view.on_mouse_move(
                &move_to(gpui::point(gpui::px(45.0), gpui::px(10.0))),
                window,
                cx,
            );
            view.on_mouse_up(&up(gpui::point(gpui::px(45.0), gpui::px(10.0))), window, cx);
            assert!(view.has_selection(), "拖拽后应有选区");
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("hello".to_string()),
                "拖拽松开应自动复制选中文本"
            );

            // 预写哨兵后单击清选区：不覆盖剪贴板
            cx.write_to_clipboard(ClipboardItem::new_string("sentinel".to_string()));
            view.on_mouse_down(
                &down(gpui::point(gpui::px(5.0), gpui::px(10.0)), 1),
                window,
                cx,
            );
            view.on_mouse_up(&up(gpui::point(gpui::px(5.0), gpui::px(10.0))), window, cx);
            assert!(!view.has_selection(), "单击未拖拽不应留下选区");
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("sentinel".to_string()),
                "单击清选区路径不应覆盖剪贴板"
            );
        });

        // 关闭开关：拖拽松开保留选区但不写剪贴板
        let (terminal_off, cx) = cx.add_window_view(|_, cx| {
            let config = TerminalConfig {
                copy_on_select: false,
                ..TerminalConfig::default()
            };
            TerminalView::new(Arc::new(FinishedSession), config, cx)
        });
        terminal_off.update_in(cx, |view, window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".to_string()));
            view.state.process_bytes(b"hello selection\r\n$ ");
            view.last_content_origin = Some(gpui::Point {
                x: gpui::px(0.0),
                y: gpui::px(0.0),
            });
            view.renderer.cell_width = gpui::px(10.0);
            view.renderer.cell_height = gpui::px(20.0);

            let down = |position, click_count| MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            };
            let move_to = |position| MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            let up = |position| MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            };

            view.on_mouse_down(
                &down(gpui::point(gpui::px(5.0), gpui::px(10.0)), 1),
                window,
                cx,
            );
            view.on_mouse_move(
                &move_to(gpui::point(gpui::px(45.0), gpui::px(10.0))),
                window,
                cx,
            );
            view.on_mouse_up(&up(gpui::point(gpui::px(45.0), gpui::px(10.0))), window, cx);
            assert!(view.has_selection(), "关闭开关仍应保留选区");
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("sentinel".to_string()),
                "开关关闭时拖拽松开不应写剪贴板"
            );
        });
    }

    /// 录制写入字节的 stdin 替身；断言鼠标上报编码用。
    struct RecordingWriter(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 固定网格度量，并用真实 VTE 解析置位鼠标模式后清空字节缓冲。
    fn enable_mouse_reporting(
        view: &mut TerminalView,
        enable_sequence: &[u8],
        buffer: &Arc<std::sync::Mutex<Vec<u8>>>,
    ) {
        view.state.process_bytes(b"hello selection\r\n$ ");
        view.state.process_bytes(enable_sequence);
        view.last_content_origin = Some(gpui::Point {
            x: gpui::px(0.0),
            y: gpui::px(0.0),
        });
        view.renderer.cell_width = gpui::px(10.0);
        view.renderer.cell_height = gpui::px(20.0);
        buffer.lock().unwrap().clear();
    }

    #[gpui::test]
    fn sgr_click_reports_bytes_and_suppresses_local_selection(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseMoveEvent, MouseUpEvent};
        use std::sync::Mutex;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded = buffer.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            let view = TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx);
            *view.stdin_writer.lock() = Box::new(RecordingWriter(recorded));
            view
        });
        terminal.update_in(cx, |view, window, cx| {
            enable_mouse_reporting(view, b"\x1b[?1000;1006h", &buffer);
            let term_mode = alacritty_terminal::term::TermMode::MOUSE_REPORT_CLICK
                | alacritty_terminal::term::TermMode::SGR_MOUSE;
            assert!(
                view.state.mode().contains(term_mode),
                "?1000;1006h 必须真实置位 CLICK|SGR"
            );

            let down = MouseDownEvent {
                button: MouseButton::Left,
                position: gpui::point(gpui::px(5.0), gpui::px(10.0)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            view.on_mouse_down(&down, window, cx);
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<0;1;1M",
                "按下上报 SGR press"
            );
            assert!(!view.has_selection(), "上报接管时本地选区让位");

            // 1000 模式不报移动
            let motion = MouseMoveEvent {
                position: gpui::point(gpui::px(45.0), gpui::px(10.0)),
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            view.on_mouse_move(&motion, window, cx);
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<0;1;1M",
                "1000 模式移动不上报"
            );
            assert!(!view.has_selection(), "上报接管期间不建本地选区");

            let up = MouseUpEvent {
                button: MouseButton::Left,
                position: gpui::point(gpui::px(5.0), gpui::px(10.0)),
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            view.on_mouse_up(&up, window, cx);
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<0;1;1M\x1b[<0;1;1m",
                "松开上报 SGR release"
            );
            assert!(
                view.mouse_down_reported.is_none(),
                "release 后不得悬挂按下标记"
            );
        });
    }

    #[gpui::test]
    fn sgr_drag_reports_motion_bytes_without_local_selection(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseMoveEvent, MouseUpEvent};
        use std::sync::Mutex;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded = buffer.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            let view = TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx);
            *view.stdin_writer.lock() = Box::new(RecordingWriter(recorded));
            view
        });
        terminal.update_in(cx, |view, window, cx| {
            enable_mouse_reporting(view, b"\x1b[?1002;1006h", &buffer);

            let down = |position| MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            let drag_to = |position| MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            let up = |position| MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            };

            view.on_mouse_down(
                &down(gpui::point(gpui::px(5.0), gpui::px(10.0))),
                window,
                cx,
            );
            assert_eq!(*buffer.lock().unwrap(), b"\x1b[<0;1;1M", "press 上报");

            // 拖到 col 4 → 32;5;1
            view.on_mouse_move(
                &drag_to(gpui::point(gpui::px(45.0), gpui::px(10.0))),
                window,
                cx,
            );
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<0;1;1M\x1b[<32;5;1M",
                "拖拽移动按 32 上报"
            );
            assert!(!view.has_selection(), "拖拽期间不建本地选区");

            // 同格重复移动去重，不重发
            view.on_mouse_move(
                &drag_to(gpui::point(gpui::px(45.0), gpui::px(10.0))),
                window,
                cx,
            );
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<0;1;1M\x1b[<32;5;1M",
                "同格移动不重发"
            );

            // 跨格移动再次上报（col 6 → 7）
            view.on_mouse_move(
                &drag_to(gpui::point(gpui::px(65.0), gpui::px(10.0))),
                window,
                cx,
            );
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<0;1;1M\x1b[<32;5;1M\x1b[<32;7;1M"
            );

            view.on_mouse_up(&up(gpui::point(gpui::px(65.0), gpui::px(10.0))), window, cx);
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<0;1;1M\x1b[<32;5;1M\x1b[<32;7;1M\x1b[<0;7;1m",
                "release 与 press 配对上报"
            );
            assert!(!view.has_selection(), "全程无本地选区");
        });
    }

    #[gpui::test]
    fn shift_bypass_keeps_local_selection_while_reporting(cx: &mut TestAppContext) {
        use gpui::{MouseMoveEvent, MouseUpEvent};
        use std::sync::Mutex;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded = buffer.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            let view = TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx);
            *view.stdin_writer.lock() = Box::new(RecordingWriter(recorded));
            view
        });
        terminal.update_in(cx, |view, window, cx| {
            enable_mouse_reporting(view, b"\x1b[?1000;1006h", &buffer);

            let shift = gpui::Modifiers {
                shift: true,
                ..Default::default()
            };
            let down = MouseDownEvent {
                button: MouseButton::Left,
                position: gpui::point(gpui::px(5.0), gpui::px(10.0)),
                modifiers: shift,
                click_count: 1,
                first_mouse: false,
            };
            let drag_to = MouseMoveEvent {
                position: gpui::point(gpui::px(45.0), gpui::px(10.0)),
                pressed_button: Some(MouseButton::Left),
                modifiers: shift,
            };
            let up = MouseUpEvent {
                button: MouseButton::Left,
                position: gpui::point(gpui::px(45.0), gpui::px(10.0)),
                modifiers: shift,
                click_count: 1,
            };

            view.on_mouse_down(&down, window, cx);
            view.on_mouse_move(&drag_to, window, cx);
            view.on_mouse_up(&up, window, cx);
            assert!(
                buffer.lock().unwrap().is_empty(),
                "Shift 绕过：全程零上报字节"
            );
            assert!(view.has_selection(), "Shift 绕过走本地选区");
            let text = view.state.with_term(|term| term.selection_to_string());
            assert_eq!(text.as_deref(), Some("hello"));
            assert!(view.mouse_down_reported.is_none());
        });
    }

    #[gpui::test]
    fn disabled_reporting_restores_local_selection(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseMoveEvent, MouseUpEvent};
        use std::sync::Mutex;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded = buffer.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            let view = TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx);
            *view.stdin_writer.lock() = Box::new(RecordingWriter(recorded));
            view
        });
        terminal.update_in(cx, |view, window, cx| {
            enable_mouse_reporting(view, b"\x1b[?1000;1006h", &buffer);
            view.state.process_bytes(b"\x1b[?1000l\x1b[?1006l");
            buffer.lock().unwrap().clear();

            let down = |position, click_count| MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            };
            let drag_to = |position| MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            let up = |position| MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            };

            // 关闭后 down/move/up 重建本地选区，零上报字节
            view.on_mouse_down(
                &down(gpui::point(gpui::px(5.0), gpui::px(10.0)), 1),
                window,
                cx,
            );
            view.on_mouse_move(
                &drag_to(gpui::point(gpui::px(45.0), gpui::px(10.0))),
                window,
                cx,
            );
            view.on_mouse_up(&up(gpui::point(gpui::px(45.0), gpui::px(10.0))), window, cx);
            assert!(buffer.lock().unwrap().is_empty(), "关闭上报后零字节");
            assert!(view.has_selection(), "关闭后恢复本地选区");
            let text = view.state.with_term(|term| term.selection_to_string());
            assert_eq!(text.as_deref(), Some("hello"));
        });
    }

    #[gpui::test]
    fn mode_flip_mid_press_drops_release_without_dangling_flag(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseUpEvent};
        use std::sync::Mutex;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded = buffer.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            let view = TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx);
            *view.stdin_writer.lock() = Box::new(RecordingWriter(recorded));
            view
        });
        terminal.update_in(cx, |view, window, cx| {
            enable_mouse_reporting(view, b"\x1b[?1000;1006h", &buffer);

            let down = MouseDownEvent {
                button: MouseButton::Left,
                position: gpui::point(gpui::px(5.0), gpui::px(10.0)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            view.on_mouse_down(&down, window, cx);
            assert_eq!(*buffer.lock().unwrap(), b"\x1b[<0;1;1M");

            // 应用在按住期间退出鼠标模式：release 静默丢弃、无悬挂标记
            view.state.process_bytes(b"\x1b[?1000l");
            let up = MouseUpEvent {
                button: MouseButton::Left,
                position: gpui::point(gpui::px(5.0), gpui::px(10.0)),
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            view.on_mouse_up(&up, window, cx);
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<0;1;1M",
                "模式翻转后 release 不写字节"
            );
            assert!(view.mouse_down_reported.is_none(), "无悬挂按下标记");
            assert!(!view.has_selection(), "翻转路径不落本地选区");
        });
    }

    #[gpui::test]
    fn sgr_flip_mid_press_never_feeds_1000_only_app(cx: &mut TestAppContext) {
        // 回归钉子（2026-11-06）：按住期间应用单独退订 SGR（?1006l，1000 保留）
        // 时，release 旧实现只查 mouse_down_reported 就发 SGR 字节——违反
        // mouse_reporting_active 门控不变量。up 分支补门控后零新字节。
        use gpui::{Modifiers, MouseUpEvent};
        use std::sync::Mutex;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded = buffer.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            let view = TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx);
            *view.stdin_writer.lock() = Box::new(RecordingWriter(recorded));
            view
        });
        terminal.update_in(cx, |view, window, cx| {
            enable_mouse_reporting(view, b"\x1b[?1000;1006h", &buffer);

            let down = MouseDownEvent {
                button: MouseButton::Left,
                position: gpui::point(gpui::px(5.0), gpui::px(10.0)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            view.on_mouse_down(&down, window, cx);
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<0;1;1M",
                "按下上报 SGR press"
            );

            // 应用按住期间单独发 ?1006l（保留 1000）：模式仍含 CLICK 但已无 SGR
            view.state.process_bytes(b"\x1b[?1006l");
            let mode = view.state.mode();
            assert!(
                mode.contains(alacritty_terminal::term::TermMode::MOUSE_REPORT_CLICK)
                    && !mode.contains(alacritty_terminal::term::TermMode::SGR_MOUSE),
                "1006l 后应为 1000-only（CLICK 置位、SGR 清除）"
            );

            // release 被门控拦下：零新字节、无悬挂标记、无本地选区
            let up = MouseUpEvent {
                button: MouseButton::Left,
                position: gpui::point(gpui::px(5.0), gpui::px(10.0)),
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            view.on_mouse_up(&up, window, cx);
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<0;1;1M",
                "1000-only 下 release 绝不补发 SGR 字节"
            );
            assert!(view.mouse_down_reported.is_none(), "无悬挂按下标记");
            assert!(!view.has_selection(), "门控路径不落本地选区");
        });
    }

    #[gpui::test]
    fn scroll_reports_wheel_in_sgr_mode_and_falls_back_locally_when_off(cx: &mut TestAppContext) {
        use gpui::{ScrollDelta, ScrollWheelEvent, TouchPhase};
        use std::sync::Mutex;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded = buffer.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            let view = TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx);
            *view.stdin_writer.lock() = Box::new(RecordingWriter(recorded));
            view
        });
        terminal.update_in(cx, |view, window, cx| {
            // 显式固定小网格并灌出回滚历史，供本地滚动断言（测试窗口首帧会
            // 按窗口尺寸重排网格，必须显式 resize，照既有 grid_point_at 测试）
            view.resize(10, 5);
            for i in 0..20 {
                view.state.process_bytes(format!("line {i}\r\n").as_bytes());
            }
            view.state.process_bytes(b"\x1b[?1000;1006h");
            view.last_content_origin = Some(gpui::Point {
                x: gpui::px(0.0),
                y: gpui::px(0.0),
            });
            view.renderer.cell_width = gpui::px(10.0);
            view.renderer.cell_height = gpui::px(20.0);
            buffer.lock().unwrap().clear();

            let wheel = |lines: f32| ScrollWheelEvent {
                position: gpui::point(gpui::px(5.0), gpui::px(10.0)),
                delta: ScrollDelta::Lines(gpui::point(0.0, lines)),
                modifiers: Default::default(),
                touch_phase: TouchPhase::Moved,
            };
            let display_offset =
                |view: &TerminalView| view.state.with_term(|term| term.grid().display_offset());
            assert_eq!(display_offset(view), 0);

            view.on_scroll(&wheel(1.0), window, cx);
            assert_eq!(
                *buffer.lock().unwrap(),
                b"\x1b[<64;1;1M",
                "滚轮按 64 上报真实格点"
            );
            assert_eq!(display_offset(view), 0, "上报接管时本地回滚不动");

            // 关闭上报：同事件回落本地滚动，零字节
            view.state.process_bytes(b"\x1b[?1000l\x1b[?1006l");
            buffer.lock().unwrap().clear();
            view.on_scroll(&wheel(1.0), window, cx);
            assert!(buffer.lock().unwrap().is_empty(), "关闭后滚轮零字节");
            assert_eq!(display_offset(view), 1, "关闭后滚轮走本地回滚");
        });
    }

    #[gpui::test]
    fn right_click_stays_local_menu_while_reporting(cx: &mut TestAppContext) {
        use super::TerminalMenuRequested;
        use gpui::Modifiers;
        use std::sync::Mutex;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded = buffer.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            let view = TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx);
            *view.stdin_writer.lock() = Box::new(RecordingWriter(recorded));
            view
        });
        let menus = Arc::new(AtomicUsize::new(0));
        let count = menus.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&terminal, move |_, _: &TerminalMenuRequested, _| {
                count.fetch_add(1, Ordering::SeqCst);
            })
        });
        terminal.update_in(cx, |view, window, cx| {
            enable_mouse_reporting(view, b"\x1b[?1000;1006h", &buffer);

            let right_down = MouseDownEvent {
                button: MouseButton::Right,
                position: gpui::point(gpui::px(5.0), gpui::px(10.0)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            view.on_right_click(&right_down, window, cx);
        });
        // GPUI 事件经 effect flush 送达订阅者，先跑空任务队列再断言
        cx.run_until_parked();
        terminal.read_with(cx, |view, _| {
            assert_eq!(menus.load(Ordering::SeqCst), 1, "右键仍发本地菜单请求");
            assert!(!view.has_selection(), "右键不建选区");
        });
        assert!(
            buffer.lock().unwrap().is_empty(),
            "右键不上报字节（既定决策）"
        );
    }

    #[gpui::test]
    fn search_shortcut_rejects_extra_modifiers(cx: &mut TestAppContext) {
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update_in(cx, |terminal, window, cx| {
            for key in ["ctrl-alt-shift-f", "ctrl-shift-super-f", "ctrl-shift-fn-f"] {
                terminal.on_key_down(
                    &gpui::KeyDownEvent {
                        keystroke: gpui::Keystroke::parse(key).unwrap(),
                        is_held: false,
                    },
                    window,
                    cx,
                );
                assert!(terminal.search.is_none(), "额外修饰键不能打开搜索: {key}");
            }
            terminal.on_key_down(
                &gpui::KeyDownEvent {
                    keystroke: gpui::Keystroke::parse("ctrl-shift-f").unwrap(),
                    is_held: false,
                },
                window,
                cx,
            );
            assert!(terminal.search.is_some(), "精确 Ctrl+Shift+F 必须打开搜索");
        });
    }

    /// 提示符跳转（OSC 133）：Alt+Shift+Up/Down 键绑定与直调同效；
    /// 额外修饰键不触发（精确匹配口径）。视口位置经 display_offset 断言。
    #[gpui::test]
    fn prompt_jump_shortcuts_scroll_between_marks(cx: &mut TestAppContext) {
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update_in(cx, |terminal, window, cx| {
            // 两条提示符标记：测试窗口会在首帧布局后 resize（默认窗口下
            // 可远大于配置 24 行），行数按实际网格取，确保首标记滚进历史。
            terminal.state.process_bytes(b"\x1b]133;A\x07");
            let output_lines = terminal.state.with_term(|term| term.grid().screen_lines()) + 8;
            for line in 0..output_lines {
                terminal
                    .state
                    .process_bytes(format!("row-{line}\r\n").as_bytes());
            }
            terminal.state.process_bytes(b"\x1b]133;A\x07");
            assert_eq!(
                terminal
                    .state
                    .with_term(|term| term.grid().display_offset()),
                0,
                "新输出后视口应钉在底部"
            );

            // Alt+Shift+Up → 上一条提示符（视口离开底部）
            press_key(terminal, "alt-shift-up", window, cx);
            let up_offset = terminal
                .state
                .with_term(|term| term.grid().display_offset());
            assert!(up_offset > 0, "Alt+Shift+Up 应滚回上一提示符");

            // Alt+Shift+Down → 下一条提示符（滚回底部）
            press_key(terminal, "alt-shift-down", window, cx);
            let down_offset = terminal
                .state
                .with_term(|term| term.grid().display_offset());
            assert!(down_offset < up_offset, "Alt+Shift+Down 应回到底部");

            // 带额外修饰键不触发（也不透传 PTY，视口不动）
            press_key(terminal, "ctrl-alt-shift-up", window, cx);
            assert_eq!(
                terminal
                    .state
                    .with_term(|term| term.grid().display_offset()),
                down_offset,
                "额外修饰键不触发跳转"
            );
        });
    }

    /// cwd 继承判别：本地会话记录并经视图暴露（OSC 7 percent 解码口径）；
    /// SSH 会话即便远端 shell 上报 OSC 7，`TerminalView::cwd` 也恒 None
    ///（远端路径不能喂本地 spawn，TODO 留给 laterterm-ssh）。
    #[gpui::test]
    fn cwd_reported_only_for_local_backend(cx: &mut TestAppContext) {
        // 本地会话：OSC 7 → 视图 cwd（含 percent 解码的中文与空格路径）
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update_in(cx, |terminal, _, _| {
            assert!(terminal.cwd().is_none(), "未上报时 cwd 为 None");
            terminal.state.process_bytes(
                "\x1b]7;file://myhost/home/u/%E4%B8%AD%E6%96%87%20dir\x07".as_bytes(),
            );
            assert_eq!(
                terminal.cwd().as_deref(),
                Some("/home/u/中文 dir"),
                "本地会话 cwd 应记录解码后的路径"
            );
        });

        // SSH 会话：状态层照单记录，视图层按后端门控不暴露
        let (remote, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(SshSession), TerminalConfig::default(), cx)
        });
        remote.update_in(cx, |terminal, _, _| {
            terminal.state.process_bytes(b"\x1b]7;file:///srv/www\x07");
            assert!(terminal.cwd().is_none(), "SSH 会话不对外暴露 cwd");
        });
    }

    /// URL 解析优先级（OSC 8 项）：显式超链接优先于文本自动检测，无
    /// 超链接时回落自动检测。三条入口共用 resolve_url_at，此处钉其口径。
    #[gpui::test]
    fn url_resolution_prefers_osc8_hyperlink_then_falls_back(cx: &mut TestAppContext) {
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update_in(cx, |terminal, _, _| {
            // 第一行：OSC 8 段「click」（列 0-4，展示文本无 URL 特征）+
            // 空格（列 5）+ 纯文本 URL（列 6 起，无 OSC 8 包裹）
            terminal.state.process_bytes(
                b"\x1b]8;;https://hyper.example\x1b\\click\x1b]8;;\x1b\\ https://text.example",
            );
            // 段内：展示文本不是 URL 形态 → 仅超链接命中
            assert_eq!(
                terminal.resolve_url_at(0, 0).as_deref(),
                Some("https://hyper.example"),
                "OSC 8 段内优先返回声明 URI"
            );
            assert_eq!(
                terminal.resolve_url_at(0, 4).as_deref(),
                Some("https://hyper.example"),
                "段内任意列命中同一段"
            );
            // 段外：纯文本 URL → 回落文本自动检测
            assert_eq!(
                terminal.resolve_url_at(0, 8).as_deref(),
                Some("https://text.example"),
                "段外回落文本自动检测"
            );
            assert_eq!(terminal.resolve_url_at(0, 5), None, "空格列两者皆不命中");

            // 第二行（并存优先级）：URL 样文本「https://fake.example」被
            // OSC 8 包住但声明 URI 不同 → 超链接胜出（文本检测返回 fake，
            // resolve 返回 real——分层由 state 层测试钉，此处钉 view 取用序）
            terminal.state.process_bytes(
                b"\r\n\x1b]8;;https://real.example\x07https://fake.example\x1b]8;;\x07",
            );
            assert_eq!(
                terminal.resolve_url_at(1, 0).as_deref(),
                Some("https://real.example"),
                "并存时超链接声明优先"
            );
        });
    }

    /// 通知文本助手：AppleScript 转义（\ 与 "）、截断与换行拍平。
    /// PowerShell 分支的 ' 翻倍在 send_desktop_notification 内联，
    /// 同一转义口径由本测试钉 AppleScript 侧的顺序敏感性。
    #[test]
    fn notification_text_helpers_escape_and_truncate() {
        use super::{escape_applescript_string, truncate_notification_text};
        // \ → \\ 与 " → \"，且反斜杠先转义（顺序反了会产生歧义序列）
        assert_eq!(escape_applescript_string("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(escape_applescript_string("plain"), "plain");
        // 截断 + 换行拍平（桌面通知单行短文案口径）
        assert_eq!(truncate_notification_text("中文"), "中文");
        assert_eq!(truncate_notification_text("a\nb\rc"), "a b c");
        assert_eq!(
            truncate_notification_text(&"x".repeat(200)),
            "x".repeat(128),
            "超长正文截到 128 字符"
        );
    }

    /// 计数三分支 + 非法正则红字替代（纯函数，无需 GPUI 上下文）。
    #[test]
    fn search_counter_text_branches() {
        use super::SearchState;

        let base = SearchState {
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            case_sensitive: false,
            regex: false,
            whole_word: false,
            regex_error: None,
            search_pending: false,
        };
        // 分支一：空查询
        assert_eq!(super::search_counter_text(&base), "0/0");
        // 分支二：有查询但无命中
        let mut no_match = base.clone();
        no_match.query = "zzz".into();
        assert_eq!(super::search_counter_text(&no_match), "无命中");
        // 分支三：n/m（current 循环移动后计数跟随）
        let mut hits = base.clone();
        hits.query = "ab".into();
        hits.matches = vec![
            crate::terminal::SearchMatch {
                line: 0,
                column: 0,
                len: 2
            };
            3
        ];
        hits.current = 2;
        assert_eq!(super::search_counter_text(&hits), "3/3");
        // 非法正则：红字「正则无效：<msg>」替代计数槽位
        let mut invalid = hits;
        invalid.regex_error = Some("(unclosed".into());
        assert_eq!(super::search_counter_text(&invalid), "正则无效：(unclosed");
    }

    #[gpui::test]
    fn search_alt_r_w_toggles_are_mutually_exclusive(cx: &mut TestAppContext) {
        use gpui::EntityInputHandler;

        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update_in(cx, |terminal, window, cx| {
            terminal.state.process_bytes(b"xab ab\n");
            terminal.open_search(cx);
            terminal.replace_text_in_range(None, "ab", window, cx);
            assert!(
                terminal.search.as_ref().unwrap().search_pending,
                "输入后挂起 150ms 防抖"
            );
            // Alt+R 开正则：开关切换即时刷新（唯一绕过防抖路径）
            press_key(terminal, "alt-r", window, cx);
            {
                let search = terminal.search.as_ref().unwrap();
                assert!(search.regex, "Alt+R 开启正则");
                assert!(!search.search_pending, "开关切换即时刷新，无挂起");
                assert_eq!(search.matches.len(), 2, "正则搜索立即生效（xab/ab 各一处）");
            }
            // 互斥：正则开启时 Alt+W 无效（W 钮置灰同语义）
            press_key(terminal, "alt-w", window, cx);
            assert!(
                !terminal.search.as_ref().unwrap().whole_word,
                "正则开启时 Alt+W 无效"
            );
            // 关正则后 Alt+W 生效；全字口径下 xab 的 ab 被排除
            press_key(terminal, "alt-r", window, cx);
            assert!(!terminal.search.as_ref().unwrap().regex);
            press_key(terminal, "alt-w", window, cx);
            let search = terminal.search.as_ref().unwrap();
            assert!(search.whole_word, "正则关闭后 Alt+W 生效");
            assert_eq!(search.matches.len(), 1, "全字排除 xab 中的 ab");
        });
    }

    #[gpui::test]
    fn search_invalid_regex_keeps_previous_matches(cx: &mut TestAppContext) {
        use gpui::EntityInputHandler;

        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update_in(cx, |terminal, window, cx| {
            terminal.state.process_bytes(b"ab ab\n");
            terminal.open_search(cx);
            terminal.replace_text_in_range(None, "ab", window, cx);
            press_key(terminal, "alt-r", window, cx);
            assert!(terminal.search.as_ref().unwrap().regex_error.is_none());

            // 追加 "(" 使正则非法（查询 "ab("），Enter 在防抖窗口内同步提交
            terminal.replace_text_in_range(None, "(", window, cx);
            assert!(terminal.search.as_ref().unwrap().search_pending);
            press_key(terminal, "enter", window, cx);
            {
                let search = terminal.search.as_ref().unwrap();
                assert!(search.regex_error.is_some(), "非法正则内联报错");
                assert_eq!(search.matches.len(), 2, "保留上次有效结果");
                assert!(!search.search_pending, "Enter 已消费挂起");
                assert_eq!(
                    super::search_counter_text(search),
                    format!("正则无效：{}", search.regex_error.as_ref().unwrap()),
                    "计数槽位换红字错误文案"
                );
            }

            // 修正查询（退格弹出括号）后 Enter 再次提交：报错清除
            terminal.search_backspace(cx);
            press_key(terminal, "enter", window, cx);
            let search = terminal.search.as_ref().unwrap();
            assert!(search.regex_error.is_none(), "合法正则刷新后清除报错");
            assert_eq!(search.matches.len(), 2);
        });
    }

    #[gpui::test]
    fn search_bar_button_actions_never_create_selection(cx: &mut TestAppContext) {
        use gpui::EntityInputHandler;

        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update_in(cx, |terminal, window, cx| {
            terminal.state.process_bytes(b"ab ab\n");
            terminal.open_search(cx);
            terminal.replace_text_in_range(None, "ab", window, cx);
            // 按钮动作链直驱（on_right_click 直调先例同口径）；真正的
            // stop_propagation 挂在 search_bar_button 的 on_mouse_down 注册里，
            // 处理器本体只改搜索态，绝不触碰 selection / PTY。
            terminal.toggle_search_case(cx); // Aa
            terminal.toggle_search_regex(cx); // .*
            terminal.toggle_search_whole_word(cx); // W（正则开启 → 无动作）
            terminal.toggle_search_regex(cx); // .* 关
            terminal.toggle_search_whole_word(cx); // W 开
            terminal.search_select_prev(cx); // ▲
            terminal.search_select_next(cx); // ▼
            assert!(!terminal.has_selection(), "搜索栏按钮点击不建选区");
            let search = terminal.search.as_ref().unwrap();
            assert!(search.case_sensitive, "Aa 动作生效");
            assert!(search.whole_word, "W 动作生效（正则已关）");
            assert!(!search.regex, ".* 开关往返复原");
        });
    }

    #[gpui::test]
    fn search_enter_flushes_pending_and_jumps_without_pending(cx: &mut TestAppContext) {
        use gpui::EntityInputHandler;

        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        terminal.update_in(cx, |terminal, window, cx| {
            terminal.state.process_bytes(b"ab ab ab\n");
            terminal.open_search(cx);
            terminal.replace_text_in_range(None, "ab", window, cx);
            assert!(terminal.search.as_ref().unwrap().search_pending);
        });
        // 防抖到期：假时钟推进 150ms 后挂起任务执行（先例 scroll 防抖测试）
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(150));
        cx.run_until_parked();
        terminal.update_in(cx, |terminal, window, cx| {
            let search = terminal.search.as_ref().unwrap();
            assert!(!search.search_pending, "防抖到期已执行");
            assert_eq!(search.matches.len(), 3);
            assert_eq!(search.current, 0, "防抖刷新落首命中");
            // 分支一（无挂起）：Enter 维持历史行为——在既有 matches 上循环跳转
            press_key(terminal, "enter", window, cx);
            assert_eq!(
                terminal.search.as_ref().unwrap().current,
                1,
                "无挂起 Enter=跳下一命中"
            );
            press_key(terminal, "enter", window, cx);
            assert_eq!(terminal.search.as_ref().unwrap().current, 2);
            assert_eq!(
                terminal.search.as_ref().unwrap().matches.len(),
                3,
                "跳转不重算 matches"
            );

            // 分支二（有挂起）：改查询后 150ms 内按 Enter → 同步提交挂起搜索
            // （查询 "aba" 无命中：flush 后 matches 清空、落首命中、消费按键）
            terminal.replace_text_in_range(None, "a", window, cx);
            assert!(terminal.search.as_ref().unwrap().search_pending);
            press_key(terminal, "enter", window, cx);
            let search = terminal.search.as_ref().unwrap();
            assert!(!search.search_pending, "Enter 同步消费挂起");
            assert_eq!(search.query, "aba");
            assert!(search.matches.is_empty(), "flush 按新查询重算");
            assert_eq!(search.current, 0, "flush 落首命中");
        });
        // Enter 消费挂起后，原防抖任务醒来必须直接丢弃（不复活旧结果）
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(150));
        cx.run_until_parked();
        terminal.update(cx, |terminal, _| {
            let search = terminal.search.as_ref().unwrap();
            assert!(!search.search_pending);
            assert!(search.matches.is_empty(), "迟到的防抖任务不得复活旧结果");
        });
    }

    #[gpui::test]
    fn committed_keyboard_and_ime_input_share_one_callback(cx: &mut TestAppContext) {
        let input = Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
        let received = input.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
                .with_input_callback(move |bytes| received.lock().unwrap().push(bytes.to_vec()))
        });
        terminal.update_in(cx, |terminal, window, cx| {
            use gpui::EntityInputHandler;

            terminal.on_key_down(
                &gpui::KeyDownEvent {
                    keystroke: gpui::Keystroke::parse("enter").unwrap(),
                    is_held: false,
                },
                window,
                cx,
            );
            terminal.on_key_down(
                &gpui::KeyDownEvent {
                    keystroke: gpui::Keystroke::parse("a").unwrap(),
                    is_held: false,
                },
                window,
                cx,
            );
            assert_eq!(input.lock().unwrap().len(), 1, "普通按键等待文本提交");
            terminal.replace_text_in_range(None, "a", window, cx);
            terminal.replace_and_mark_text_in_range(None, "zhong", None, window, cx);
            assert_eq!(input.lock().unwrap().len(), 2, "预编辑不能广播");
            terminal.replace_text_in_range(None, "中文", window, cx);
            assert!(terminal.marked_text.is_empty());

            // 广播目标入口不可再次广播；搜索态只改本地查询。
            terminal.write_input(b"remote broadcast");
            terminal.open_search(cx);
            terminal.replace_text_in_range(None, "query", window, cx);
            terminal.on_key_down(
                &gpui::KeyDownEvent {
                    keystroke: gpui::Keystroke::parse("ctrl-c").unwrap(),
                    is_held: false,
                },
                window,
                cx,
            );
        });
        assert_eq!(
            *input.lock().unwrap(),
            vec![b"\r".to_vec(), b"a".to_vec(), "中文".as_bytes().to_vec()],
            "控制键、ASCII 与 IME 提交各广播一次"
        );
    }

    #[gpui::test]
    fn input_callback_runs_outside_writer_lock_and_respects_key_handler(cx: &mut TestAppContext) {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            let terminal =
                TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx);
            let writer = terminal.stdin_writer.clone();
            terminal
                .with_key_handler(|event| event.keystroke.key == "escape")
                .with_input_callback(move |_| {
                    assert!(writer.try_lock().is_some(), "回调前必须释放本端写锁");
                    observed.fetch_add(1, Ordering::SeqCst);
                })
        });
        terminal.update_in(cx, |terminal, window, cx| {
            for key in ["escape", "enter"] {
                terminal.on_key_down(
                    &gpui::KeyDownEvent {
                        keystroke: gpui::Keystroke::parse(key).unwrap(),
                        is_held: false,
                    },
                    window,
                    cx,
                );
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1, "拦截的按键不广播");
    }

    #[gpui::test]
    fn input_callback_ignores_write_and_flush_failures(cx: &mut TestAppContext) {
        struct FailingWriter {
            fail_on_flush: bool,
        }
        impl std::io::Write for FailingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.fail_on_flush {
                    Ok(bytes.len())
                } else {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
                .with_input_callback(move |_| {
                    observed.fetch_add(1, Ordering::SeqCst);
                })
        });
        terminal.update_in(cx, |terminal, window, cx| {
            use gpui::EntityInputHandler;

            for fail_on_flush in [false, true] {
                *terminal.stdin_writer.lock() = Box::new(FailingWriter { fail_on_flush });
                terminal.on_key_down(
                    &gpui::KeyDownEvent {
                        keystroke: gpui::Keystroke::parse("enter").unwrap(),
                        is_held: false,
                    },
                    window,
                    cx,
                );
                terminal.replace_text_in_range(None, "中文", window, cx);
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0, "失败输入不广播");
    }

    #[gpui::test]
    fn scroll_callback_stays_debounced_and_only_reports_changes(cx: &mut TestAppContext) {
        use std::time::Duration;

        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let terminal = cx.new(|cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
                .with_scroll_callback(move |_| {
                    observed.fetch_add(1, Ordering::SeqCst);
                })
        });
        cx.background_executor.allow_parking();
        terminal.update(cx, |terminal, cx| {
            for _ in 0..10 {
                terminal.schedule_scroll_report(cx);
            }
        });
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(Duration::from_millis(149));
        cx.run_until_parked();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "150ms 内不刷新宿主");
        cx.background_executor
            .advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "同一洪峰只报告一次");

        terminal.update(cx, |terminal, cx| terminal.schedule_scroll_report(cx));
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "摘要未变不重复报告");

        terminal.update(cx, |terminal, cx| {
            terminal
                .state
                .process_bytes("line\r\n".repeat(40).as_bytes());
            terminal.schedule_scroll_report(cx);
        });
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "历史变化必须上报");
    }

    #[gpui::test]
    fn font_invalidation_is_consumed_by_next_render(cx: &mut TestAppContext) {
        use gpui::px;

        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        cx.update(|window, cx| window.draw(cx).clear());
        terminal.read_with(cx, |terminal, _| {
            assert!(!terminal.renderer.needs_measurement());
            assert!(!terminal.font_trace_changed);
        });

        terminal.update(cx, |terminal, cx| {
            let mut config = terminal.config().clone();
            config.font_size += px(1.0);
            terminal.update_config(config, cx);
            assert!(terminal.renderer.needs_measurement());
            assert!(terminal.font_trace_changed);
        });
        cx.update(|window, cx| window.draw(cx).clear());
        terminal.read_with(cx, |terminal, _| {
            assert!(!terminal.renderer.needs_measurement());
            assert!(!terminal.font_trace_changed);
        });

        terminal.update(cx, |terminal, cx| {
            let mut config = terminal.config().clone();
            config.ligatures = !config.ligatures;
            terminal.update_config(config, cx);
            assert!(!terminal.renderer.needs_measurement());
            assert!(!terminal.font_trace_changed);
        });
    }

    #[gpui::test]
    fn hidden_terminal_exits_without_render(cx: &mut TestAppContext) {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let terminal = cx
            .new(|cx| TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx));
        let _subscription = cx.update(|cx| {
            cx.subscribe(&terminal, move |_, _: &TerminalExited, _| {
                count.fetch_add(1, Ordering::SeqCst);
            })
        });
        // 不挂载到窗口：模拟非活动标签的终端。读泵 EOF 必须独立于 render 消费。
        for _ in 0..100 {
            cx.run_until_parked();
            if calls.load(Ordering::SeqCst) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(terminal);
    }

    /// 手动基准（默认 `#[ignore]`）：采集 `LATERTERM_RENDER_TRACE` 的锁窗口对照数据。
    ///
    /// ```text
    /// LATERTERM_RENDER_TRACE=1 cargo test --release -p laterterm-termview --lib \
    ///     render_trace_lock_window_benchmark -- --ignored --nocapture
    /// ```
    ///
    /// 隔离测试窗口逐帧 `window.draw`（不经真实 GPU/合成器，文本整形走同一文本系统），
    /// stderr 的 `[render-trace]` 行即为样本。对照口径只认 `paint_lock_hold_ms`
    /// （快照前=整段持锁绘制，快照后=仅快照复制，见 `render_trace_enabled` 注记）；
    /// 该数据只作锁窗口对照，非真机帧率基准（真机口径见 roadmap M1 注记）。
    #[gpui::test]
    #[ignore]
    fn render_trace_lock_window_benchmark(cx: &mut TestAppContext) {
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
        });
        // 灌入近似持续输出的网格：大量行 + 少量 SGR 着色/加粗，制造真实负载形态
        terminal.update(cx, |view, _| {
            for i in 0..2000 {
                let line =
                    format!("\x1b[1;34mrow {i:06}\x1b[0m 0123456789abcdefghijklmnopqrstuvwxyz\r\n");
                view.state.process_bytes(line.as_bytes());
            }
        });
        // 预热：首帧含字体测量与网格 resize，不计入样本
        for _ in 0..5 {
            cx.update(|window, cx| window.draw(cx).clear());
        }
        let frames = 200;
        for _ in 0..frames {
            cx.update(|window, cx| window.draw(cx).clear());
        }
    }

    #[test]
    fn paste_shortcut_matches_only_exact_two_combos() {
        use super::paste_shortcut_matches;

        for hit in ["ctrl-shift-v", "ctrl-shift-V", "shift-insert"] {
            assert!(
                paste_shortcut_matches(&gpui::Keystroke::parse(hit).unwrap()),
                "{hit} 应命中粘贴快捷键"
            );
        }
        // 单 Ctrl+V 是终端 0x16 惯例、Ctrl+C/Ctrl+Shift+C 等均不命中；
        // 附加修饰（Alt/Super/Fn）与 Shift+其它键也不命中。
        for miss in [
            "ctrl-v",
            "ctrl-c",
            "ctrl-shift-c",
            "ctrl-alt-shift-v",
            "ctrl-shift-super-v",
            "ctrl-shift-fn-v",
            "insert",
            "ctrl-insert",
            "shift-v",
        ] {
            assert!(
                !paste_shortcut_matches(&gpui::Keystroke::parse(miss).unwrap()),
                "{miss} 不应命中粘贴快捷键"
            );
        }
    }

    #[gpui::test]
    fn copy_selection_reports_char_count_and_notice_callback(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseMoveEvent, MouseUpEvent};
        use std::sync::Mutex;

        let noticed = Arc::new(Mutex::new(Vec::<usize>::new()));
        let observed = noticed.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
                .with_copy_notice_callback(move |_, _, chars| {
                    observed.lock().unwrap().push(chars);
                })
        });
        terminal.update_in(cx, |view, window, cx| {
            view.state.process_bytes(b"hello selection\r\n$ ");
            view.last_content_origin = Some(gpui::Point {
                x: gpui::px(0.0),
                y: gpui::px(0.0),
            });
            view.renderer.cell_width = gpui::px(10.0);
            view.renderer.cell_height = gpui::px(20.0);

            let down = |position, click_count| MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            };
            let move_to = |position| MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            let up = |position| MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            };

            // 拖拽松开：copy_on_select 路径写剪贴板并回调字符数
            view.on_mouse_down(
                &down(gpui::point(gpui::px(5.0), gpui::px(10.0)), 1),
                window,
                cx,
            );
            view.on_mouse_move(
                &move_to(gpui::point(gpui::px(45.0), gpui::px(10.0))),
                window,
                cx,
            );
            view.on_mouse_up(&up(gpui::point(gpui::px(45.0), gpui::px(10.0))), window, cx);
            assert_eq!(*noticed.lock().unwrap(), vec![5], "应回调已复制的字符数");

            // 公开 API 返回字符数；无选区时返回 None
            assert_eq!(view.copy_selection(cx), Some(5));
            view.state.with_term_mut(|term| term.selection = None);
            assert_eq!(view.copy_selection(cx), None);
        });
    }

    #[gpui::test]
    fn paste_request_callback_fires_only_outside_search(cx: &mut TestAppContext) {
        use std::sync::Mutex;

        let requests = Arc::new(Mutex::new(0usize));
        let observed = requests.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| {
            TerminalView::new(Arc::new(FinishedSession), TerminalConfig::default(), cx)
                .with_paste_request_callback(move |_, _| {
                    *observed.lock().unwrap() += 1;
                })
        });
        terminal.update_in(cx, |view, window, cx| {
            for key in ["ctrl-shift-v", "shift-insert"] {
                view.on_key_down(
                    &gpui::KeyDownEvent {
                        keystroke: gpui::Keystroke::parse(key).unwrap(),
                        is_held: false,
                    },
                    window,
                    cx,
                );
            }
            assert_eq!(*requests.lock().unwrap(), 2, "两种粘贴快捷键都应触发回调");

            // 搜索打开时快捷键被搜索分支吞掉，不触发粘贴请求
            view.open_search(cx);
            view.on_key_down(
                &gpui::KeyDownEvent {
                    keystroke: gpui::Keystroke::parse("ctrl-shift-v").unwrap(),
                    is_held: false,
                },
                window,
                cx,
            );
            assert_eq!(
                *requests.lock().unwrap(),
                2,
                "搜索态粘贴快捷键被拦截，不触发回调"
            );
        });
    }
}
