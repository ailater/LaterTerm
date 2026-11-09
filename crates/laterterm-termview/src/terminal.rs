//! Terminal state management.
//!
//! This module provides [`TerminalState`], a thread-safe wrapper around alacritty's
//! [`Term`] structure. It manages the terminal
//! emulator state, including the character grid, cursor position, and VTE parser.
//!
//! # Architecture
//!
//! `TerminalState` wraps the alacritty terminal in `Arc<Mutex<>>` to allow safe
//! concurrent access from:
//!
//! - The async reader task (writing bytes to the terminal)
//! - The render thread (reading the grid for display)
//! - The main thread (handling resize events)
//!
//! # VTE Parsing
//!
//! The terminal uses alacritty's VTE parser to process byte streams. When bytes
//! arrive from the PTY, they are fed through the parser via [`process_bytes`],
//! which calls handler methods on the `Term` to update the grid:
//!
//! ```text
//! PTY bytes → VTE Parser → Term handlers → Grid updates
//!                          ├─ print()     (regular characters)
//!                          ├─ execute()   (control chars: BEL, BS, etc.)
//!                          ├─ esc_dispatch()  (escape sequences)
//!                          └─ csi_dispatch()  (CSI sequences: colors, cursor, etc.)
//! ```
//!
//! # Example
//!
//! ```
//! use std::sync::mpsc::channel;
//! use gpui_terminal::event::GpuiEventProxy;
//! use gpui_terminal::terminal::TerminalState;
//!
//! let (tx, rx) = channel();
//! let event_proxy = GpuiEventProxy::new(tx);
//! let mut terminal = TerminalState::new(80, 24, 10_000, event_proxy);
//!
//! // Process some output (e.g., colored text)
//! terminal.process_bytes(b"\x1b[31mRed text\x1b[0m");
//! ```
//!
//! [`process_bytes`]: TerminalState::process_bytes

use crate::event::GpuiEventProxy;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use parking_lot::Mutex;
use std::sync::Arc;

/// 回滚搜索命中。
///
/// `line` 是「历史 + 屏幕」自顶向下的绝对行号（0 = 最老的历史行），
/// 与渲染时的 `Line(line.0 + history_size)` 对应；`column`/`len` 为网格列，
/// 宽字符按实际占用的列数计。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub line: usize,
    pub column: usize,
    pub len: usize,
}

/// 大小写不敏感的逐字符子串搜索（避免 UTF-8 字节索引在中文场景错位）。
/// URL 前缀（小写匹配，大小写不敏感）。
const URL_SCHEMES: [&str; 4] = ["http://", "https://", "ftp://", "www."];

/// URL 结束字符：空白 + 常见的包裹/终止符。
fn is_url_end_char(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | '|' | '`'
        )
}

/// 从行文本中提取包含指定列的 URL。
/// `cells` 是 (列号, 字符) 的映射，用于把字符串索引换算回网格列。
/// 从行文本中提取包含指定列的 URL，返回 `(url, 起始列, 结束列)`（结束列开区间）。
/// `cells` 是 (列号, 字符) 的映射，用于把字符串索引换算回网格列。
fn extract_url_span_containing(
    text: &str,
    cells: &[(usize, char)],
    click_col: usize,
) -> Option<(String, usize, usize)> {
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let raw: Vec<char> = text.chars().collect();

    for scheme in URL_SCHEMES {
        let sc: Vec<char> = scheme.chars().collect();
        let mut i = 0;
        while i + sc.len() <= lower.len() {
            if lower[i..i + sc.len()] == sc[..] {
                let start = i;
                let mut end = i + sc.len();
                while end < raw.len() && !is_url_end_char(raw[end]) {
                    end += 1;
                }
                while end > start && matches!(raw[end - 1], '.' | ',' | ';' | ':' | '!' | '?') {
                    end -= 1;
                }
                let start_col = cells.get(start).map(|(c, _)| *c).unwrap_or(usize::MAX);
                let end_col = cells
                    .get(end.saturating_sub(1))
                    .map(|(c, _)| c + 1)
                    .unwrap_or(0);
                if click_col >= start_col && click_col < end_col {
                    let url: String = raw[start..end].iter().collect();
                    let url = if scheme == "www." {
                        format!("https://{url}")
                    } else {
                        url
                    };
                    return Some((url, start_col, end_col));
                }
                i = end;
            } else {
                i += 1;
            }
        }
    }
    None
}

