//! Terminal rendering module.
//!
//! This module provides [`TerminalRenderer`], which handles efficient rendering of
//! terminal content using GPUI's text and drawing systems.
//!
//! # Rendering Pipeline
//!
//! The renderer processes the terminal grid in several stages:
//!
//! ```text
//! Terminal Grid → Layout Phase → Paint Phase
//!                      │              │
//!                      ├─ Collect backgrounds
//!                      ├─ Batch text runs
//!                      │              │
//!                      │              ├─ Paint default background
//!                      │              ├─ Paint non-default backgrounds
//!                      │              ├─ Paint text characters
//!                      │              └─ Paint cursor
//! ```
//!
//! # Optimizations
//!
//! The renderer includes several optimizations to minimize draw calls:
//!
//! 1. **Background Merging**: Adjacent cells with the same background color are
//!    merged into single rectangles, reducing the number of quads to paint.
//!
//! 2. **Text Batching**: Adjacent cells with identical styling (color, bold, italic)
//!    are grouped into [`BatchedTextRun`]s for efficient text shaping.
//!
//! 3. **Default Background Skip**: Cells with the default background color don't
//!    generate separate background rectangles.
//!
//! 4. **Cell Measurement**: Font metrics are measured once using the '│' (BOX DRAWINGS
//!    LIGHT VERTICAL) character and cached for consistent cell dimensions.
//!
//! # Cell Dimensions
//!
//! Cell size is calculated from actual font metrics using the '│' character,
//! which spans the full cell height in properly designed terminal fonts:
//!
//! - **Width**: Measured from shaped '│' character
//! - **Height**: `(ascent + descent) × line_height_multiplier`
//!
//! The `line_height_multiplier` (default 1.0) can be adjusted to add extra
//! vertical space if needed for specific fonts.
//!
//! # Example
//!
//! ```ignore
//! use gpui::px;
//! use laterterm_termview::{ColorPalette, CursorStyle, TerminalRenderer};
//!
//! let renderer = TerminalRenderer::new(
//!     "JetBrains Mono".to_string(),
//!     px(14.0),
//!     1.0,  // line height multiplier
//!     ColorPalette::default(),
//!     true,      // 连字
//!     Vec::new(), // 备选字体链
//!     CursorStyle::Block,
//! );
//! ```

use crate::box_drawing;
use crate::colors::ColorPalette;
use crate::event::GpuiEventProxy;
use crate::view::CursorStyle;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::Color;
use gpui::{
    App, Bounds, Edges, Font, FontFallbacks, FontFeatures, FontStyle, FontWeight, Hsla, Pixels,
    Point, SharedString, Size, TextRun, UnderlineStyle, Window, px, quad, transparent_black,
};

/// 一次绘制期的可见网格快照。
///
/// [`FrameSnapshot::capture`] 在锁内只做可见区逐格复制与字段读取（含选区换算），
/// 调用方拿到返回值即可释放锁；网格绘制与搜索/选区/预编辑覆盖层全部以本快照为
/// 数据源，保证 cells 与 selection/display_offset/cursor 天然同帧（无撕裂）。
///
/// 复制成本 ≈ `size_of::<Cell>() × 可见格数`（约 24B/格；120×40 ≈ 115KB，
/// 4K 满屏 ≈ 1.8MB）。实测（release、228×98=22344 格、隔离测试窗口）锁内
/// 复制中位 0.114ms，为快照前整段持锁绘制（0.531ms）的 21.5%。已知代价：
/// 两阶段「先全网格复制、再逐行消费」的工作集（≈2×快照体积）大于快照前
/// 「收集一行→立刻消费一行」的行热模式，隔离基准整帧 +12%（约 +65µs，
/// 〈1% vsync 帧预算）；备选的「锁内布局、锁外整形」形态实测锁窗口 0.29ms
/// （复制之外 layout 本身约 0.17ms）而整帧无统计显著改善，故保留本形态。
/// 详见 docs/roadmap.md M1 注记（2026-11-07，非基准承诺）。
#[derive(Clone)]
pub struct FrameSnapshot {
    /// 可见区逐格副本：`cells[视口行][列]`。
    pub cells: Vec<Vec<Cell>>,
    /// 网格列数。
    pub columns: usize,
    /// 可见行数。
    pub screen_lines: usize,
    /// 总行数（含回滚历史）。
    pub total_lines: usize,
    /// 显示偏移（向上回滚的行数）。
    pub display_offset: usize,
    /// 动态颜色覆盖（OSC 4/10/11 等写入的调色板快照）。
    pub colors: Colors,
    /// 终端模式位（光标可见性等）。
    pub mode: TermMode,
    /// 光标位置（网格坐标）。
    pub cursor: AlacPoint,
    /// 选区范围（与 cells 同一次持锁内换算，同帧一致）。
    pub selection: Option<SelectionRange>,
}

impl Default for FrameSnapshot {
    fn default() -> Self {
        Self {
            cells: Vec::new(),
            columns: 0,
            screen_lines: 0,
            total_lines: 0,
            display_offset: 0,
            colors: Colors::default(),
            mode: TermMode::NONE,
            cursor: AlacPoint::new(Line(0), Column(0)),
            selection: None,
        }
    }
}

impl FrameSnapshot {
    /// 在**持有终端锁**期间捕获可见网格快照；拿到返回值后调用方应尽快释放锁。
    ///
    /// 行坐标公式与既有绘制路径逐字一致：可见行 `line_idx` →
    /// `Line(line_idx - display_offset)`。
    pub fn capture(term: &Term<GpuiEventProxy>) -> Self {
        let mut out = Self::default();
        Self::capture_into(term, &mut out);
        out
    }

    /// 把可见网格快照写入 `out`，**复用其已有 rows/row 容量**（稳态帧零分配）。
    ///
    /// 与 [`Self::capture`] 等价；区别仅在于复用调用方提供的缓冲（缓冲池模式：
    /// 帧尾归还、下帧取用）。网格 resize 导致行/列数变化时，多出的行缓冲被
    /// 截断、不足的部分重新分配——resize 是低频事件，稳态帧全部命中复用路径。
    pub fn capture_into(term: &Term<GpuiEventProxy>, out: &mut Self) {
        let grid = term.grid();
        let columns = grid.columns();
        let screen_lines = grid.screen_lines();
        let total_lines = grid.total_lines();
        let display_offset = grid.display_offset();
        if out.cells.len() > screen_lines {
            out.cells.truncate(screen_lines);
        }
        for line_idx in 0..screen_lines {
            let line = Line(line_idx as i32 - display_offset as i32);
            match out.cells.get_mut(line_idx) {
                Some(row) => {
                    row.clear();
                    if row.capacity() < columns {
                        row.reserve(columns);
                    }
                    for col_idx in 0..columns {
                        row.push(grid[AlacPoint::new(line, Column(col_idx))].clone());
                    }
                }
                None => {
                    let mut row = Vec::with_capacity(columns);
                    for col_idx in 0..columns {
                        row.push(grid[AlacPoint::new(line, Column(col_idx))].clone());
                    }
                    out.cells.push(row);
                }
            }
        }
        out.columns = columns;
        out.screen_lines = screen_lines;
        out.total_lines = total_lines;
        out.display_offset = display_offset;
        // 选区必须在同一次持锁内对同一份 Term 换算：cells 与 selection 同帧，
        // 与快照前「锁贯穿 paint+overlay」的一致性等价。
        out.selection = term.selection.as_ref().and_then(|sel| sel.to_range(term));
        out.colors = *term.colors();
        out.mode = *term.mode();
        out.cursor = grid.cursor.point;
    }
}

/// 搜索命中高亮矩形：把回滚搜索命中换算为视口内像素矩形（纯函数，可测）。
///
/// `match_line` 是「历史+屏幕」自顶向下的绝对行号（0 = 最老历史行，同
/// [`crate::terminal::SearchMatch::line`]）；命中滚出视口（上方或下方）返回
/// `None`。数据源必须来自与网格同帧的快照字段，滚动到历史中部时命中框才不错行。
#[allow(clippy::too_many_arguments)] // 纯函数逐项传参，与调用点公式一一对应
pub fn search_match_rect(
    match_line: usize,
    match_column: usize,
    match_len: usize,
    display_offset: usize,
    history_lines: usize,
    screen_lines: usize,
    content_origin: Point<Pixels>,
    cell_width: Pixels,
    cell_height: Pixels,
) -> Option<Bounds<Pixels>> {
    let row = match_line as i64 + display_offset as i64 - history_lines as i64;
    if row < 0 || row as usize >= screen_lines {
        return None;
    }
    let x = content_origin.x + cell_width * match_column as f32;
    let y = content_origin.y + cell_height * row as f32;
    Some(Bounds {
        origin: Point { x, y },
        size: Size {
            width: cell_width * match_len as f32,
            height: cell_height,
        },
    })
}

