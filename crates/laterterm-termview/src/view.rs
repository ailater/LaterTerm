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
use crate::render::TerminalRenderer;
use crate::terminal::TerminalState;
use alacritty_terminal::grid::Dimensions;
use gpui::{Edges, ElementInputHandler, EntityInputHandler, UTF16Selection, *};
use laterterm_core::TerminalSession;
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

    /// 平滑滚动：滚轮/触控板按像素累积连续滚动，而不是一次跳一整行。
    /// 默认开启。
    pub smooth_scrolling: bool,
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
            smooth_scrolling: true,
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

    /// Callback for terminal title changes
    title_callback: Option<TitleCallback>,

    /// Callback for clipboard store requests
    clipboard_store_callback: Option<ClipboardStoreCallback>,

    /// Callback for terminal exit events
    exit_callback: Option<ExitCallback>,

    /// 滚动位置变化回调（供外部绘制滚动条指示器等）。
    scroll_callback: Option<ScrollCallback>,

    /// 上次上报的滚动摘要；与当前值不同才触发回调，防止 notify 风暴。
    last_scroll: Option<ScrollSummary>,
    /// 防抖在途标记：输出洪峰时合并为每 150ms 至多一次宿主刷新。
    scroll_report_scheduled: bool,

    /// Text currently being composed by the system input method.
    marked_text: String,

    /// 回滚搜索状态；打开时终端按键被拦截为搜索输入，不再写入 PTY。
    search: Option<SearchState>,

    /// 所属窗口句柄；首帧 render 时记录，PTY 数据路径用它把 notify
    /// 绑定到窗口上下文，避免后台任务通知不触发窗口重绘。
    window_handle: Option<AnyWindowHandle>,
}

/// 搜索栏状态。
struct SearchState {
    query: String,
    matches: Vec<crate::terminal::SearchMatch>,
    /// 当前命中的下标（在 matches 中循环移动）。
    current: usize,
}