/// 大小写敏感可切换的行搜索（大小写不敏感时与 search_grid 同口径）。
fn search_grid_sensitivity(
    grid: &Grid<Cell>,
    needle: &[char],
    case_sensitive: bool,
) -> Vec<SearchMatch> {
    if case_sensitive {
        search_grid_exact(grid, needle)
    } else {
        search_grid(grid, needle)
    }
}

/// 大小写敏感的行搜索（不 lowercase）。
fn search_grid_exact(grid: &Grid<Cell>, needle: &[char]) -> Vec<SearchMatch> {
    let total_lines = grid.total_lines();
    let history = total_lines - grid.screen_lines();
    let columns = grid.columns();
    let mut matches = Vec::new();
    for absolute in 0..total_lines {
        let line = Line(absolute as i32 - history as i32);
        let mut text: Vec<char> = Vec::with_capacity(columns);
        let mut char_columns: Vec<usize> = Vec::with_capacity(columns);
        for column in 0..columns {
            let cell = &grid[AlacPoint::new(line, Column(column))];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            text.push(cell.c);
            char_columns.push(column);
        }
        let mut start = 0;
        while start + needle.len() <= text.len() {
            if text[start..start + needle.len()] == *needle {
                let first = char_columns[start];
                let last = char_columns[start + needle.len() - 1];
                let last_width = if grid[AlacPoint::new(line, Column(last))]
                    .flags
                    .contains(Flags::WIDE_CHAR)
                {
                    2
                } else {
                    1
                };
                matches.push(SearchMatch {
                    line: absolute,
                    column: first,
                    len: last + last_width - first,
                });
                start += needle.len();
            } else {
                start += 1;
            }
        }
    }
    matches
}

fn search_grid(grid: &Grid<Cell>, needle: &[char]) -> Vec<SearchMatch> {
    let total_lines = grid.total_lines();
    let history = total_lines - grid.screen_lines();
    let columns = grid.columns();
    let mut matches = Vec::new();
    for absolute in 0..total_lines {
        let line = Line(absolute as i32 - history as i32);
        // 行文本 + 每个字符所在列（宽字符续列跳过，由首列代表）
        let mut text: Vec<char> = Vec::with_capacity(columns);
        let mut char_columns: Vec<usize> = Vec::with_capacity(columns);
        for column in 0..columns {
            let cell = &grid[AlacPoint::new(line, Column(column))];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            text.push(cell.c);
            char_columns.push(column);
        }
        // 小写化可能一对多展开（如 'İ' → "i̇"），列映射必须与字符同步构建
        let mut haystack: Vec<char> = Vec::with_capacity(text.len());
        let mut haystack_columns: Vec<usize> = Vec::with_capacity(text.len());
        for (index, c) in text.iter().enumerate() {
            for lower in c.to_lowercase() {
                haystack.push(lower);
                haystack_columns.push(char_columns[index]);
            }
        }
        if haystack.len() < needle.len() {
            continue;
        }
        let mut start = 0;
        while start + needle.len() <= haystack.len() {
            if haystack[start..start + needle.len()] == *needle {
                let first = haystack_columns[start];
                let last = haystack_columns[start + needle.len() - 1];
                // 末字符可能占两列（宽字符），按其实际占宽计算命中列跨度
                let last_width = if grid[AlacPoint::new(line, Column(last))]
                    .flags
                    .contains(Flags::WIDE_CHAR)
                {
                    2
                } else {
                    1
                };
                matches.push(SearchMatch {
                    line: absolute,
                    column: first,
                    len: last + last_width - first,
                });
                start += needle.len();
            } else {
                start += 1;
            }
        }
    }
    matches
}