/// 选区高亮的逐行区间 `(视口行, 起始列, 结束列[不含])`（纯函数，可测）。
///
/// 只返回落在视口内的行；首末行按选区端点裁剪列，中间行整行覆盖。
/// `display_offset`/`columns`/`screen_lines` 应取自与网格同帧的快照字段。
pub fn selection_row_spans(
    selection: SelectionRange,
    display_offset: usize,
    columns: usize,
    screen_lines: usize,
) -> Vec<(usize, usize, usize)> {
    let mut spans = Vec::new();
    for screen_row in 0..screen_lines as i64 {
        let line = screen_row - display_offset as i64;
        if line < selection.start.line.0 as i64 || line > selection.end.line.0 as i64 {
            continue;
        }
        let start_col = if line == selection.start.line.0 as i64 {
            selection.start.column.0
        } else {
            0
        };
        let end_col = if line == selection.end.line.0 as i64 {
            (selection.end.column.0 + 1).min(columns)
        } else {
            columns
        };
        if end_col <= start_col {
            continue;
        }
        spans.push((screen_row as usize, start_col, end_col));
    }
    spans
}

/// 预编辑行的视口换算：光标行按显示偏移平移并钳进视口（纯函数，可测）。
///
/// 滚离屏幕时光标不可见，锚到最近的可见行，预编辑浮层与候选框不出屏。
pub fn preedit_row(cursor_line: i32, display_offset: i32, screen_lines: usize) -> usize {
    (cursor_line + display_offset).clamp(0, screen_lines as i32 - 1) as usize
}

/// 单个字符在终端网格中占用的列数（东亚宽字符 2 列）。
///
/// 覆盖 CJK 常用区段即可满足中文输入法场景；与网格渲染的 WIDE_CHAR
/// 语义一致（中文场景足够，不追求完整 unicode-width 表）。
pub fn char_columns(ch: char) -> usize {
    let c = ch as u32;
    let wide = (0x1100..=0x115F).contains(&c)        // 谚文兼容字母
        || (0x2E80..=0xA4CF).contains(&c)   // CJK 部首..彝文
        || (0xAC00..=0xD7A3).contains(&c)   // 谚文音节
        || (0xF900..=0xFAFF).contains(&c)   // CJK 兼容表意
        || (0xFE30..=0xFE4F).contains(&c)   // CJK 兼容形式
        || (0xFF00..=0xFF60).contains(&c)   // 全角形式
        || (0xFFE0..=0xFFE6).contains(&c)
        || (0x20000..=0x3FFFD).contains(&c); // CJK 扩展
    if wide { 2 } else { 1 }
}

/// 预编辑文本的网格布局：每个字符 → (起始列, 字符, 占列数)。
///
/// 从光标列 `start_col` 起横向排布，行尾放不下即截断（候选框仍锚定在
/// 已排布区域，符合终端 IME 的常见呈现）。纯函数，无窗口依赖，可测。
pub fn preedit_layout(text: &str, start_col: usize, columns: usize) -> Vec<(usize, char, usize)> {
    let mut placed = Vec::new();
    let mut col = start_col.min(columns);
    for ch in text.chars() {
        let width = char_columns(ch);
        if col + width > columns {
            break;
        }
        placed.push((col, ch, width));
        col += width;
    }
    placed
}

/// 按光标样式计算光标矩形的几何（纯函数，无窗口依赖，可测）。
///
/// - Block：占满整格（历史行为不变，quad 盖字的现状保持）；
/// - Bar：光标列左缘的竖条，高度为整格；
/// - Underline：贴格底的横条，宽度为整格。
///
/// 细条厚度按单元格尺寸取比例（格宽 15% / 格高 10%）并用 `.max(px(1.0))`
/// 兜底：纯几何随字体度量缩放，4K 缩放下按比例不失真，也不会薄到不可见。
pub fn cursor_rect(
    style: CursorStyle,
    cursor_x: Pixels,
    cursor_y: Pixels,
    cell_width: Pixels,
    cell_height: Pixels,
) -> Bounds<Pixels> {
    match style {
        CursorStyle::Block => Bounds {
            origin: Point {
                x: cursor_x,
                y: cursor_y,
            },
            size: Size {
                width: cell_width,
                height: cell_height,
            },
        },
        CursorStyle::Bar => Bounds {
            origin: Point {
                x: cursor_x,
                y: cursor_y,
            },
            size: Size {
                width: (cell_width * 0.15).max(px(1.0)),
                height: cell_height,
            },
        },
        CursorStyle::Underline => {
            let thickness = (cell_height * 0.1).max(px(1.0));
            Bounds {
                origin: Point {
                    x: cursor_x,
                    y: cursor_y + cell_height - thickness,
                },
                size: Size {
                    width: cell_width,
                    height: thickness,
                },
            }
        }
    }
}

/// 特殊下划线（双线/点线/虚线）的 quad 矩形序列（纯函数，无窗口依赖可测）。
///
/// - 线厚统一取 `t = (cell_height * 0.07).max(px(1.0))`（比例先例同
///   `cursor_rect`：随字体度量缩放，1px 兜底不失真不可见）；
/// - Double：两条 t 厚横线，中间留 t 间隙（贴行底）；
/// - Dotted：点段按 cell 宽切分——每格两个点（点/空各 cell 宽的 1/4，
///   不小于 t 兜底），按 run 宽度截尾；
/// - Dashed：长段与空比 2:1（段 cell 宽的 1/2、空 1/4），按 run 宽度截尾。
///
/// `x`/`width` 为 run 的水平范围，`row_bottom` 为行底 y（覆盖层向上生长）。
pub fn special_underline_quads(
    kind: UnderlineKind,
    x: Pixels,
    row_bottom: Pixels,
    width: Pixels,
    cell_width: Pixels,
    cell_height: Pixels,
) -> Vec<Bounds<Pixels>> {
    let t = (cell_height * 0.07).max(px(1.0));
    // 点/段的水平切分以 cell 宽为参照（每格两点/一段的节奏），1px 兜底
    let dot = (cell_width * 0.25).max(t);
    let dash = (cell_width * 0.5).max(t * 2.0);
    let mut rects = Vec::new();
    match kind {
        UnderlineKind::Double => {
            // 两条 t 厚横线：上线 [bottom-3t, bottom-2t]、下线 [bottom-t, bottom]
            for offset in [2.0, 0.0] {
                rects.push(Bounds {
                    origin: Point {
                        x,
                        y: row_bottom - t * offset - t,
                    },
                    size: Size { width, height: t },
                });
            }
        }
        UnderlineKind::Dotted => {
            // 点/空各 dot 宽交替，按 run 宽度截尾
            let mut cursor = x;
            let end = x + width;
            while cursor < end {
                let dot_width = (end - cursor).min(dot);
                rects.push(Bounds {
                    origin: Point {
                        x: cursor,
                        y: row_bottom - t,
                    },
                    size: Size {
                        width: dot_width,
                        height: t,
                    },
                });
                cursor = cursor + dot + dot;
            }
        }
        UnderlineKind::Dashed => {
            // 段 dash、空 dash/2（2:1），按 run 宽度截尾
            let mut cursor = x;
            let end = x + width;
            while cursor < end {
                let dash_width = (end - cursor).min(dash);
                rects.push(Bounds {
                    origin: Point {
                        x: cursor,
                        y: row_bottom - t,
                    },
                    size: Size {
                        width: dash_width,
                        height: t,
                    },
                });
                cursor = cursor + dash + dash / 2.0;
            }
        }
        // 直线/波浪/无：不走 quad 路径
        UnderlineKind::None | UnderlineKind::Straight | UnderlineKind::Wavy => {}
    }
    rects
}

/// A batched run of text with consistent styling.
///
/// This struct groups adjacent terminal cells with identical visual attributes
/// to reduce the number of text rendering calls.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchedTextRun {
    /// The text content to render
    pub text: String,

    /// Starting column position
    pub start_col: usize,

    /// Exclusive ending column, including wide-character spacer columns.
    pub end_col: usize,

    /// Row position
    pub row: usize,

    /// Foreground color
    pub fg_color: Hsla,

    /// Background color
    pub bg_color: Hsla,

    /// Bold flag
    pub bold: bool,

    /// Italic flag
    pub italic: bool,

    /// 下划线样式（SGR 4:0~4:5，含彩色下划线颜色）
    pub underline: UnderlineKind,

    /// 下划线颜色（SGR 58 指定；None = 回退前景色）
    pub underline_color: Option<Hsla>,
}