/// 会话退出事件；不依赖终端是否可见。
pub struct TerminalExited;

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

        // Create event channel for terminal events
        let (event_tx, event_rx) = mpsc::channel();

        // Clone event_tx for the reader task to send Exit event when PTY closes
        let exit_event_tx = event_tx.clone();

        // Create event proxy for alacritty
        let event_proxy = GpuiEventProxy::new(event_tx);

        // Create terminal state
        let state = TerminalState::new(config.cols, config.rows, config.scrollback, event_proxy);

        // Create renderer with font settings and color palette
        let renderer = TerminalRenderer::new(
            config.font_family.clone(),
            config.font_size,
            config.line_height_multiplier,
            config.colors.clone(),
            config.ligatures,
            config.font_fallbacks.clone(),
        );

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
                        let changed = view.blink_on.swap(next, std::sync::atomic::Ordering::Relaxed)
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
            title_callback: None,
            clipboard_store_callback: None,
            exit_callback: None,
            scroll_callback: None,
            last_scroll: None,
            scroll_report_scheduled: false,
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
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
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

        // 搜索打开时拦截所有按键，一律不透传 PTY
        if self.search.is_some() {
            match keystroke.key.as_str() {
                "escape" => self.close_search(cx),
                "enter" | "return" => {
                    if keystroke.modifiers.shift {
                        self.search_select_prev(cx);
                    } else {
                        self.search_select_next(cx);
                    }
                }
                "backspace" => self.search_backspace(cx),
                "up" => self.search_select_prev(cx),
                "down" => self.search_select_next(cx),
                _ => {
                    // 可打印字符走 InputHandler 提交路径追加查询；
                    // 其余组合键（Ctrl+C 等）在搜索态直接吞掉。
                }
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

    /// Handle mouse down events.
    ///
    /// Currently a placeholder for future mouse selection and interaction support.
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Request focus when clicking the terminal
        window.focus(&self.focus_handle);
        // 点击后光标立即回到可见相位，避免刚点完就停在熄灭态。
        self.blink_on.store(true, std::sync::atomic::Ordering::Relaxed);
        cx.notify();

        // URL 点击：把像素坐标换算为网格单元，提取 URL 并用系统浏览器打开
        if let Some(origin) = self.last_content_origin {
            let cell = crate::mouse::pixel_to_cell(
                event.position,
                origin,
                self.renderer.cell_width,
                self.renderer.cell_height,
            );
            let row = cell.line.0.max(0) as usize;
            let col = cell.column.0;
            if let Some(url) = self.state.extract_url_at(row, col) {
                open_url(&url);
            }
        }
    }

    /// Handle mouse up events.
    ///
    /// Currently a placeholder for future mouse selection support.
    fn on_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // TODO: Implement mouse selection
        // - End selection at released cell
        // - Copy selection to clipboard if configured
    }

    /// Handle mouse move events.
    ///
    /// Currently a placeholder for future mouse selection support.
    fn on_mouse_move(
        &mut self,
        _event: &MouseMoveEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // TODO: Implement mouse selection
        // - Update selection range while dragging
        // - Send mouse motion reports if mouse tracking is enabled
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
        let point = alacritty_terminal::index::Point::new(
            alacritty_terminal::index::Line(0),
            alacritty_terminal::index::Column(0),
        );
        if let Some(bytes) = crate::mouse::scroll_report(lines, point, 0, mode) {
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
                    }
                }
                TerminalEvent::ClipboardLoad => {
                    // Terminal wants to load data from clipboard
                    // TODO: Implement clipboard integration
                }
                TerminalEvent::Exit => {
                    if let Some(callback) = self.exit_callback.take() {
                        // 兼容旧回调接口，不能在 render 中直接修改父布局。
                        cx.defer_in(window, move |_, window, cx| callback(window, cx));
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

    /// 搜索查询追加分本（来自 InputHandler 提交或按键）。
    fn search_push_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.query.push_str(text);
            self.refresh_search(cx);
        }
    }

    /// 删除查询的最后一个字符。
    fn search_backspace(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search
            && search.query.pop().is_some()
        {
            self.refresh_search(cx);
        }
    }

    /// 查询变化后重新执行搜索并跳到首个命中。
    fn refresh_search(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            let matches = self.state.search(&search.query);
            search.matches = matches;
            search.current = 0;
            self.scroll_to_current_match();
        }
        cx.notify();
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
        // 光标闪烁开关变化时立即恢复常显，避免停留在熄灭相位。
        if config.cursor_blink != self.config.cursor_blink && config.cursor_blink {
            self.blink_on.store(true, std::sync::atomic::Ordering::Relaxed);
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
        let focus_handle = self.focus_handle.clone();
        // 光标可见性快照：canvas 闭包在绘制阶段读取（闪烁相位由 blink 任务驱动）
        let cursor_visible =
            !self.config.cursor_blink
                || self.blink_on.load(std::sync::atomic::Ordering::Relaxed);
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
        let search_bar = self.search.as_ref().map(|search| {
            let counter = if search.query.is_empty() {
                "0/0".to_string()
            } else if search.matches.is_empty() {
                "无命中".to_string()
            } else {
                format!("{}/{}", search.current + 1, search.matches.len())
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
                .child(div().ml_auto().text_color(rgb(0xE0AF68)).child(counter))
                .child(
                    div()
                        .text_color(rgb(0x565A78))
                        .child("Enter↓ Shift+Enter↑ Esc 关闭"),
                )
        });

        let root = div()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, _window, _cx| bounds,
                    move |bounds, _, window, cx| {
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                        use alacritty_terminal::grid::Dimensions;

                        // 测量已在上面的实体 render 阶段完成；这里直接使用快照，
                        // 不在 canvas 闭包中重复字体整形。
                        let measured_renderer = renderer.clone();

                        // Calculate available space after padding, clamping tiny windows safely.
                        let available_width: f32 =
                            (bounds.size.width - padding.left - padding.right).into();
                        let available_height: f32 =
                            (bounds.size.height - padding.top - padding.bottom).into();
                        let cell_width_f32: f32 = measured_renderer.cell_width.into();
                        let cell_height_f32: f32 = measured_renderer.cell_height.into();

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

                        let paint_lock_started = trace.map(|_| Instant::now());
                        let term = state_arc.lock();
                        let paint_lock_wait = paint_lock_started.map(|started| started.elapsed());
                        let history_lines = trace.map(|_| term.grid().total_lines() - term.screen_lines());

                        // Paint the terminal with measured dimensions
                        let paint_started = trace.map(|_| Instant::now());
                        // 记录内容原点（URL 点击检测用：padding 内侧的字符区起点）
                        entity.update(cx, |view: &mut Self, _| {
                            view.last_content_origin = Some(Point {
                                x: bounds.origin.x + padding.left,
                                y: bounds.origin.y + padding.top,
                            });
                        });
                        measured_renderer.paint(
                            bounds,
                            padding,
                            &term,
                            window,
                            cx,
                            cursor_visible,
                            scroll_offset,
                        );
                        let paint_elapsed = paint_started.map(|started| started.elapsed());

                        let trace_report = trace.map(|(reason, started, entity_id, scale, measure_elapsed)| {
                            (reason, entity_id, scale, measure_elapsed, started.elapsed())
                        });

                        // 当前搜索命中的高亮覆盖层（网格坐标锚定，随滚动移动）
                        if search_open && let Some(match_) = current_match {
                            let grid = term.grid();
                            let history = grid.total_lines() - grid.screen_lines();
                            let display_offset = grid.display_offset() as i64;
                            let row = match_.line as i64 + display_offset - history as i64;
                            if row >= 0 && (row as usize) < grid.screen_lines() {
                                let origin_x = bounds.origin.x + padding.left;
                                let origin_y = bounds.origin.y + padding.top;
                                let x =
                                    origin_x + measured_renderer.cell_width * match_.column as f32;
                                let y = origin_y + measured_renderer.cell_height * row as f32;
                                window.paint_quad(gpui::quad(
                                    Bounds {
                                        origin: Point { x, y },
                                        size: Size {
                                            width: measured_renderer.cell_width * match_.len as f32,
                                            height: measured_renderer.cell_height,
                                        },
                                    },
                                    px(0.0),
                                    gpui::Hsla::from(gpui::rgb(0xE0AF68)).opacity(0.35),
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
                            measured_renderer.paint_preedit(
                                &term,
                                origin,
                                window,
                                cx,
                                &marked_text,
                                scroll_offset,
                            );
                        }
                        // 持锁完成后再输出，避免诊断 I/O 阻塞 PTY 读线程。
                        drop(term);
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
fn open_url(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/c", "start", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(program).args(args).spawn();
}

#[cfg(test)]
mod tests {
    use super::{TerminalConfig, TerminalExited, TerminalSession, TerminalView};
    use gpui::{AppContext, TestAppContext};
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
            fn backend(&self) -> BackendKind { BackendKind::Local }
            fn title(&self) -> String { String::new() }
            fn take_streams(&self) -> std::io::Result<SessionStreams> {
                Ok(SessionStreams { reader: Box::new(std::io::empty()), writer: Box::new(std::io::sink()) })
            }
            fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
                self.0.lock().unwrap().push((cols, rows));
                Ok(())
            }
            fn kill(&self) {}
        }
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let captured = sizes.clone();
        let (terminal, cx) = cx.add_window_view(|_, cx| TerminalView::new(Arc::new(RecordingSession(captured)), TerminalConfig::default(), cx));
        cx.run_until_parked();
        let grid = terminal.read_with(cx, |view, _| view.state.with_term(|term| {
            use alacritty_terminal::grid::Dimensions;
            (term.columns() as u16, term.screen_lines() as u16)
        }));
        sizes.lock().unwrap().clear();
        let captured = sizes.clone();
        let (_second, cx) = cx.add_window_view(|_, cx| {
            let config = TerminalConfig { cols: grid.0 as usize, rows: grid.1 as usize, ..TerminalConfig::default() };
            TerminalView::new(Arc::new(RecordingSession(captured)), config, cx)
        });
        cx.run_until_parked();
        assert_eq!(sizes.lock().unwrap().last().copied(), Some(grid), "首次布局必须同步会话网格");
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
}