/// Simple dimensions implementation for terminal initialization.
struct TermDimensions {
    columns: usize,
    screen_lines: usize,
}

impl TermDimensions {
    fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        // For initial setup, total lines equals screen lines
        // The scrollback buffer will be managed by the Term itself
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }

    fn last_column(&self) -> alacritty_terminal::index::Column {
        alacritty_terminal::index::Column(self.columns.saturating_sub(1))
    }
}

/// Thread-safe terminal state wrapper.
///
/// This struct wraps alacritty's [`Term`] in an
/// `Arc<parking_lot::Mutex<>>` to allow safe concurrent access from multiple threads.
/// It also manages the VTE parser for processing incoming bytes from the PTY.
///
/// # Thread Safety
///
/// The terminal state can be safely shared across threads:
///
/// - Use [`term_arc`](Self::term_arc) to get a cloned `Arc` for sharing
/// - Use [`with_term`](Self::with_term) for read access to the grid
/// - Use [`with_term_mut`](Self::with_term_mut) for write access
///
/// The mutex is held only for the duration of the closure, minimizing contention.
///
/// # Grid Access
///
/// The terminal grid is accessed through the `Term` structure:
///
/// ```ignore
/// terminal_state.with_term(|term| {
///     let grid = term.grid();
///     let cursor = grid.cursor.point;
///     let cell = &grid[cursor];
///     // Read cell content, colors, flags, etc.
/// });
/// ```
///
/// # Performance Notes
///
/// - `parking_lot::Mutex` is used for faster locking than `std::sync::Mutex`
/// - Lock contention is minimized by keeping critical sections short
/// - The VTE parser state is kept outside the mutex (only accessed from one thread)
pub struct TerminalState {
    /// The underlying alacritty terminal emulator.
    term: Arc<Mutex<Term<GpuiEventProxy>>>,

    /// VTE parser for converting byte streams into terminal actions.
    parser: Processor,

    /// Number of columns in the terminal.
    cols: usize,

    /// Number of rows (lines) in the terminal.
    rows: usize,

    /// 数据版本号：每次 process_bytes 自增。渲染侧用它判断「有新数据未上屏」，
    /// 作为 flume 异步唤醒丢失时的兜底刷新依据。
    data_epoch: std::sync::atomic::AtomicU64,
}