/// 下划线样式（SGR 4:0~4:5）。渲染路径分两档：
/// - Straight/Wavy：gpui `TextRun.underline` 字形下划线（GPU Underline
///   primitive 原生渲染，wavy 开波浪），零额外 quad；
/// - Double/Dotted/Dashed：gpui 无原生样式，走逐帧 quad 覆盖层
///   （quad 数与 run 数成正比而非逐格，正常 TUI 输出 4:x 罕见）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnderlineKind {
    /// 无下划线（SGR 4:0 / 未设置）。
    None,
    /// 单直线（SGR 4:1）。
    Straight,
    /// 双线（SGR 4:2）。
    Double,
    /// 波浪线（SGR 4:3）。
    Wavy,
    /// 点线（SGR 4:4）。
    Dotted,
    /// 虚线（SGR 4:5，2:1 长段）。
    Dashed,
}

/// Background rectangle to paint.
///
/// Represents a rectangular region with a solid color background.
#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundRect {
    /// Starting column position
    pub start_col: usize,

    /// Ending column position (exclusive)
    pub end_col: usize,

    /// Row position
    pub row: usize,

    /// Background color
    pub color: Hsla,
}

impl BackgroundRect {
    /// Check if this rectangle can be merged with another.
    ///
    /// Two rectangles can be merged if they:
    /// - Are on the same row
    /// - Have the same color
    /// - Are horizontally adjacent
    fn can_merge_with(&self, other: &Self) -> bool {
        self.row == other.row && self.color == other.color && self.end_col == other.start_col
    }
}

/// Terminal renderer with font settings and cell dimensions.
///
/// This struct manages the rendering of terminal content, including text,
/// backgrounds, and cursor. It maintains font metrics and provides the
/// [`paint`](Self::paint) method for drawing the terminal grid.
///
/// # Font Metrics
///
/// Cell dimensions are calculated from actual font measurements via
/// [`measure_cell`](Self::measure_cell). This ensures accurate character
/// positioning regardless of the font used.
///
/// # Usage
///
/// The renderer is typically used internally by [`TerminalView`](crate::TerminalView),
/// but can also be used directly for custom rendering:
///
/// ```ignore
/// // Measure cell dimensions (call once per font change)
/// renderer.measure_cell(window);
///
/// // Paint the terminal grid
/// renderer.paint(bounds, padding, &term, window, cx);
/// ```
///
/// # Performance
///
/// For optimal performance:
/// - Call `measure_cell` only when font settings change
/// - The `paint` method is designed to be called every frame
/// - Background and text batching minimize GPU draw calls
#[derive(Clone)]
pub struct TerminalRenderer {
    /// Font family name (e.g., "Fira Code", "Menlo")
    pub font_family: String,

    /// Font size in pixels
    pub font_size: Pixels,

    /// Width of a single character cell
    pub cell_width: Pixels,

    /// Height of a single character cell (line height)
    pub cell_height: Pixels,

    /// Multiplier for line height to accommodate tall glyphs
    pub line_height_multiplier: f32,

    /// 是否启用连字（calt）；关闭后 `->` 等按单字符渲染
    pub ligatures: bool,

    /// 备选字体链：主字体缺字时按序回退（如 CJK）；空表示系统默认回退
    pub font_fallbacks: Vec<String>,

    /// 光标形状：方块 / 竖线 / 下划线；默认方块（历史行为）
    pub cursor_style: CursorStyle,

    /// Color palette for resolving terminal colors
    pub palette: ColorPalette,

    /// 背景不透明度（0.15~1.0）。1.0 = 历史行为（default 底色满幅绘制）；
    /// <1.0 时 default 底色跳绘（窗口底色由宿主层承载），单元格背景 quad 在
    /// 绘制时按 alpha 缩放——合并（layout_row/merge_backgrounds）仍按原色
    /// 相等比较，缩放只在 paint 时乘一次。
    pub background_opacity: f32,

    /// Whether cell metrics need to be measured again.
    needs_measurement: bool,
}

impl TerminalRenderer {
    /// Creates a new terminal renderer with the given font settings and color palette.
    ///
    /// # Arguments
    ///
    /// * `font_family` - The name of the font family to use
    /// * `font_size` - The font size in pixels
    /// * `line_height_multiplier` - Multiplier for line height (e.g., 1.2 for 20% extra)
    /// * `palette` - The color palette to use for terminal colors
    /// * `ligatures` - Whether to enable ligatures (calt)
    /// * `font_fallbacks` - Fallback fonts for missing glyphs (e.g., CJK)
    /// * `cursor_style` - Cursor shape: block, bar, or underline
    ///
    /// # Returns
    ///
    /// A new `TerminalRenderer` instance with default cell dimensions.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use gpui::px;
    /// use laterterm_termview::render::TerminalRenderer;
    /// use laterterm_termview::{ColorPalette, CursorStyle};
    ///
    /// let renderer = TerminalRenderer::new(
    ///     "Fira Code".to_string(),
    ///     px(14.0),
    ///     1.0,
    ///     ColorPalette::default(),
    ///     true,
    ///     Vec::new(),
    ///     CursorStyle::Block,
    /// );
    /// ```
    pub fn new(
        font_family: String,
        font_size: Pixels,
        line_height_multiplier: f32,
        palette: ColorPalette,
        ligatures: bool,
        font_fallbacks: Vec<String>,
        cursor_style: CursorStyle,
    ) -> Self {
        // Default cell dimensions - will be measured on first paint
        // Using 0.6 as approximate em-width ratio for monospace fonts
        let cell_width = font_size * 0.6;
        let cell_height = font_size * 1.4; // Line height with some spacing

        Self {
            font_family,
            font_size,
            cell_width,
            cell_height,
            line_height_multiplier,
            ligatures,
            font_fallbacks,
            cursor_style,
            palette,
            // 默认完全不透明（历史行为）；宿主经 pub 字段按需覆写
            background_opacity: 1.0,
            needs_measurement: true,
        }
    }