impl TerminalState {
    /// Create a new terminal state with the given dimensions.
    ///
    /// # Arguments
    ///
    /// * `cols` - The number of columns (character width) of the terminal
    /// * `rows` - The number of rows (lines) of the terminal
    /// * `event_proxy` - The event proxy for forwarding terminal events to GPUI
    ///
    /// # Returns
    ///
    /// A new `TerminalState` instance.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::mpsc::channel;
    /// use gpui_terminal::event::GpuiEventProxy;
    /// use gpui_terminal::terminal::TerminalState;
    ///
    /// let (tx, rx) = channel();
    /// let event_proxy = GpuiEventProxy::new(tx);
    /// let terminal = TerminalState::new(80, 24, 10_000, event_proxy);
    /// ```
    pub fn new(cols: usize, rows: usize, scrollback: usize, event_proxy: GpuiEventProxy) -> Self {
        // Configure the history buffer before creating the terminal grids.
        let config = Config {
            scrolling_history: scrollback,
            ..Default::default()
        };

        // Create dimensions for terminal initialization
        let dimensions = TermDimensions::new(cols, rows);

        // Create the terminal with the given configuration and dimensions
        let term = Term::new(config, &dimensions, event_proxy);

        // Create the VTE parser for processing incoming bytes
        let parser = Processor::new();

        Self {
            term: Arc::new(Mutex::new(term)),
            parser,
            cols,
            rows,
            data_epoch: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 数据版本号：渲染侧用它检测「有新字节尚未上屏」。
    pub fn data_epoch(&self) -> u64 {
        self.data_epoch.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Process incoming bytes from the PTY.
    ///
    /// This method feeds the bytes through the VTE parser, which will call
    /// the appropriate handler methods on the terminal to update its state.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The bytes received from the PTY
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use gpui_terminal::event::GpuiEventProxy;
    /// # use gpui_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let mut terminal = TerminalState::new(80, 24, 10_000, event_proxy);
    /// // Process some output from the PTY
    /// terminal.process_bytes(b"Hello, world!\r\n");
    /// ```
    pub fn process_bytes(&mut self, bytes: &[u8]) {
        let mut term = self.term.lock();
        // The parser.advance method calls handler methods on the Term
        // The Term implements the Handler trait from the VTE crate
        self.parser.advance(&mut *term, bytes);
        self.data_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Resize the terminal to new dimensions.
    ///
    /// This method updates the terminal's internal grid to match the new size.
    /// It should be called when the terminal view is resized.
    ///
    /// # Arguments
    ///
    /// * `cols` - The new number of columns
    /// * `rows` - The new number of rows
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use gpui_terminal::event::GpuiEventProxy;
    /// # use gpui_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let mut terminal = TerminalState::new(80, 24, 10_000, event_proxy);
    /// // Resize to 120x30
    /// terminal.resize(120, 30);
    /// ```
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        self.cols = cols;
        self.rows = rows;

        let mut term = self.term.lock();

        // Create dimensions for the resize
        let dimensions = TermDimensions::new(cols, rows);

        // Resize the terminal
        term.resize(dimensions);
    }

    /// Update the scrollback history capacity of the active grid.
    pub fn set_scrollback(&mut self, scrollback: usize) {
        self.term.lock().grid_mut().update_history(scrollback);
    }

    /// Get the current terminal mode.
    ///
    /// The terminal mode affects how certain key sequences are interpreted,
    /// particularly arrow keys in application cursor mode.
    ///
    /// # Returns
    ///
    /// The current `TermMode` flags.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use gpui_terminal::event::GpuiEventProxy;
    /// # use gpui_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let terminal = TerminalState::new(80, 24, 10_000, event_proxy);
    /// use alacritty_terminal::term::TermMode;
    ///
    /// let mode = terminal.mode();
    /// if mode.contains(TermMode::APP_CURSOR) {
    ///     println!("Application cursor mode is enabled");
    /// }
    /// ```
    pub fn mode(&self) -> TermMode {
        let term = self.term.lock();
        *term.mode()
    }

    /// IME 锚点：光标在「当前视口」中的 (行, 列)。
    ///
    /// 预编辑浮层与输入法候选框都以它定位。行按显示偏移换算并钳进
    /// 视口（滚离屏幕时光标不可见，锚到最近的可见行，候选框不出屏）。
    pub fn ime_anchor(&self) -> (usize, usize) {
        self.with_term(|term| {
            let grid = term.grid();
            let display_offset = grid.display_offset() as i32;
            let row = (grid.cursor.point.line.0 + display_offset)
                .clamp(0, grid.screen_lines() as i32 - 1) as usize;
            (row, grid.cursor.point.column.0)
        })
    }

    /// 大小写不敏感地搜索整个回滚缓冲与可见屏幕。
    ///
    /// 命中按从最老历史行到屏幕底行的顺序返回；`SearchMatch::line` 使用
    /// 「历史+屏幕」自顶向下的绝对行号（0 = 最老一行），列号与列宽均为
    /// 网格列（宽字符占两列），可直接换算为像素坐标。
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use gpui_terminal::event::GpuiEventProxy;
    /// # use gpui_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let mut terminal = TerminalState::new(20, 4, 32, event_proxy);
    /// terminal.process_bytes(b"hello world\r\n");
    /// let matches = terminal.search("WORLD");
    /// assert_eq!(matches.len(), 1);
    /// assert_eq!(matches[0].column, 6);
    /// ```
    pub fn search(&self, query: &str) -> Vec<SearchMatch> {
        self.search_with(query, false)
    }

    /// 大小写敏感可切换的搜索（`case_sensitive=false` 与 [`Self::search`] 同口径）。
    pub fn search_with(&self, query: &str, case_sensitive: bool) -> Vec<SearchMatch> {
        let needle: Vec<char> = if case_sensitive {
            query.chars().collect()
        } else {
            query.to_lowercase().chars().collect()
        };
        if needle.is_empty() {
            return Vec::new();
        }
        self.with_term(|term| search_grid_sensitivity(term.grid(), &needle, case_sensitive))
    }

    /// 从网格中提取点击位置处的 URL。
    ///
    /// 提取指定网格位置命中的 URL 及其列范围（悬停下划线用）。
    /// 返回 `(url, 起始列, 结束列)`；无命中为 None。结束列为开区间。
    pub fn extract_url_span_at(&self, row: usize, col: usize) -> Option<(String, usize, usize)> {
        self.with_term(|term| {
            let grid = term.grid();
            let history = (grid.total_lines() - grid.screen_lines()) as i32;
            let display_offset = grid.display_offset() as i32;
            // 屏幕行 → grid 行：与 search_grid 同口径
            let absolute = history + row as i32 + display_offset;
            let line = Line(absolute - history);

            let columns = grid.columns();
            let mut cells: Vec<(usize, char)> = Vec::with_capacity(columns);
            for c in 0..columns {
                let cell = &grid[AlacPoint::new(line, Column(c))];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                cells.push((c, cell.c));
            }
            let text: String = cells.iter().map(|(_, ch)| *ch).collect();
            extract_url_span_containing(&text, &cells, col)
        })
    }

    /// `row` 是屏幕行（0 = 视口顶行），`col` 是网格列。
    /// 检测整个行文本中的 http/https/ftp/www 前缀，返回包含点击列的完整 URL。
    pub fn extract_url_at(&self, row: usize, col: usize) -> Option<String> {
        self.extract_url_span_at(row, col).map(|(url, _, _)| url)
    }

    /// Execute a function with read access to the terminal.
    ///
    /// This method provides safe read access to the underlying `Term` structure.
    /// The terminal is locked for the duration of the function call.
    ///
    /// # Arguments
    ///
    /// * `f` - A function that takes a reference to the `Term` and returns a value
    ///
    /// # Returns
    ///
    /// The value returned by the function `f`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use gpui_terminal::event::GpuiEventProxy;
    /// # use gpui_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let terminal = TerminalState::new(80, 24, 10_000, event_proxy);
    /// let cursor_pos = terminal.with_term(|term| {
    ///     term.grid().cursor.point
    /// });
    /// ```
    pub fn with_term<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Term<GpuiEventProxy>) -> R,
    {
        let term = self.term.lock();
        f(&term)
    }

    /// Execute a function with mutable access to the terminal.
    ///
    /// This method provides safe write access to the underlying `Term` structure.
    /// The terminal is locked for the duration of the function call.
    ///
    /// # Arguments
    ///
    /// * `f` - A function that takes a mutable reference to the `Term` and returns a value
    ///
    /// # Returns
    ///
    /// The value returned by the function `f`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use gpui_terminal::event::GpuiEventProxy;
    /// # use gpui_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let terminal = TerminalState::new(80, 24, 10_000, event_proxy);
    /// terminal.with_term_mut(|term| {
    ///     // Perform some mutation on the term
    ///     term.scroll_display(alacritty_terminal::grid::Scroll::Delta(5));
    /// });
    /// ```
    pub fn with_term_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Term<GpuiEventProxy>) -> R,
    {
        let mut term = self.term.lock();
        f(&mut term)
    }

    /// Get the number of columns in the terminal.
    ///
    /// # Returns
    ///
    /// The current number of columns.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Get the number of rows in the terminal.
    ///
    /// # Returns
    ///
    /// The current number of rows.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Get a cloned reference to the underlying terminal Arc.
    ///
    /// This allows sharing the terminal state across multiple threads or components.
    ///
    /// # Returns
    ///
    /// A cloned `Arc<Mutex<Term<GpuiEventProxy>>>`.
    pub fn term_arc(&self) -> Arc<Mutex<Term<GpuiEventProxy>>> {
        Arc::clone(&self.term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn test_terminal_creation() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, 10_000, event_proxy);

        assert_eq!(terminal.cols(), 80);
        assert_eq!(terminal.rows(), 24);
    }

    #[test]
    fn test_process_bytes() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(80, 24, 10_000, event_proxy);

        // Process some text
        terminal.process_bytes(b"Hello, world!");

        // Verify the text was written to the grid
        terminal.with_term(|term| {
            let grid = term.grid();
            // The text should be at the cursor position
            // We can't easily test the exact content without more complex grid inspection
            assert!(grid.columns() == 80);
        });
    }

    #[test]
    fn test_resize() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(80, 24, 10_000, event_proxy);

        terminal.resize(120, 30);

        assert_eq!(terminal.cols(), 120);
        assert_eq!(terminal.rows(), 30);

        terminal.resize(0, 0);
        assert_eq!(terminal.cols(), 1);
        assert_eq!(terminal.rows(), 1);

        terminal.with_term(|term| {
            let grid = term.grid();
            assert_eq!(grid.columns(), 1);
            assert_eq!(grid.screen_lines(), 1);
        });
    }

    #[test]
    fn test_grid_line_semantics_for_cursor_rendering() {
        // 锚定 grid Line 坐标语义: Line 自顶向下为正, Line(0) = 屏幕顶行。
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, event_proxy);

        // 全新终端(未输出内容)的光标在顶行行首 —— 与真实终端一致。
        terminal.with_term(|term| {
            let grid = term.grid();
            let cursor = grid.cursor.point;
            assert_eq!(cursor.line.0, 0, "初始光标应在顶行 Line(0)");
            assert_eq!(cursor.column.0, 0);
            assert_eq!(cursor.line.0 + grid.display_offset() as i32, 0);
        });

        // 输出两行,光标应紧跟最后写入的内容行。
        terminal.process_bytes(b"one\r\ntwo\r\n");

        terminal.with_term(|term| {
            let grid = term.grid();
            let cursor = grid.cursor.point;
            let display_offset = grid.display_offset() as i32;
            let cursor_line = cursor.line.0 + display_offset;
            assert!(
                (0..grid.screen_lines() as i32).contains(&cursor_line),
                "光标行 {cursor_line} 应在可见区: cursor.line={} display_offset={display_offset}",
                cursor.line.0
            );
            assert_eq!(cursor_line, 2, "两行输出后光标应在屏幕第 2 行");
            assert_eq!(cursor.column.0, 0);
            // 光标紧跟在最后写入行("two" 在第 1 行)之后的空行首
            let prev: String = (0..3)
                .map(|c| grid[AlacPoint::new(Line(cursor_line - 1), Column(c))].c)
                .collect();
            assert_eq!(prev, "two", "光标必须紧跟最后写入的内容行(其上一行)");
        });

        // 首帧 grow resize(4→8 行): 光标行号与内容行都必须保持不变
        // (alacritty 的 grow_lines 在 history 为空时内容留在原行),
        // 否则光标与内容分离 —— 即「打开时光标画在屏幕最前面」缺陷。
        terminal.resize(20, 8);
        terminal.with_term(|term| {
            let grid = term.grid();
            let display_offset = grid.display_offset() as i32;
            let cursor_line = grid.cursor.point.line.0 + display_offset;
            assert_eq!(cursor_line, 2, "grow resize 后光标行号必须保持不变");
            let prev: String = (0..3)
                .map(|c| grid[AlacPoint::new(Line(cursor_line - 1), Column(c))].c)
                .collect();
            assert_eq!(
                prev, "two",
                "resize 后内容与光标必须保持相对位置(光标上一行仍是最后写入行)"
            );
        });
    }

    #[test]
    fn shell_prompt_cursor_stays_after_prompt_when_rows_shrink() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 30, 100, GpuiEventProxy::new(tx));
        terminal.process_bytes(b"user@host:~$ ");
        terminal.resize(80, 20);
        terminal.with_term(|term| {
            assert_eq!(
                term.grid().cursor.point.column.0,
                13,
                "光标应位于提示符之后"
            );
            assert_eq!(
                term.grid().cursor.point.line.0,
                0,
                "首行提示符不应因缩小高度而移出可见区域"
            );
        });
    }

    #[test]
    fn clear_screen_empties_visible_area() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, 100, GpuiEventProxy::new(tx));
        terminal.process_bytes(b"stale output before clear");
        use alacritty_terminal::vte::ansi::Handler as _;
        terminal.with_term_mut(|term| {
            term.clear_screen(alacritty_terminal::vte::ansi::ClearMode::All);
        });
        terminal.with_term(|term| {
            use alacritty_terminal::grid::Dimensions;
            let grid = term.grid();
            for row in 0..grid.screen_lines() {
                for column in 0..grid.columns() {
                    let cell = grid[alacritty_terminal::index::Line(row as i32)]
                        [alacritty_terminal::index::Column(column)]
                    .c;
                    assert_eq!(cell, ' ', "清屏后可见区应全空");
                }
            }
        });
    }

    #[test]
    fn selection_stored_on_term_roundtrips_text() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, 100, GpuiEventProxy::new(tx));
        terminal.process_bytes(b"hello selection\r\n$ ");
        terminal.with_term_mut(|term| {
            use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side};
            use alacritty_terminal::selection::{
                Selection as AlacSelection, SelectionType as AlacSelectionType,
            };
            let mut selection = AlacSelection::new(
                AlacSelectionType::Simple,
                AlacPoint::new(Line(0), Column(0)),
                Side::Left,
            );
            selection.update(AlacPoint::new(Line(0), Column(4)), Side::Right);
            term.selection = Some(selection);
            assert_eq!(term.selection_to_string().as_deref(), Some("hello"));
        });
    }

    #[test]
    fn test_scrollback_and_display_offset() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, event_proxy);

        for line in 0..40 {
            terminal.process_bytes(format!("line-{line}\r\n").as_bytes());
        }
        terminal.with_term(|term| {
            assert!(term.grid().total_lines() - term.grid().screen_lines() >= 32);
            assert_eq!(term.grid().display_offset(), 0);
        });

        terminal.with_term_mut(|term| {
            term.scroll_display(alacritty_terminal::grid::Scroll::Delta(2));
        });
        terminal.with_term(|term| assert_eq!(term.grid().display_offset(), 2));

        terminal.with_term_mut(|term| {
            term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        });
        terminal.with_term(|term| assert_eq!(term.grid().display_offset(), 0));
    }

    #[test]
    fn test_ime_anchor_follows_cursor_and_clamps_when_scrolled() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, event_proxy);
        terminal.process_bytes(b"abc");

        // 光标停在第一行列 3
        assert_eq!(terminal.ime_anchor(), (0, 3));

        // 满屏输出后光标在末行；上滚 2 行时锚点钳回视口内
        for line in 0..40 {
            terminal.process_bytes(format!("line-{line}\r\n").as_bytes());
        }
        terminal.with_term(|term| {
            assert_eq!(term.grid().display_offset(), 0);
        });
        let (row, _) = terminal.ime_anchor();
        assert!(row < 4, "未滚动时锚点应在视口内: {row}");

        terminal.with_term_mut(|term| {
            term.scroll_display(alacritty_terminal::grid::Scroll::Delta(2));
        });
        let (row, _) = terminal.ime_anchor();
        assert!(row < 4, "滚动后锚点仍应钳在视口内: {row}");
    }

    #[test]
    fn test_extract_url_from_grid() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(60, 4, 32, event_proxy);
        terminal.process_bytes(b"See https://example.com/page\r\n");

        // 先验证 search 能找到 URL（确保文本在网格里）
        let matches = terminal.search("https");
        assert!(!matches.is_empty(), "search 应找到 https");

        // 再测 URL 提取——用 search 返回的列作为点击列
        let m = &matches[0];
        let url = terminal.extract_url_at(0, m.column + 2);
        assert_eq!(
            url.as_deref(),
            Some("https://example.com/page"),
            "点击列={}→",
            m.column + 2
        );
    }

    #[test]
    fn test_extract_url_trailing_punctuation() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(60, 4, 32, event_proxy);
        terminal.process_bytes(b"Link: https://example.com.,\r\n");
        let matches = terminal.search("https");
        assert!(!matches.is_empty());
        let url = terminal.extract_url_at(0, matches[0].column + 2);
        assert_eq!(url.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn test_extract_url_www_prefix() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(60, 4, 32, event_proxy);
        terminal.process_bytes(b"Visit www.example.com today\r\n");
        let matches = terminal.search("www");
        assert!(!matches.is_empty());
        let url = terminal.extract_url_at(0, matches[0].column + 2);
        assert_eq!(url.as_deref(), Some("https://www.example.com"));
    }

    #[test]
    fn test_extract_url_none_in_plain_text() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, event_proxy);
        terminal.process_bytes("中文内容没有链接\r\nplain text line\r\n".as_bytes());
        assert!(terminal.extract_url_at(0, 2).is_none());
        assert!(terminal.extract_url_at(1, 5).is_none());
    }

    #[test]
    fn test_search_visible_line_and_column() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, event_proxy);
        terminal.process_bytes(b"hello world\r\n");

        let matches = terminal.search("WORLD");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].column, 6);
        assert_eq!(matches[0].len, 5);

        // 空查询无命中
        assert!(terminal.search("").is_empty());
        assert!(terminal.search("zzz").is_empty());
    }

    #[test]
    fn test_search_scrollback_ordering() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, event_proxy);
        for line in 0..40 {
            terminal.process_bytes(format!("line-{line}\r\n").as_bytes());
        }

        let matches = terminal.search("line-3");
        // 回滚容量 32：line-3 已被挤出历史，只剩 line-30..line-39 共 10 处，
        // 自顶向下（最老在前）排列
        assert_eq!(matches.len(), 10);
        let texts: Vec<String> = matches
            .iter()
            .map(|m| {
                terminal.with_term(|term| {
                    let grid = term.grid();
                    let history = grid.total_lines() - grid.screen_lines();
                    (0..5)
                        .map(|offset| {
                            grid[AlacPoint::new(
                                Line(m.line as i32 - history as i32),
                                Column(m.column + offset),
                            )]
                            .c
                            .to_string()
                        })
                        .collect()
                })
            })
            .collect();
        // 第一个命中应是最早输出的 line-3，其后是 line-30 起步的十个
        assert_eq!(texts[0], "line-");
        assert!(matches.windows(2).all(|w| w[0].line <= w[1].line));
    }

    #[test]
    fn test_search_cjk_text() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, event_proxy);
        terminal.process_bytes("中文搜索测试\r\n第二行 中文\n".as_bytes());

        let matches = terminal.search("中文");
        assert_eq!(matches.len(), 2);
        // 每个汉字占两列：第二行命中从列 4 开始（"第二行 " = 3 汉字 6 列 + 1 空格）
        assert_eq!(matches[1].column, 7);
        assert_eq!(matches[1].len, 4);
    }

    #[test]
    fn test_search_matches_on_same_line() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, event_proxy);
        terminal.process_bytes(b"ab ab ab\n");

        let matches = terminal.search("ab");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].column, 0);
        assert_eq!(matches[1].column, 3);
        assert_eq!(matches[2].column, 6);
    }

    #[test]
    fn test_mode() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, 10_000, event_proxy);

        let mode = terminal.mode();
        // Mode should be a valid TermMode value (just verify we can get it)
        let _bits = mode.bits();
    }

    #[test]
    fn test_with_term() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, 10_000, event_proxy);

        let cols = terminal.with_term(|term| term.grid().columns());
        assert_eq!(cols, 80);
    }

    #[test]
    fn test_with_term_mut() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, 10_000, event_proxy);

        terminal.with_term_mut(|term| {
            // Just verify we can get mutable access
            let _grid = term.grid_mut();
        });
    }

    #[test]
    fn test_term_arc() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, 10_000, event_proxy);

        let arc1 = terminal.term_arc();
        let arc2 = terminal.term_arc();

        // Both Arcs should point to the same terminal
        assert!(Arc::ptr_eq(&arc1, &arc2));
    }
}