    /// 按样式构造字体：应用连字开关与备选字体链。
    ///
    /// 渲染器所有 Font 构造点（测量/批量文本）都走这里，保证设置一致。
    pub fn font_for(&self, bold: bool, italic: bool) -> Font {
        Font {
            family: self.font_family.clone().into(),
            features: if self.ligatures {
                FontFeatures::default()
            } else {
                FontFeatures::disable_ligatures()
            },
            fallbacks: if self.font_fallbacks.is_empty() {
                None
            } else {
                Some(FontFallbacks::from_fonts(self.font_fallbacks.clone()))
            },
            weight: if bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            style: if italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            },
        }
    }

    /// Whether the renderer needs fresh font metrics.
    pub fn needs_measurement(&self) -> bool {
        self.needs_measurement
    }

    /// Mark the cached cell metrics as stale.
    pub fn invalidate_measurement(&mut self) {
        self.needs_measurement = true;
    }

    /// Measure cell dimensions based on actual font metrics.
    ///
    /// This method measures the actual width and height of characters
    /// using the GPUI text system. It uses the '│' (BOX DRAWINGS LIGHT VERTICAL)
    /// character which spans the full cell height in properly designed terminal fonts.
    ///
    /// # Arguments
    ///
    /// * `window` - The GPUI window for text system access
    pub fn measure_cell(&mut self, window: &mut Window) {
        // Measure using '│' (U+2502, BOX DRAWINGS LIGHT VERTICAL)
        // This character spans the full cell height in terminal fonts, making it
        // ideal for measuring exact cell dimensions used by TUIs
        let font = self.font_for(false, false);

        let text_run = TextRun {
            len: "│".len(),
            font,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        // Shape the box-drawing character to get cell metrics
        let shaped = window
            .text_system()
            .shape_line("│".into(), self.font_size, &[text_run], None);

        // Get the width from the shaped line
        if shaped.width > px(0.0) {
            self.cell_width = shaped.width;
        }

        // Calculate height from ascent + descent with optional multiplier
        let line_height = (shaped.ascent + shaped.descent).ceil();
        if line_height > px(0.0) {
            self.cell_height = line_height * self.line_height_multiplier.max(0.1);
        }
        self.needs_measurement = false;
    }

    /// Layout cells into batched text runs and background rects for a single row.
    ///
    /// This method processes a row of terminal cells and groups adjacent cells
    /// with identical styling into batched runs. It also collects background
    /// rectangles that need to be painted.
    ///
    /// # Arguments
    ///
    /// * `row` - The row number
    /// * `cells` - Iterator over (column, &Cell) pairs（借用版：直接引用
    ///   [`FrameSnapshot`] 快照行，消除旧值迭代对每格的第二次 clone；
    ///   签名自快照改造起由 `(usize, Cell)` 有意变更为 `(usize, &Cell)`，
    ///   工作区内唯一调用方为 [`Self::paint_frame`]）
    /// * `colors` - Terminal color configuration
    ///
    /// # Returns
    ///
    /// A tuple of `(backgrounds, text_runs)` where:
    /// - `backgrounds` is a vector of merged background rectangles
    /// - `text_runs` is a vector of batched text runs
    pub fn layout_row<'a>(
        &self,
        row: usize,
        cells: impl Iterator<Item = (usize, &'a Cell)>,
        colors: &Colors,
        default_bg: Hsla,
    ) -> (Vec<BackgroundRect>, Vec<BatchedTextRun>) {
        let mut backgrounds = Vec::new();
        let mut text_runs = Vec::new();

        let mut current_run: Option<BatchedTextRun> = None;
        let mut current_bg: Option<BackgroundRect> = None;

        for (col, cell) in cells {
            // Skip wide-character spacers; the leading cell owns both columns.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                if let Some(run) = current_run.take() {
                    text_runs.push(run);
                }
                continue;
            }

            // Extract cell styling
            let fg_color = self.palette.resolve(cell.fg, colors);
            let bg_color = self.palette.resolve(cell.bg, colors);
            let bold = cell.flags.contains(Flags::BOLD);
            let italic = cell.flags.contains(Flags::ITALIC);
            // SGR 4:0~4:5 五种下划线形态（bitflags 并存时按直线优先的固定
            // 序取首个；SGR 语义一次只置一种）
            let underline = if cell.flags.contains(Flags::UNDERLINE) {
                UnderlineKind::Straight
            } else if cell.flags.contains(Flags::DOUBLE_UNDERLINE) {
                UnderlineKind::Double
            } else if cell.flags.contains(Flags::UNDERCURL) {
                UnderlineKind::Wavy
            } else if cell.flags.contains(Flags::DOTTED_UNDERLINE) {
                UnderlineKind::Dotted
            } else if cell.flags.contains(Flags::DASHED_UNDERLINE) {
                UnderlineKind::Dashed
            } else {
                UnderlineKind::None
            };
            // 彩色下划线（SGR 58）：resolve 进主题色板；None = 回退前景色
            //（由绘制侧 unwrap_or(fg_color) 兜底，同 xterm 口径）
            let underline_color = cell
                .underline_color()
                .map(|color| self.palette.resolve(color, colors));

            // Get the character (or space if empty)
            let ch = if cell.c == ' ' || cell.c == '\0' {
                ' '
            } else {
                cell.c
            };

            // 默认背景由整行底色覆盖，不创建逐格背景矩形。
            if bg_color == default_bg {
                if let Some(bg_rect) = current_bg.take() {
                    backgrounds.push(bg_rect);
                }
            } else if let Some(ref mut bg_rect) = current_bg {
                if bg_rect.color == bg_color && bg_rect.end_col == col {
                    bg_rect.end_col = col + 1;
                } else {
                    backgrounds.push(bg_rect.clone());
                    current_bg = Some(BackgroundRect {
                        start_col: col,
                        end_col: col + 1,
                        row,
                        color: bg_color,
                    });
                }
            } else {
                current_bg = Some(BackgroundRect {
                    start_col: col,
                    end_col: col + 1,
                    row,
                    color: bg_color,
                });
            }

            // 空格只由背景层覆盖，不需要进入字体整形；空白处也切断文本 run。
            if ch == ' ' || ch == '\0' {
                if let Some(run) = current_run.take() {
                    text_runs.push(run);
                }
                continue;
            }

            // Handle text runs; custom box-drawing passes own these glyphs.
            if box_drawing::is_box_drawing_char(cell.c) {
                if let Some(run) = current_run.take() {
                    text_runs.push(run);
                }
                continue;
            }
            if let Some(ref mut run) = current_run {
                if run.fg_color == fg_color
                    && run.bg_color == bg_color
                    && run.bold == bold
                    && run.italic == italic
                    && run.underline == underline
                    && run.underline_color == underline_color
                    && run.end_col == col
                {
                    // Extend current run
                    run.text.push(ch);
                    run.end_col = col + char_columns(ch);
                } else {
                    // Save current run and start new one
                    text_runs.push(run.clone());
                    current_run = Some(BatchedTextRun {
                        text: ch.to_string(),
                        start_col: col,
                        end_col: col + char_columns(ch),
                        row,
                        fg_color,
                        bg_color,
                        bold,
                        italic,
                        underline,
                        underline_color,
                    });
                }
            } else {
                // Start new run
                current_run = Some(BatchedTextRun {
                    text: ch.to_string(),
                    start_col: col,
                    end_col: col + char_columns(ch),
                    row,
                    fg_color,
                    bg_color,
                    bold,
                    italic,
                    underline,
                    underline_color,
                });
            }
        }

        // Push final run and background
        if let Some(run) = current_run {
            text_runs.push(run);
        }
        if let Some(bg) = current_bg {
            backgrounds.push(bg);
        }

        // Merge adjacent backgrounds with same color
        let merged_backgrounds = self.merge_backgrounds(backgrounds);

        (merged_backgrounds, text_runs)
    }

    /// Merge adjacent background rects with same color.
    ///
    /// This optimization reduces the number of rectangles to paint by
    /// combining horizontally adjacent rectangles that share the same color.
    ///
    /// # Arguments
    ///
    /// * `rects` - Vector of background rectangles to merge
    ///
    /// # Returns
    ///
    /// A new vector with merged rectangles
    fn merge_backgrounds(&self, mut rects: Vec<BackgroundRect>) -> Vec<BackgroundRect> {
        if rects.is_empty() {
            return rects;
        }

        let mut merged = Vec::new();
        let mut current = rects.remove(0);

        for rect in rects {
            if current.can_merge_with(&rect) {
                current.end_col = rect.end_col;
            } else {
                merged.push(current);
                current = rect;
            }
        }

        merged.push(current);
        merged
    }

    /// Paint terminal content to the window.
    ///
    /// This is the main rendering method that draws the terminal grid,
    /// including backgrounds, text, and cursor.
    ///
    /// 薄包装（公共 API 保持不变）：锁内一次 [`FrameSnapshot::capture`] 随即
    /// 释放，绘制本体走 [`Self::paint_frame`]——调用方持有 `&Term` 的时间从
    /// 「整段绘制」缩到「快照复制」。
    ///
    /// # Arguments
    ///
    /// * `bounds` - The bounding box to render within
    /// * `padding` - Padding around the terminal content
    /// * `term` - The terminal state
    /// * `window` - The GPUI window
    /// * `cx` - The application context
    #[allow(clippy::too_many_arguments)] // 绘制入口：逐项传参比打包结构更直观
    pub fn paint(
        &self,
        bounds: Bounds<Pixels>,
        padding: Edges<Pixels>,
        term: &Term<GpuiEventProxy>,
        window: &mut Window,
        _cx: &mut App,
        cursor_visible: bool,
        scroll_offset: f32,
    ) {
        let frame = FrameSnapshot::capture(term);
        self.paint_frame(
            bounds,
            padding,
            &frame,
            window,
            _cx,
            cursor_visible,
            scroll_offset,
        );
    }

    /// 按既有快照绘制网格：数据源全部来自 [`FrameSnapshot`]（与覆盖层同帧）。
    ///
    /// 与快照前 `paint(&Term)` 的绘制逐行等价：三段顺序（背景→行内容
    /// 横线→竖线→文本→光标）与全部坐标公式保持不动，仅把数据源从
    /// `term.grid()` 直读换成同帧快照字段。
    #[allow(clippy::too_many_arguments)] // 绘制入口：逐项传参比打包结构更直观
    pub fn paint_frame(
        &self,
        bounds: Bounds<Pixels>,
        padding: Edges<Pixels>,
        frame: &FrameSnapshot,
        window: &mut Window,
        _cx: &mut App,
        cursor_visible: bool,
        scroll_offset: f32,
    ) {
        // Get terminal dimensions（数据源：同帧快照）
        let num_lines = frame.screen_lines;
        let colors = &frame.colors;

        // Calculate default background color
        let default_bg = self.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            colors,
        );

        // Paint default background (covers full bounds including padding)
        // 透明模式（background_opacity<1）跳绘：窗口底色由宿主层以主题
        // background 原色@o 承载，此处再画满幅底色会叠高有效 alpha
        // （多层同画合成 1-(1-o)^n，o=0.7 时两层即 0.91），静默破坏透明口径。
        if self.background_opacity >= 1.0 {
            window.paint_quad(quad(
                bounds,
                px(0.0),
                default_bg,
                Edges::<Pixels>::default(),
                transparent_black(),
                Default::default(),
            ));
        }

        // Calculate origin offset (content starts after padding)
        let origin = Point {
            x: bounds.origin.x + padding.left,
            y: bounds.origin.y + padding.top,
        };

        // Iterate over visible lines
        let display_offset = frame.display_offset as i32;
        for line_idx in 0..num_lines {
            // 本行快照（锁内已复制，这里只借引用，不再逐格 clone）
            let row_cells = &frame.cells[line_idx];

            // Layout the row once; text runs are shaped in batches below.
            // 借用迭代：消除快照前「进 cells Vec 一次 + 值迭代再 clone 一次」的第二遍复制。
            let (backgrounds, text_runs) =
                self.layout_row(line_idx, row_cells.iter().enumerate(), colors, default_bg);

            // Paint backgrounds
            for bg_rect in backgrounds {
                // Skip if it's the default background color
                if bg_rect.color == default_bg {
                    continue;
                }

                let x = origin.x + self.cell_width * (bg_rect.start_col as f32);
                let y = origin.y + self.cell_height * (bg_rect.row as f32 + scroll_offset);
                let width = self.cell_width * ((bg_rect.end_col - bg_rect.start_col) as f32);
                let height = self.cell_height;

                let rect_bounds = Bounds {
                    origin: Point { x, y },
                    size: Size { width, height },
                };

                window.paint_quad(quad(
                    rect_bounds,
                    px(0.0),
                    // 不透明度单点缩放：合并/跳过判定（上方 == default_bg）仍按
                    // 原色进行，alpha 缩放只在绘制时乘一次；o=1.0 时 opacity(1.0)
                    // 原样返回，历史绘制零变化。
                    bg_rect.color.opacity(self.background_opacity),
                    Edges::<Pixels>::default(),
                    transparent_black(),
                    Default::default(),
                ));
            }

            // Calculate vertical offset to center text in cell
            // The multiplier adds extra height; we want to distribute it evenly top/bottom
            let base_height = self.cell_height / self.line_height_multiplier;
            let vertical_offset = (self.cell_height - base_height) / 2.0;

            let y_base = origin.y + self.cell_height * (line_idx as f32 + scroll_offset);
            let cy = y_base + self.cell_height / 2.0;

            // First pass: find and draw horizontal spans of box-drawing characters
            // This draws continuous lines across multiple cells to avoid gaps
            let mut processed_horizontal: std::collections::HashSet<usize> =
                std::collections::HashSet::new();

            let mut i = 0;
            while i < row_cells.len() {
                let cell = &row_cells[i];
                let col_idx = i;
                let ch = cell.c;

                // Check if this starts a horizontal span
                if let Some(weight) = box_drawing::get_horizontal_weight(ch) {
                    let fg_color = self.palette.resolve(cell.fg, colors);
                    let start_col = col_idx;
                    let mut end_col = col_idx;

                    // Look ahead for consecutive cells with same horizontal weight
                    let mut j = i + 1;
                    while j < row_cells.len() {
                        let next_cell = &row_cells[j];
                        let next_col = j;
                        // Must be adjacent
                        if next_col != end_col + 1 {
                            break;
                        }
                        // Must have same horizontal weight and same color
                        let next_fg = self.palette.resolve(next_cell.fg, colors);
                        if box_drawing::get_horizontal_weight(next_cell.c) == Some(weight)
                            && next_fg == fg_color
                        {
                            end_col = next_col;
                            j += 1;
                        } else {
                            break;
                        }
                    }

                    // Draw the horizontal span
                    let start_x = origin.x + self.cell_width * (start_col as f32);
                    let end_x = origin.x + self.cell_width * ((end_col + 1) as f32);

                    box_drawing::draw_horizontal_span(
                        start_x,
                        end_x,
                        cy,
                        weight,
                        self.cell_width,
                        fg_color,
                        window,
                    );

                    // Mark these columns as having horizontal drawn
                    for col in start_col..=end_col {
                        processed_horizontal.insert(col);
                    }

                    // Skip past this span
                    i = j;
                    continue;
                }
                i += 1;
            }

            // Second pass: draw vertical components and non-horizontal box chars
            for (col_idx, cell) in row_cells.iter().enumerate() {
                let ch = cell.c;

                if ch == ' ' || ch == '\0' {
                    continue;
                }

                let x = origin.x + self.cell_width * (col_idx as f32);
                let fg_color = self.palette.resolve(cell.fg, colors);

                if box_drawing::is_box_drawing_char(ch) {
                    let cell_bounds = Bounds {
                        origin: Point { x, y: y_base },
                        size: Size {
                            width: self.cell_width,
                            height: self.cell_height,
                        },
                    };

                    if processed_horizontal.contains(&col_idx) {
                        // Horizontal already drawn, just draw vertical components
                        box_drawing::draw_vertical_components(
                            ch,
                            cell_bounds,
                            fg_color,
                            self.cell_width,
                            window,
                        );
                    } else {
                        // Not part of a horizontal span, draw the whole character
                        box_drawing::draw_box_character(
                            ch,
                            cell_bounds,
                            fg_color,
                            self.cell_width,
                            window,
                        );
                    }
                    continue;
                }
            }

            // Third pass: shape and paint contiguous regular-text runs at once.
            // This avoids one rustybuzz/cosmic-text shaping call per cell per pane.
            for run in text_runs {
                let font = self.font_for(run.bold, run.italic);
                // 直线/波浪走字形下划线（gpui Underline primitive 原生 GPU
                // 渲染）；颜色回退前景色（SGR 59 未指定时的 xterm 同款口径）。
                // Double/Dotted/Dashed 传 None（无原生样式），由下方 quad
                // 覆盖层绘制，避免字形层再画一条错样式的直线。
                let underline = match run.underline {
                    UnderlineKind::Straight | UnderlineKind::Wavy => Some(UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(run.underline_color.unwrap_or(run.fg_color)),
                        wavy: run.underline == UnderlineKind::Wavy,
                    }),
                    UnderlineKind::None
                    | UnderlineKind::Double
                    | UnderlineKind::Dotted
                    | UnderlineKind::Dashed => None,
                };
                let text_run = TextRun {
                    len: run.text.len(),
                    font,
                    color: run.fg_color,
                    background_color: None,
                    underline,
                    strikethrough: None,
                };
                let text: SharedString = run.text.into();
                let shaped_line =
                    window
                        .text_system()
                        .shape_line(text, self.font_size, &[text_run], None);
                let x = origin.x + self.cell_width * run.start_col as f32;
                let y = y_base + vertical_offset;
                let _ = shaped_line.paint(Point { x, y }, self.cell_height, window, _cx);

                // 双线/点线/虚线：逐帧 quad 覆盖层（与 run 数成正比而非逐格，
                // 正常 TUI 输出 4:2/4:4/4:5 罕见，性能可控）。不透明度单点
                // 与背景层同口径。
                match run.underline {
                    UnderlineKind::Double | UnderlineKind::Dotted | UnderlineKind::Dashed => {
                        let run_x = origin.x + self.cell_width * (run.start_col as f32);
                        let width = self.cell_width * ((run.end_col - run.start_col) as f32);
                        // 行底（含滚动偏移）：与背景层同一 y 口径
                        let row_bottom =
                            origin.y + self.cell_height * (line_idx as f32 + scroll_offset + 1.0);
                        for rect_bounds in special_underline_quads(
                            run.underline,
                            run_x,
                            row_bottom,
                            width,
                            self.cell_width,
                            self.cell_height,
                        ) {
                            window.paint_quad(quad(
                                rect_bounds,
                                px(0.0),
                                run.underline_color
                                    .unwrap_or(run.fg_color)
                                    .opacity(self.background_opacity),
                                Edges::<Pixels>::default(),
                                transparent_black(),
                                Default::default(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }

        if !frame.mode.contains(TermMode::SHOW_CURSOR) || !cursor_visible {
            return;
        }

        let cursor_point = frame.cursor;
        let cursor_line = cursor_point.line.0 + display_offset;
        if cursor_line < 0 || cursor_line >= num_lines as i32 {
            return;
        }
        let cursor_x = origin.x + self.cell_width * (cursor_point.column.0 as f32);
        let cursor_y = origin.y + self.cell_height * (cursor_line as f32 + scroll_offset);

        let cursor_color = self.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Cursor),
            colors,
        );

        let cursor_bounds = cursor_rect(
            self.cursor_style,
            cursor_x,
            cursor_y,
            self.cell_width,
            self.cell_height,
        );

        window.paint_quad(quad(
            cursor_bounds,
            px(0.0),
            cursor_color,
            Edges::<Pixels>::default(),
            transparent_black(),
            Default::default(),
        ));
    }

    /// 绘制 IME 预编辑浮层：光标处起排、带下划线、不写网格。
    ///
    /// 预编辑文本不属于 VT 流（shell 看不见它），只在提交时写入 PTY；
    /// 因此这里按覆盖层画在网格之上，「不破坏网格」是硬约束。
    /// 行尾放不下的部分截断（布局见 [`preedit_layout`]）。
    /// 公共 API 保持不变；快照调用方请用 [`Self::paint_preedit_frame`]。
    pub fn paint_preedit(
        &self,
        term: &Term<GpuiEventProxy>,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
        text: &str,
        scroll_offset: f32,
    ) {
        if text.is_empty() {
            return;
        }
        let grid = term.grid();
        let display_offset = grid.display_offset() as i32;
        let cursor = grid.cursor.point;
        let row = preedit_row(cursor.line.0, display_offset, grid.screen_lines());
        let fg_color = self.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
            term.colors(),
        );
        self.paint_preedit_at(
            origin,
            window,
            cx,
            text,
            scroll_offset,
            cursor.column.0,
            row,
            grid.columns(),
            fg_color,
        );
    }

    /// 快照版预编辑浮层：数据源为同帧 [`FrameSnapshot`]（与网格绘制同帧，无撕裂）。
    pub fn paint_preedit_frame(
        &self,
        frame: &FrameSnapshot,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
        text: &str,
        scroll_offset: f32,
    ) {
        if text.is_empty() {
            return;
        }
        let row = preedit_row(
            frame.cursor.line.0,
            frame.display_offset as i32,
            frame.screen_lines,
        );
        let fg_color = self.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
            &frame.colors,
        );
        self.paint_preedit_at(
            origin,
            window,
            cx,
            text,
            scroll_offset,
            frame.cursor.column.0,
            row,
            frame.columns,
            fg_color,
        );
    }

    /// 预编辑绘制本体：`(起始列, 视口行, 列数, 前景色)` 直读版与快照版共用，
    /// 公式唯一（行换算见 [`preedit_row`]，布局见 [`preedit_layout`]）。
    #[allow(clippy::too_many_arguments)] // 内部共用体：逐项传参与两版调用方对齐
    fn paint_preedit_at(
        &self,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
        text: &str,
        scroll_offset: f32,
        start_col: usize,
        row: usize,
        columns: usize,
        fg_color: Hsla,
    ) {
        let font = self.font_for(false, false);
        // 与网格文字相同的垂直居中口径
        let base_height = self.cell_height / self.line_height_multiplier;
        let vertical_offset = (self.cell_height - base_height) / 2.0;
        let y = origin.y + self.cell_height * (row as f32 + scroll_offset) + vertical_offset;

        for (col, ch, _) in preedit_layout(text, start_col, columns) {
            let char_str = ch.to_string();
            let run = TextRun {
                len: char_str.len(),
                font: font.clone(),
                color: fg_color,
                background_color: None,
                underline: Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(fg_color),
                    wavy: false,
                }),
                strikethrough: None,
            };
            let shaped_line = window.text_system().shape_line(
                SharedString::from(char_str),
                self.font_size,
                &[run],
                None,
            );
            let x = origin.x + self.cell_width * (col as f32);
            let _ = shaped_line.paint(Point { x, y }, self.cell_height, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_snapshot_captures_visible_grid_and_overlays() {
        use crate::event::GpuiEventProxy;
        use crate::terminal::TerminalState;
        use alacritty_terminal::grid::Scroll;
        use alacritty_terminal::index::Side;
        use alacritty_terminal::selection::{
            Selection as AlacSelection, SelectionType as AlacSelectionType,
        };
        use alacritty_terminal::term::color::COUNT;
        use alacritty_terminal::vte::ansi::NamedColor;
        use std::sync::mpsc::channel;

        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(20, 4, 32, false, GpuiEventProxy::new(tx));
        // OSC 4 调色板覆盖（colors 非默认）+ 宽字符行 + SGR 加粗红样式行
        terminal.process_bytes(b"\x1b]4;1;rgb:ff/00/00\x1b\\");
        terminal.process_bytes("首行中文\r\n".as_bytes());
        for i in 0..12 {
            terminal.process_bytes(format!("\x1b[1;31mR{i}\x1b[0m中文\r\n").as_bytes());
        }
        // 选区：网格第 2 行 0..5 列；再上滚 2 行制造 display_offset
        terminal.with_term_mut(|term| {
            let mut selection = AlacSelection::new(
                AlacSelectionType::Simple,
                AlacPoint::new(Line(2), Column(0)),
                Side::Left,
            );
            selection.update(AlacPoint::new(Line(2), Column(4)), Side::Right);
            term.selection = Some(selection);
        });
        terminal.with_term_mut(|term| term.scroll_display(Scroll::Delta(2)));

        let (snapshot, expected) = terminal.with_term(|term| {
            let snap = FrameSnapshot::capture(term);
            // 同一次持锁内对同一份 Term 现读，逐字段对账
            let grid = term.grid();
            let cells: Vec<Vec<Cell>> = (0..grid.screen_lines())
                .map(|row| {
                    let line = Line(row as i32 - grid.display_offset() as i32);
                    (0..grid.columns())
                        .map(|col| grid[AlacPoint::new(line, Column(col))].clone())
                        .collect()
                })
                .collect();
            let expected = (
                cells,
                grid.columns(),
                grid.screen_lines(),
                grid.total_lines(),
                grid.display_offset(),
                grid.cursor.point,
                *term.mode(),
                term.selection.as_ref().and_then(|sel| sel.to_range(term)),
                *term.colors(),
            );
            (snap, expected)
        });

        let (
            cells,
            columns,
            screen_lines,
            total_lines,
            display_offset,
            cursor,
            mode,
            selection,
            colors,
        ) = expected;

        // 前置条件自证：滚动/选区/调色板/可见区样式确实生效，对账不是空转
        assert_eq!(snapshot.display_offset, 2, "前置条件：上滚两行");
        assert!(snapshot.selection.is_some(), "前置条件：选区已建立");
        assert!(snapshot.colors[1].is_some(), "前置条件：OSC 4 覆盖红色");
        assert!(
            snapshot
                .cells
                .iter()
                .flatten()
                .any(|cell| cell.flags.contains(Flags::WIDE_CHAR)),
            "前置条件：宽字符在可见区"
        );
        assert!(
            snapshot
                .cells
                .iter()
                .flatten()
                .any(|cell| cell.flags.contains(Flags::BOLD)
                    && cell.fg == Color::Named(NamedColor::Red)),
            "前置条件：SGR 样式在可见区"
        );

        assert_eq!(snapshot.cells, cells, "可见区逐格对账（SGR/宽字符/滚动）");
        assert_eq!(snapshot.columns, columns);
        assert_eq!(snapshot.screen_lines, screen_lines);
        assert_eq!(snapshot.total_lines, total_lines);
        assert_eq!(snapshot.display_offset, display_offset);
        assert_eq!(snapshot.cursor, cursor);
        assert_eq!(snapshot.mode, mode);
        assert_eq!(snapshot.selection, selection, "选区与锁内 to_range 一致");
        // Colors 未实现 PartialEq，逐槽对账
        for index in 0..COUNT {
            assert_eq!(snapshot.colors[index], colors[index], "颜色槽 {index}");
        }

        // 缓冲池复用路径：同一缓冲二次 capture_into（行容量已预扩），结果必须
        // 与首捕逐格一致——稳态帧全部走这条复用路径
        let mut reused = snapshot.clone();
        terminal.with_term(|term| FrameSnapshot::capture_into(term, &mut reused));
        assert_eq!(reused.cells, snapshot.cells, "复用缓冲逐格一致");
        assert_eq!(reused.display_offset, snapshot.display_offset);
        assert_eq!(reused.selection, snapshot.selection);
    }

    #[test]
    fn layout_row_borrowed_matches_owned_value_iteration() {
        let renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.0,
            ColorPalette::default(),
            true,
            Vec::new(),
            CursorStyle::Block,
        );
        let colors = Colors::default();
        let default_bg = renderer.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            &colors,
        );

        // 行形态覆盖：普通字符 / 空格 / 宽字符对 / box-drawing / 可合并样式段
        let mut cells = vec![Cell::default(); 10];
        cells[0].c = 'a';
        cells[1].c = 'b';
        cells[2].c = 'c';
        cells[3].c = ' ';
        cells[4].c = '中';
        cells[4].flags = Flags::WIDE_CHAR;
        cells[5].c = ' ';
        cells[5].flags = Flags::WIDE_CHAR_SPACER;
        cells[6].c = '─';
        for (col, ch) in [(7usize, 'X'), (8usize, 'Y')] {
            cells[col].c = ch;
            cells[col].fg = Color::Named(alacritty_terminal::vte::ansi::NamedColor::Red);
            cells[col].bg = Color::Named(alacritty_terminal::vte::ansi::NamedColor::Blue);
            cells[col].flags = Flags::BOLD;
        }
        cells[9].c = 'z';

        // 新借用迭代（快照行直引）
        let (bg_borrowed, runs_borrowed) =
            renderer.layout_row(0, cells.iter().enumerate(), &colors, default_bg);
        // 旧值迭代语义：先克隆成拥有值再借引用迭代 —— 输出必须一致
        let owned: Vec<(usize, Cell)> = cells.iter().cloned().enumerate().collect();
        let (bg_owned, runs_owned) = renderer.layout_row(
            0,
            owned.iter().map(|(col, cell)| (*col, cell)),
            &colors,
            default_bg,
        );
        assert_eq!(bg_borrowed, bg_owned, "背景与旧值迭代一致");
        assert_eq!(runs_borrowed, runs_owned, "文本 run 与旧值迭代一致");

        // 字面期望：abc 单 run；空格切断；「中」占两列；box-drawing 不进 run；
        // X/Y 同样式合并；z 样式不同自成 run
        let runs: Vec<(usize, usize, &str)> = runs_borrowed
            .iter()
            .map(|run| (run.start_col, run.end_col, run.text.as_str()))
            .collect();
        assert_eq!(
            runs,
            vec![(0, 3, "abc"), (4, 6, "中"), (7, 9, "XY"), (9, 10, "z")]
        );
        // 背景合并：col7-8 蓝底合并为单矩形
        assert_eq!(bg_borrowed.len(), 1);
        assert_eq!((bg_borrowed[0].start_col, bg_borrowed[0].end_col), (7, 9));
    }

    #[test]
    fn search_match_rect_maps_history_row_and_rejects_offscreen() {
        let origin = gpui::point(px(10.0), px(20.0));
        // 命中在视口内：row = line + display_offset - history = 38 + 2 - 36 = 4
        let rect = search_match_rect(38, 4, 6, 2, 36, 40, origin, px(8.0), px(20.0))
            .expect("视口内命中应有矩形");
        assert_eq!(rect.origin.x, px(10.0 + 8.0 * 4.0));
        assert_eq!(rect.origin.y, px(20.0 + 20.0 * 4.0));
        assert_eq!(rect.size.width, px(8.0 * 6.0));
        assert_eq!(rect.size.height, px(20.0));
        // 滚出视口上方（row<0）/下方（row>=screen_lines）→ None
        assert!(search_match_rect(10, 0, 3, 2, 36, 40, origin, px(8.0), px(20.0)).is_none());
        assert!(search_match_rect(80, 0, 3, 2, 36, 40, origin, px(8.0), px(20.0)).is_none());
    }

    #[test]
    fn selection_row_spans_clamp_to_viewport_and_columns() {
        // 选区跨网格行 -2..=1（端点列 3 与 9），display_offset=2，视口 6 行 10 列
        let selection = SelectionRange::new(
            AlacPoint::new(Line(-2), Column(3)),
            AlacPoint::new(Line(1), Column(9)),
            false,
        );
        assert_eq!(
            selection_row_spans(selection, 2, 10, 6),
            vec![(0, 3, 10), (1, 0, 10), (2, 0, 10), (3, 0, 10)],
            "首行从端点列起，中间行整行，末行裁到端点列+1"
        );
        // 选区整体滚出视口上方 → 空扇区
        let above = SelectionRange::new(
            AlacPoint::new(Line(-9), Column(0)),
            AlacPoint::new(Line(-8), Column(5)),
            false,
        );
        assert!(selection_row_spans(above, 2, 10, 6).is_empty());
        // 末列越界钳到 columns
        let wide = SelectionRange::new(
            AlacPoint::new(Line(0), Column(8)),
            AlacPoint::new(Line(0), Column(20)),
            false,
        );
        assert_eq!(selection_row_spans(wide, 0, 10, 6), vec![(0, 8, 10)]);
    }

    #[test]
    fn preedit_row_clamps_into_viewport() {
        assert_eq!(preedit_row(3, 0, 4), 3);
        assert_eq!(preedit_row(-5, 2, 4), 0, "滚出视口钳到顶行");
        assert_eq!(preedit_row(10, 0, 4), 3, "越底钳到底行");
    }

    #[test]
    fn test_renderer_creation() {
        let renderer = TerminalRenderer::new(
            "Fira Code".to_string(),
            px(14.0),
            1.0,
            ColorPalette::default(),
            true,
            Vec::new(),
            CursorStyle::Block,
        );
        assert_eq!(renderer.font_family, "Fira Code");
        assert_eq!(renderer.font_size, px(14.0));
        assert_eq!(renderer.line_height_multiplier, 1.0);
    }

    #[test]
    fn test_background_rect_merge() {
        let black = Hsla::black();

        let rect1 = BackgroundRect {
            start_col: 0,
            end_col: 5,
            row: 0,
            color: black,
        };

        let rect2 = BackgroundRect {
            start_col: 5,
            end_col: 10,
            row: 0,
            color: black,
        };

        assert!(rect1.can_merge_with(&rect2));

        let rect3 = BackgroundRect {
            start_col: 5,
            end_col: 10,
            row: 1,
            color: black,
        };

        assert!(!rect1.can_merge_with(&rect3));
    }

    #[test]
    fn test_merge_backgrounds() {
        let renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.0,
            ColorPalette::default(),
            true,
            Vec::new(),
            CursorStyle::Block,
        );
        let black = Hsla::black();

        let rects = vec![
            BackgroundRect {
                start_col: 0,
                end_col: 5,
                row: 0,
                color: black,
            },
            BackgroundRect {
                start_col: 5,
                end_col: 10,
                row: 0,
                color: black,
            },
        ];

        let merged = renderer.merge_backgrounds(rects);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start_col, 0);
        assert_eq!(merged[0].end_col, 10);
    }

    // ───────── 下划线（SGR 4:0~4:5 + 彩色下划线） ─────────

    /// 构造带指定样式的一行 cells（便于 layout_row 分 run 断言）。
    fn underline_cells(specs: &[(usize, char, Flags)]) -> Vec<Cell> {
        let mut cells = vec![Cell::default(); 12];
        for &(col, ch, flags) in specs {
            cells[col].c = ch;
            cells[col].flags = flags;
        }
        cells
    }

    #[test]
    fn layout_row_splits_runs_by_underline_style() {
        let renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.0,
            ColorPalette::default(),
            true,
            Vec::new(),
            CursorStyle::Block,
        );
        let colors = Colors::default();
        let default_bg = renderer.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            &colors,
        );

        // 同色三段：直线 → 点线 → 无下划线。样式变化必须分裂 run
        //（点线走 quad 覆盖层、直线走字形路径，混入同一 run 会错样式）
        let cells = underline_cells(&[
            (0, 'a', Flags::UNDERLINE),
            (1, 'b', Flags::UNDERLINE),
            (2, 'c', Flags::DOTTED_UNDERLINE),
            (3, 'd', Flags::DOTTED_UNDERLINE),
            (4, 'e', Flags::empty()),
            (5, 'f', Flags::empty()),
        ]);
        let (_, runs) = renderer.layout_row(0, cells.iter().enumerate(), &colors, default_bg);
        assert_eq!(runs.len(), 3, "样式三段应分裂为三个 run: {runs:?}");
        assert_eq!(runs[0].underline, UnderlineKind::Straight);
        assert_eq!(runs[0].start_col, 0);
        assert_eq!(runs[0].end_col, 2);
        assert_eq!(runs[1].underline, UnderlineKind::Dotted);
        assert_eq!(runs[1].start_col, 2);
        assert_eq!(runs[1].end_col, 4);
        assert_eq!(runs[2].underline, UnderlineKind::None);
        assert_eq!(runs[2].start_col, 4);
        assert_eq!(runs[2].end_col, 6);

        // 双线/波浪/虚线各自成段（键覆盖全部 UnderlineKind）
        let cells = underline_cells(&[
            (0, 'a', Flags::DOUBLE_UNDERLINE),
            (1, 'b', Flags::UNDERCURL),
            (2, 'c', Flags::DASHED_UNDERLINE),
        ]);
        let (_, runs) = renderer.layout_row(0, cells.iter().enumerate(), &colors, default_bg);
        assert_eq!(runs.len(), 3, "三异样式应三 run: {runs:?}");
        assert_eq!(runs[0].underline, UnderlineKind::Double);
        assert_eq!(runs[1].underline, UnderlineKind::Wavy);
        assert_eq!(runs[2].underline, UnderlineKind::Dashed);
    }

    #[test]
    fn layout_row_splits_runs_by_underline_color() {
        let renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.0,
            ColorPalette::default(),
            true,
            Vec::new(),
            CursorStyle::Block,
        );
        let colors = Colors::default();
        let default_bg = renderer.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            &colors,
        );

        // 同为直线但 SGR 58 颜色从红变蓝：颜色变化必须分裂 run
        let mut cells = vec![Cell::default(); 4];
        cells[0].c = 'a';
        cells[0].flags = Flags::UNDERLINE;
        cells[0].set_underline_color(Some(Color::Named(
            alacritty_terminal::vte::ansi::NamedColor::Red,
        )));
        cells[1].c = 'b';
        cells[1].flags = Flags::UNDERLINE;
        cells[1].set_underline_color(Some(Color::Named(
            alacritty_terminal::vte::ansi::NamedColor::Red,
        )));
        cells[2].c = 'c';
        cells[2].flags = Flags::UNDERLINE;
        cells[2].set_underline_color(Some(Color::Named(
            alacritty_terminal::vte::ansi::NamedColor::Blue,
        )));
        cells[3].c = 'd';
        cells[3].flags = Flags::UNDERLINE;
        let (_, runs) = renderer.layout_row(0, cells.iter().enumerate(), &colors, default_bg);
        assert_eq!(runs.len(), 3, "颜色红/蓝/回退三段应三 run: {runs:?}");
        assert_eq!(runs[0].underline, UnderlineKind::Straight);
        assert!(runs[0].underline_color.is_some(), "红段带 SGR 58 色");
        assert_eq!(runs[0].end_col, 2);
        // 蓝段：颜色变化分裂出的第二段（列 2..3）
        assert_eq!(runs[1].start_col, 2);
        assert_ne!(
            runs[1].underline_color, runs[0].underline_color,
            "颜色变化必须分裂 run"
        );
        // 未指定 SGR 58 的段：None = 绘制侧回退前景色
        assert_eq!(runs[2].start_col, 3);
        assert!(runs[2].underline_color.is_none(), "未指定色回退前景色");
    }

    #[test]
    fn special_underline_quads_geometry() {
        let cw = px(10.0);
        let ch = px(20.0);
        let x = px(100.0);
        let bottom = px(50.0);
        let width = px(30.0); // 3 个 cell

        // 直线/波浪/无：不走 quad 路径（字形路径负责）
        assert!(special_underline_quads(UnderlineKind::None, x, bottom, width, cw, ch).is_empty());
        assert!(
            special_underline_quads(UnderlineKind::Straight, x, bottom, width, cw, ch).is_empty()
        );
        assert!(special_underline_quads(UnderlineKind::Wavy, x, bottom, width, cw, ch).is_empty());

        // 双线：恰好两个横条，厚 = (20*0.07).max(1px) = 1.4px，带间隙
        let double = special_underline_quads(UnderlineKind::Double, x, bottom, width, cw, ch);
        assert_eq!(double.len(), 2);
        let t = px(1.4);
        for rect in &double {
            assert_eq!(rect.size.width, width, "双线横条横跨整个 run");
            assert_eq!(rect.size.height, t);
        }
        // 上线 [bottom-3t, bottom-2t]、下线 [bottom-t, bottom]，间隙 t
        assert_eq!(double[0].origin.y, bottom - t * 3.0);
        assert_eq!(double[1].origin.y, bottom - t);

        // 点线：每格两点（点/空各 cell 宽 1/4 = 2.5px），3 格宽 30px →
        // 点在 0/5/10/15/20/25 → 6 个点（最后一点截到 run 尾）
        let dotted = special_underline_quads(UnderlineKind::Dotted, x, bottom, width, cw, ch);
        assert_eq!(dotted.len(), 6, "3 格宽 30px 应有 6 个点: {dotted:?}");
        assert_eq!(dotted[0].origin.x, x);
        assert_eq!(dotted[1].origin.x, x + px(5.0));
        assert_eq!(dotted[0].size.width, px(2.5));
        assert_eq!(dotted[0].size.height, t, "点高 = 线厚");

        // 虚线：段 cell 宽 1/2（5px）、空 1/4（2.5px），2:1 比例，
        // 30px = 4 个完整周期 → 段 0/7.5/15/22.5，末段完整 5px（格节奏
        // 优先于贴尾：尾随 2.5px 空隙属正常点线/虚线观感）
        let dashed = special_underline_quads(UnderlineKind::Dashed, x, bottom, width, cw, ch);
        assert_eq!(dashed.len(), 4, "3 格宽 30px 应有 4 个长段: {dashed:?}");
        assert_eq!(dashed[0].origin.x, x);
        assert_eq!(dashed[1].origin.x, x + px(7.5));
        assert_eq!(dashed[0].size.width, px(5.0));
        let last = dashed.last().unwrap();
        assert_eq!(last.origin.x, x + px(22.5));
        assert_eq!(last.size.width, px(5.0));
    }

    #[test]
    fn cursor_rect_shapes() {
        let (cw, ch) = (px(8.0), px(20.0));

        // 方块：占满整格（历史行为）
        let block = cursor_rect(CursorStyle::Block, px(10.0), px(40.0), cw, ch);
        assert_eq!((block.size.width, block.size.height), (cw, ch));

        // 竖线：左缘对齐光标列，高度整格，宽度按比例且小于格宽
        let bar = cursor_rect(CursorStyle::Bar, px(10.0), px(40.0), cw, ch);
        assert_eq!(bar.origin.x, px(10.0));
        assert_eq!(bar.size.height, ch);
        assert!(bar.size.width < cw);
        assert_eq!(bar.size.width, (cw * 0.15).max(px(1.0)));

        // 下划线：宽度整格，贴格底，厚度按比例且小于格高
        let under = cursor_rect(CursorStyle::Underline, px(10.0), px(40.0), cw, ch);
        assert_eq!(under.size.width, cw);
        assert_eq!(under.origin.y + under.size.height, px(60.0));
        assert!(under.size.height < ch);

        // 高 DPI 兜底：格尺寸极小时厚度不薄于 1px
        let tiny_bar = cursor_rect(CursorStyle::Bar, px(0.0), px(0.0), px(2.0), px(3.0));
        assert_eq!(tiny_bar.size.width, px(1.0));
        let tiny_under = cursor_rect(CursorStyle::Underline, px(0.0), px(0.0), px(2.0), px(3.0));
        assert_eq!(tiny_under.size.height, px(1.0));
    }
}

#[cfg(test)]
mod preedit_tests {
    use super::{char_columns, preedit_layout};

    #[test]
    fn ascii_advances_one_column_each() {
        let placed = preedit_layout("abc", 2, 10);
        let cols: Vec<usize> = placed.iter().map(|(col, _, _)| *col).collect();
        assert_eq!(cols, vec![2, 3, 4]);
    }

    #[test]
    fn cjk_takes_two_columns_and_mixed_layout_is_contiguous() {
        assert_eq!(char_columns('中'), 2);
        assert_eq!(char_columns('a'), 1);
        // "中a文" → 列 0(宽2)、2、3(宽2)
        let placed = preedit_layout("中a文", 0, 10);
        let layout: Vec<(usize, char, usize)> = placed;
        assert_eq!(layout, vec![(0, '中', 2), (2, 'a', 1), (3, '文', 2)]);
    }

    #[test]
    fn truncates_at_line_end() {
        // 10 列宽，从第 8 列起：放 "中"(8..10) 后 "a" 放不下即截断
        let placed = preedit_layout("中a", 8, 10);
        assert_eq!(placed, vec![(8, '中', 2)]);
        // 宽字符跨行尾放不下整个字符时，该字符整体不画
        assert!(preedit_layout("中", 9, 10).is_empty());
    }
}
