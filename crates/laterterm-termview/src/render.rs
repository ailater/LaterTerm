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
//! 4. **Cell Measurement**: Font metrics are measured once from a common ASCII
//!    sample and cached for consistent cell dimensions.
//!
//! 5. **竖直 run 合并**：盒线字符的竖直分量按列跨行合并为单段（帧级列状态
//!    [`TerminalRenderer::vrun_state`]），tmux 边框/树形的 │├└ 从逐格绘制
//!    变为一次整段绘制；端点收缩见 [`box_drawing::vertical_run_extent`]。
//!
//! 6. **轴对齐段 quad 化**：盒线水平/垂直段走 `paint_quad` 直绘（floor/ceil
//!    整像素对齐），零 tessellation；仅斜线与圆角曲线保留路径构建。
//!
//! 7. **逐帧零分配**：Font 8 槽缓存（family/fallbacks 仅重建时分配）、颜色
//!    解析 memo（帧内 Vec 线性查，[`resolve_cached`]）、行级布局缓冲
//!    （[`RowScratch`] 每行 clear 复用）全部跨行/跨槽位复用。
//!
//! # Cell Dimensions
//!
//! Cell size is calculated from actual font metrics:
//!
//! - **Width**: average advance of common lowercase/digits (fallback fonts may
//!   make single glyphs non-uniform)
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
use std::cell::RefCell;

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

/// 行级布局缓冲：paint 循环外创建一次、每行 [`RowScratch::clear`] 复用
///（与快照缓冲池同款），布局阶段稳态帧不再逐行分配 Vec/String。
#[derive(Default)]
pub struct RowScratch {
    /// 本行合并后的背景矩形
    backgrounds: Vec<BackgroundRect>,
    /// 本行分批后的文本 run（第三遍绘制时被 drain，容量保留）
    text_runs: Vec<BatchedTextRun>,
    /// 新 run 首字符的暂存缓冲：clear + push 后整体移交 run（`mem::take`），
    /// 消除逐格 `ch.to_string()` 分配；断点改为整体移交后不再有 run 克隆
    text: String,
    /// 第一遍水平 span 的列覆盖位图（替代逐行 HashSet；clear+resize 复用）
    horizontal_flags: Vec<bool>,
}

impl RowScratch {
    /// 清空全部行级缓冲（容量保留，供下一行复用）。
    pub fn clear(&mut self) {
        self.backgrounds.clear();
        self.text_runs.clear();
        self.text.clear();
        self.horizontal_flags.clear();
    }

    /// 本行背景矩形（布局后只读视图）。
    pub fn backgrounds(&self) -> &[BackgroundRect] {
        &self.backgrounds
    }

    /// 本行文本 run（布局后只读视图；第三遍绘制时会被 drain）。
    pub fn text_runs(&self) -> &[BatchedTextRun] {
        &self.text_runs
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

    /// 探测选定的实际渲染 family（Some = 用户字体不可用/非等宽，已回退）。
    /// None = 尚未探测或沿用用户字体。`font_for` 全部走这里。
    resolved_family: Option<String>,

    /// 回退提示只打一次（measure_cell 会在字体切换/窗口新建时多次执行）。
    fallback_notice_emitted: bool,

    /// Whether cell metrics need to be measured again.
    needs_measurement: bool,

    /// 连字/粗体/斜体 8 槽字体缓存（槽位索引见 [`font_slot_index`]）。
    /// 命中时每 text run 不再重建 family String 与 FontFallbacks。
    fonts: RefCell<[Option<Font>; 8]>,

    /// 字体缓存置脏：family/fallbacks/resolved_family 变化时置位。重建收敛
    /// 于本文件内两处——[`Self::invalidate_measurement`]（view 层设置变化
    /// 的唯一入口）与 [`Self::measure_cell`] 的探测落定点，view 层零改动。
    fonts_dirty: std::cell::Cell<bool>,

    /// 帧级竖直 run 列状态（每列一槽）：paint 入口按列数 clear+resize，
    /// 容量跨帧保留；RefCell 因 paint_frame 为 `&self`。
    vrun_state: RefCell<Vec<Option<VPending>>>,
}

/// 探测一个 family 是否可用且为真等宽。
///
/// 返回 `(单元格宽, ascent, descent)`；None = family 缺失（GPUI 逐字符回退
/// 会拆出多个 font run）或字形 advance 不一致（混入比例字体）。GPUI 对
/// 缺失字体的行为不是报错而是静默回退，必须靠字形级探测识别。
fn probe_monospace(
    window: &mut Window,
    font: &Font,
    font_size: Pixels,
) -> Option<(Pixels, Pixels, Pixels)> {
    // 混合宽窄 ASCII 类别：真等宽字体逐字形 advance 一致；比例字体/
    // 混合回退必然拉开差距。不含空格与连字对，避免字距/合字干扰。
    let sample = "Mm0%";
    let text_run = TextRun {
        len: sample.len(),
        font: font.clone(),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window
        .text_system()
        .shape_line(sample.into(), font_size, &[text_run], None);
    if shaped.runs.len() != 1 || shaped.runs[0].glyphs.len() != sample.len() {
        return None;
    }
    let glyphs = &shaped.runs[0].glyphs;
    let mut advances: Vec<Pixels> = glyphs
        .windows(2)
        .map(|pair| pair[1].position.x - pair[0].position.x)
        .collect();
    advances.push(shaped.width - glyphs.last().unwrap().position.x);
    let first = advances[0];
    if first <= px(0.0) {
        return None;
    }
    // 容差 1%：吸收亚像素舍入；真等宽差距为 0，混排回退远超此值
    if advances
        .iter()
        .any(|advance| (*advance - first).abs() > first * 0.01)
    {
        return None;
    }
    Some((first, shaped.ascent, shaped.descent))
}

/// 平台内置等宽兜底候选（按观感优先级排序），供用户字体不可用时探测。
fn platform_monospace_candidates() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        vec![
            "SF Mono".into(),
            "Menlo".into(),
            "Monaco".into(),
            "Courier New".into(),
            "monospace".into(),
        ]
    }
    #[cfg(target_os = "linux")]
    {
        vec![
            "JetBrains Mono".into(),
            "Noto Sans Mono".into(),
            "DejaVu Sans Mono".into(),
            "Liberation Mono".into(),
            "monospace".into(),
        ]
    }
    #[cfg(target_os = "windows")]
    {
        vec![
            "Cascadia Mono".into(),
            "Consolas".into(),
            "Courier New".into(),
            "monospace".into(),
        ]
    }
}

/// 帧内颜色解析 memo 表：`(终端色, 已解析 Hsla)` 线性小表。
///
/// vte 0.15 的 `ansi::Color` 只派生 `Debug, Clone, Copy, PartialEq, Eq`
///（无 `Hash`），HashMap 方案不成立；终端一帧的唯一色通常几十个，线性扫
/// 低于建表 + 哈希成本且零额外分配。生命周期为单帧：paint 入口创建、
/// 帧末随栈帧释放；`colors`（OSC 覆盖快照）一帧内不变，故仅以终端色为
/// 键即帧内一致。
pub type ColorMemo = Vec<(Color, Hsla)>;

/// 带帧内 memo 的颜色解析：命中直接返回，未命中 `palette.resolve` 后入表。
fn resolve_cached(
    memo: &mut ColorMemo,
    palette: &ColorPalette,
    color: Color,
    colors: &Colors,
) -> Hsla {
    if let Some((_, resolved)) = memo.iter().find(|(c, _)| c == &color) {
        return *resolved;
    }
    let resolved = palette.resolve(color, colors);
    memo.push((color, resolved));
    resolved
}

/// 字体缓存槽位索引：bit0=bold、bit1=italic、bit2=ligatures（共 8 组合）。
#[inline]
fn font_slot_index(bold: bool, italic: bool, ligatures: bool) -> usize {
    bold as usize | ((italic as usize) << 1) | ((ligatures as usize) << 2)
}

/// Font 构造唯一入口：连字/粗体/斜体均显式传参，不隐式读渲染器运行时状态。
/// family 与备选链由调用方决定（缓存重建时才分配 String/Vec）。
fn build_font(
    family: &str,
    fallbacks: &[String],
    ligatures: bool,
    bold: bool,
    italic: bool,
) -> Font {
    Font {
        family: family.to_string().into(),
        features: if ligatures {
            FontFeatures::default()
        } else {
            FontFeatures::disable_ligatures()
        },
        fallbacks: if fallbacks.is_empty() {
            None
        } else {
            Some(FontFallbacks::from_fonts(fallbacks.to_vec()))
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

/// 竖直 run 合并的列级 pending：一列中尚未冲刷的连续竖线段。
#[derive(Clone)]
struct VPending {
    /// run 首格上缘的视口 y（含滚动偏移）
    start_y: Pixels,
    /// 竖直分量粗细（整段一致）
    weight: box_drawing::LineWeight,
    /// 前景色（整段一致）
    color: Hsla,
    /// 首格 segments.top 是否存在（true=起点贴首格上缘；false=退到首格中心）
    top_extends: bool,
    /// 已扫入末格 segments.bottom 是否存在（冲刷时决定终点是否退半格）
    bottom_extends: bool,
    /// run 首格行号（计算末格上缘用）
    first_row: usize,
    /// 期望的下一行号（= 已扫入末行 + 1；行连续性判定）
    next_row: usize,
}

/// 冲刷一列的竖直 run：按首/末格端点收缩（[`box_drawing::vertical_run_extent`]）
/// 整段绘制，槽位随即清空。空槽为无操作。
fn flush_vrun(
    vrun: &mut [Option<VPending>],
    col: usize,
    origin: Point<Pixels>,
    cell_width: Pixels,
    cell_height: Pixels,
    window: &mut Window,
) {
    let Some(pending) = vrun[col].take() else {
        return;
    };
    let rows_spanned = (pending.next_row - pending.first_row) as f32;
    // 末格上缘 y = 首格上缘 + (段内行数 - 1) × 格高
    let last_top = pending.start_y + cell_height * (rows_spanned - 1.0);
    let (start_frac, end_frac) =
        box_drawing::vertical_run_extent(pending.top_extends, pending.bottom_extends);
    let start_y = pending.start_y + cell_height * f32::from(start_frac);
    let end_y = last_top + cell_height * f32::from(end_frac);
    let cx = origin.x + cell_width * (col as f32 + 0.5);
    box_drawing::draw_vertical_span(
        cx,
        start_y,
        end_y,
        pending.weight,
        cell_width,
        pending.color,
        window,
    );
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
            resolved_family: None,
            fallback_notice_emitted: false,
            needs_measurement: true,
            fonts: RefCell::new([const { None }; 8]),
            fonts_dirty: std::cell::Cell::new(false),
            vrun_state: RefCell::new(Vec::new()),
        }
    }

    /// 按样式取字体（带 8 槽缓存）。
    ///
    /// 查询侧读 [`Self::ligatures`] **选槽**（运行时状态）；重建侧
    /// [`Self::construct_slot_font`] 按槽位自身的 ligatures 位构造（缓存
    /// 内容）——两侧职责分离：连字翻转只是换槽，旧槽内容原样保留可复用，
    /// features 永远与所在槽位一致。命中时零分配（family String 与
    /// FontFallbacks 仅在重建时构造一次）。
    pub fn font_for(&self, bold: bool, italic: bool) -> Font {
        if self.fonts_dirty.get() {
            // 置脏自闭合：family/fallbacks/resolved_family 变化统一在此生效
            let mut slots = self.fonts.borrow_mut();
            for slot in slots.iter_mut() {
                *slot = None;
            }
            drop(slots);
            self.fonts_dirty.set(false);
        }
        let idx = font_slot_index(bold, italic, self.ligatures);
        if let Some(font) = self.fonts.borrow()[idx].clone() {
            return font;
        }
        let font = self.construct_slot_font(idx);
        self.fonts.borrow_mut()[idx] = Some(font.clone());
        font
    }

    /// 按槽位构造 Font（重建侧）：features 必须由**该槽自身的 ligatures 位**
    ///（idx bit2）决定，禁止读 self.ligatures——ligatures 变更不触发
    /// invalidate_measurement（连字不影响单元格度量，view 层有意不重测），
    /// 若重建读运行时值，连字切换后 8 槽 features 一起错且永不自愈。
    fn construct_slot_font(&self, idx: usize) -> Font {
        let family = self
            .resolved_family
            .clone()
            .unwrap_or_else(|| self.font_family.clone());
        build_font(
            &family,
            &self.font_fallbacks,
            idx & 0b100 != 0,
            idx & 0b001 != 0,
            idx & 0b010 != 0,
        )
    }

    /// Whether the renderer needs fresh font metrics.
    pub fn needs_measurement(&self) -> bool {
        self.needs_measurement
    }

    /// 探测选定的实际渲染 family（Some = 用户字体不可用，已回退）。
    /// 仅测试断言使用；非测试构建无调用方（平台差异，见 workspace 同款标注）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn resolved_family(&self) -> Option<&str> {
        self.resolved_family.as_deref()
    }

    /// Mark the cached cell metrics as stale.
    ///
    /// 同步置脏 8 槽字体缓存：view 层的 family/fallbacks 变化统一走这里
    ///（view.rs 仅有的两处调用点），缓存失效不需要 view 层感知。
    pub fn invalidate_measurement(&mut self) {
        self.needs_measurement = true;
        self.fonts_dirty.set(true);
    }

    /// Measure cell dimensions based on actual font metrics.
    ///
    /// 先探测用户配置 family 是否为真等宽（GPUI 文本系统对缺失字体会按
    /// 字符类别走不同回退，M 与数字 advance 不一致 → prompt token 被列距
    /// 推开形成大空隙、整体观感显著差于原生终端）。不可用时按平台顺序
    /// 探测内置等宽候选，选定后经 [`Self::resolved_family`] 供全部渲染
    /// 使用；全部候选失败则沿用数字均值（旧行为）。
    ///
    /// # Arguments
    ///
    /// * `window` - The GPUI window for text system access
    pub fn measure_cell(&mut self, window: &mut Window) {
        let mut candidates = vec![self.font_family.clone()];
        candidates.extend(platform_monospace_candidates());
        candidates.dedup();
        // 平台 family 名单预检：GPUI/CoreText 对未知 family 会静默落到
        // 缺省字形（notdef 等宽），单靠 advance 探测识别不出来；先查
        // all_font_names 再探测。
        let available = window.text_system().all_font_names();

        let mut selected: Option<(String, Pixels, Pixels, Pixels)> = None;
        for family in &candidates {
            let is_generic = family == "monospace";
            if !is_generic
                && !available
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(family))
            {
                continue;
            }
            let font = build_font(family, &self.font_fallbacks, self.ligatures, false, false);
            if let Some((advance, ascent, descent)) = probe_monospace(window, &font, self.font_size)
            {
                selected = Some((family.clone(), advance, ascent, descent));
                break;
            }
        }

        let Some((family, advance, ascent, descent)) = selected else {
            // 全部候选（含平台兜底）都非等宽：沿用用户字体 + 数字均值，
            // 不崩但观感受限——与旧口径一致。
            let sample = "0123456789";
            let sample_chars = sample.chars().count() as f32;
            let font = self.font_for(false, false);
            let text_run = TextRun {
                len: sample.len(),
                font,
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped =
                window
                    .text_system()
                    .shape_line(sample.into(), self.font_size, &[text_run], None);
            let measured_width = shaped.width / sample_chars;
            if measured_width > px(0.0) {
                self.cell_width = measured_width;
            }
            let line_height = (shaped.ascent + shaped.descent).ceil();
            if line_height > px(0.0) {
                self.cell_height = line_height * self.line_height_multiplier.max(0.1);
            }
            self.resolved_family = None;
            // 探测落定也是字体缓存的失效点：resolved_family 变化（含 Some→None）
            // 必须让 8 槽在下一次 font_for 重建
            self.fonts_dirty.set(true);
            self.needs_measurement = false;
            return;
        };

        if family != self.font_family && !self.fallback_notice_emitted {
            eprintln!(
                "字体 {:?} 不可用（未安装或非等宽），已回退到 {:?}；可在设置中更改终端字体",
                self.font_family, family
            );
            self.fallback_notice_emitted = true;
        }
        self.resolved_family = Some(family);
        // 探测落定：resolved_family 变化必须让 8 槽字体缓存重建
        self.fonts_dirty.set(true);
        self.cell_width = advance;
        let line_height = (ascent + descent).ceil();
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
    /// * `memo` - 帧内颜色解析 memo（跨行复用，随帧释放）
    /// * `scratch` - 行级布局缓冲：入口清空（容量保留）、填充后经只读
    ///   访问器 [`RowScratch::backgrounds`] / [`RowScratch::text_runs`]
    ///   取用；调用方跨行复用同一实例
    pub fn layout_row<'a>(
        &self,
        row: usize,
        cells: impl Iterator<Item = (usize, &'a Cell)>,
        colors: &Colors,
        default_bg: Hsla,
        memo: &mut ColorMemo,
        scratch: &mut RowScratch,
    ) {
        // 行级缓冲复用：清空（容量保留）后填充
        scratch.clear();

        let mut current_run: Option<BatchedTextRun> = None;
        let mut current_bg: Option<BackgroundRect> = None;

        for (col, cell) in cells {
            // Skip wide-character spacers; the leading cell owns both columns.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                if let Some(run) = current_run.take() {
                    scratch.text_runs.push(run);
                }
                continue;
            }

            // Extract cell styling（帧内 memo：命中零成本）
            let fg_color = resolve_cached(memo, &self.palette, cell.fg, colors);
            let bg_color = resolve_cached(memo, &self.palette, cell.bg, colors);
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
                .map(|color| resolve_cached(memo, &self.palette, color, colors));

            // Get the character (or space if empty)
            let ch = if cell.c == ' ' || cell.c == '\0' {
                ' '
            } else {
                cell.c
            };

            // 默认背景由整行底色覆盖，不创建逐格背景矩形。
            if bg_color == default_bg {
                if let Some(bg_rect) = current_bg.take() {
                    scratch.backgrounds.push(bg_rect);
                }
            } else if let Some(ref mut bg_rect) = current_bg {
                if bg_rect.color == bg_color && bg_rect.end_col == col {
                    bg_rect.end_col = col + 1;
                } else {
                    scratch.backgrounds.push(bg_rect.clone());
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
                    scratch.text_runs.push(run);
                }
                continue;
            }

            // Handle text runs; custom box-drawing passes own these glyphs.
            if box_drawing::is_box_drawing_char(cell.c) {
                if let Some(run) = current_run.take() {
                    scratch.text_runs.push(run);
                }
                continue;
            }
            match current_run.take() {
                Some(mut run)
                    if run.fg_color == fg_color
                        && run.bg_color == bg_color
                        && run.bold == bold
                        && run.italic == italic
                        && run.underline == underline
                        && run.underline_color == underline_color
                        && run.end_col == col =>
                {
                    // Extend current run（原地追加，零分配）
                    run.text.push(ch);
                    run.end_col = col + char_columns(ch);
                    current_run = Some(run);
                }
                Some(run) => {
                    // 断点：旧 run 整体移入列表（消除 run.clone() 的 O(n)
                    // 深拷贝）；新 run 首字符走暂存缓冲——clear + push 后
                    // mem::take 整体移交，消除逐格 ch.to_string() 分配
                    scratch.text_runs.push(run);
                    scratch.text.clear();
                    scratch.text.push(ch);
                    current_run = Some(BatchedTextRun {
                        text: std::mem::take(&mut scratch.text),
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
                None => {
                    // Start new run（同断点路径：暂存缓冲整体移交）
                    scratch.text.clear();
                    scratch.text.push(ch);
                    current_run = Some(BatchedTextRun {
                        text: std::mem::take(&mut scratch.text),
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
        }

        // Push final run and background
        if let Some(run) = current_run {
            scratch.text_runs.push(run);
        }
        if let Some(bg) = current_bg {
            scratch.backgrounds.push(bg);
        }

        // Merge adjacent backgrounds with same color（原地合并，复用缓冲容量）
        self.merge_backgrounds(&mut scratch.backgrounds);
    }

    /// Merge adjacent background rects with same color（原地）。
    ///
    /// This optimization reduces the number of rectangles to paint by
    /// combining horizontally adjacent rectangles that share the same color.
    /// 双指针原地压实，不新分配 Vec（调用方缓冲跨行复用）。
    fn merge_backgrounds(&self, rects: &mut Vec<BackgroundRect>) {
        if rects.len() < 2 {
            return;
        }

        let mut write = 0;
        for read in 1..rects.len() {
            if rects[write].can_merge_with(&rects[read]) {
                rects[write].end_col = rects[read].end_col;
            } else {
                write += 1;
                rects[write] = rects[read].clone();
            }
        }
        rects.truncate(write + 1);
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

        // 帧内颜色解析 memo：随帧创建/释放（终端唯一色几十个，线性查即可）
        let mut color_memo: ColorMemo = Vec::new();

        // 帧级竖直 run 列状态：按列数 clear+resize（容量跨帧保留，逐帧零分配）
        {
            let mut vrun = self.vrun_state.borrow_mut();
            vrun.clear();
            vrun.resize_with(frame.columns, || None);
        }

        // 行级布局缓冲：循环外创建一次，每行 layout_row 入口 clear() 复用
        let mut scratch = RowScratch::default();

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
            // 借用迭代：消除快照前「进 cells Vec 一次 + 值迭代再 clone 一次」的第二遍复制；
            // 行级缓冲经 scratch 复用（每行 clear，容量保留）。
            self.layout_row(
                line_idx,
                row_cells.iter().enumerate(),
                colors,
                default_bg,
                &mut color_memo,
                &mut scratch,
            );

            // Paint backgrounds
            for bg_rect in scratch.backgrounds() {
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
            // 列覆盖改用位图（clear+resize 复用缓冲），替代逐行 HashSet 分配
            scratch.horizontal_flags.clear();
            scratch.horizontal_flags.resize(row_cells.len(), false);

            let mut i = 0;
            while i < row_cells.len() {
                let cell = &row_cells[i];
                let col_idx = i;
                let ch = cell.c;

                // Check if this starts a horizontal span
                if let Some(weight) = box_drawing::get_horizontal_weight(ch) {
                    let fg_color = resolve_cached(&mut color_memo, &self.palette, cell.fg, colors);
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
                        let next_fg =
                            resolve_cached(&mut color_memo, &self.palette, next_cell.fg, colors);
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
                        scratch.horizontal_flags[col] = true;
                    }

                    // Skip past this span
                    i = j;
                    continue;
                }
                i += 1;
            }

            // Second pass: 竖直分量按列跨行合并（帧级列状态 vrun_state），
            // 同列相邻格粗细/颜色一致即延长，冲刷时整段 draw_vertical_span
            //（消除逐格 tessellation/逐格 quad）；不参与合并的残段逐格走
            // 原路径，绘制语义与逐格口径一致（几何对照见 box_drawing 单测）。
            {
                let mut vrun = self.vrun_state.borrow_mut();
                for (col_idx, cell) in row_cells.iter().enumerate() {
                    let ch = cell.c;

                    // 空格/普通字符：切断该列 run（y 不连续）；普通字符本体
                    // 由第三遍文本 run 绘制
                    if ch == ' ' || ch == '\0' || !box_drawing::is_box_drawing_char(ch) {
                        flush_vrun(
                            &mut vrun,
                            col_idx,
                            origin,
                            self.cell_width,
                            self.cell_height,
                            window,
                        );
                        continue;
                    }

                    // 圆角（╭╮╯╰）是曲线字符：短路在 v_weight 计算之前——
                    // corner 的竖直半边若并入 run 会被画成直线端点，弧线退化
                    // 成直角（draw_rounded_corner 仅 draw_box_character 可达）。
                    // 与旧逐格路径同口径：冲刷该列 pending 后整格走
                    // draw_box_character 画二次曲线，不新起 pending。圆角横段
                    // 是单侧的（corner_tl: left=None/right=Some），不满足
                    // get_horizontal_weight 的双侧同粗判定，永不进第一遍水平
                    // 位图，无需判 horizontal_flags
                    if box_drawing::is_rounded_corner(ch) {
                        flush_vrun(
                            &mut vrun,
                            col_idx,
                            origin,
                            self.cell_width,
                            self.cell_height,
                            window,
                        );
                        box_drawing::draw_box_character(
                            ch,
                            Bounds {
                                origin: Point {
                                    x: origin.x + self.cell_width * (col_idx as f32),
                                    y: y_base,
                                },
                                size: Size {
                                    width: self.cell_width,
                                    height: self.cell_height,
                                },
                            },
                            resolve_cached(&mut color_memo, &self.palette, cell.fg, colors),
                            self.cell_width,
                            window,
                        );
                        continue;
                    }

                    // 斜线/曲线字符（get_box_segments 返回 None）：不参与
                    // 合并，也不在此补段——与历史逐格路径一致（原
                    // draw_box_character 对 None 直接返回，不产出墨迹），
                    // 仅需冲刷该列 pending
                    let Some(segments) = box_drawing::get_box_segments(ch) else {
                        flush_vrun(
                            &mut vrun,
                            col_idx,
                            origin,
                            self.cell_width,
                            self.cell_height,
                            window,
                        );
                        continue;
                    };

                    let fg_color = resolve_cached(&mut color_memo, &self.palette, cell.fg, colors);

                    // 竖直分量粗细：上下等粗取该粗细；半线取单边粗细；
                    // 异粗（╿ 等）无法并入单粗细 run，走逐格原路径
                    let v_weight = match (segments.top, segments.bottom) {
                        (Some(t), Some(b)) if t == b => Some(t),
                        (Some(t), None) => Some(t),
                        (None, Some(b)) => Some(b),
                        _ => None,
                    };

                    let x = origin.x + self.cell_width * (col_idx as f32);
                    let cell_bounds = Bounds {
                        origin: Point { x, y: y_base },
                        size: Size {
                            width: self.cell_width,
                            height: self.cell_height,
                        },
                    };

                    match v_weight {
                        Some(weight) => {
                            // 与该列 pending 同粗细同色且行连续 → 延长。
                            // 延长格必须上下贯通（top&bottom 均存在）：半线/
                            // 角格只作 run 的首/末格（端点收缩半格），作中段
                            // 会让整段覆盖其空白半格、偏离逐格墨迹并集
                            let joins = segments.top.is_some()
                                && segments.bottom.is_some()
                                && match &vrun[col_idx] {
                                    Some(pending) => {
                                        pending.weight == weight
                                            && pending.color == fg_color
                                            && pending.next_row == line_idx
                                    }
                                    None => false,
                                };
                            if joins {
                                let pending = vrun[col_idx].as_mut().expect("joins 已判定槽位存在");
                                pending.bottom_extends = segments.bottom.is_some();
                                pending.next_row = line_idx + 1;
                            } else {
                                // 粗细/颜色变化或 y 不连续：冲刷旧段并起新段
                                //（被水平位图覆盖的格同样只以竖直分量入段）
                                flush_vrun(
                                    &mut vrun,
                                    col_idx,
                                    origin,
                                    self.cell_width,
                                    self.cell_height,
                                    window,
                                );
                                vrun[col_idx] = Some(VPending {
                                    start_y: y_base,
                                    weight,
                                    color: fg_color,
                                    top_extends: segments.top.is_some(),
                                    bottom_extends: segments.bottom.is_some(),
                                    first_row: line_idx,
                                    next_row: line_idx + 1,
                                });
                            }
                            // 本格竖直分量已并入 run（冲刷时整段绘制）；这里
                            // 只补画第一遍未覆盖的水平残段（如 ┌ 的右短横）
                            if !scratch.horizontal_flags[col_idx] {
                                box_drawing::draw_horizontal_components(
                                    ch,
                                    cell_bounds,
                                    fg_color,
                                    self.cell_width,
                                    window,
                                );
                            }
                        }
                        None => {
                            // 异粗竖直：冲刷后逐格原路径（内部分段粗细不同）
                            flush_vrun(
                                &mut vrun,
                                col_idx,
                                origin,
                                self.cell_width,
                                self.cell_height,
                                window,
                            );
                            if scratch.horizontal_flags[col_idx] {
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
                        }
                    }
                }
            }

            // Third pass: shape and paint contiguous regular-text runs at once.
            // This avoids one rustybuzz/cosmic-text shaping call per cell per pane.
            // drain 整体移出：run 的 String 直接移交 shape_line（SharedString），
            // 零克隆；Vec 容量随 drain 保留供下一行复用。
            for run in scratch.text_runs.drain(..) {
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
                                // 不乘 background_opacity：与直线/波浪字形线、
                                // URL 悬停下划线同口径（下划线是内容的一部分，
                                // 不随窗口透明度变淡）
                                run.underline_color.unwrap_or(run.fg_color),
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

        // 帧末：冲刷全部列残余竖直 run（扫到最后一行仍未断链的段），
        // 光标之前绘制，保持光标在网格内容之上
        {
            let mut vrun = self.vrun_state.borrow_mut();
            for col in 0..vrun.len() {
                flush_vrun(
                    &mut vrun,
                    col,
                    origin,
                    self.cell_width,
                    self.cell_height,
                    window,
                );
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

        // 新借用迭代（快照行直引；行级缓冲复用）
        let mut memo: ColorMemo = Vec::new();
        let mut scratch_a = RowScratch::default();
        renderer.layout_row(
            0,
            cells.iter().enumerate(),
            &colors,
            default_bg,
            &mut memo,
            &mut scratch_a,
        );
        // 旧值迭代语义：先克隆成拥有值再借引用迭代 —— 输出必须一致
        let owned: Vec<(usize, Cell)> = cells.iter().cloned().enumerate().collect();
        let mut scratch_b = RowScratch::default();
        renderer.layout_row(
            0,
            owned.iter().map(|(col, cell)| (*col, cell)),
            &colors,
            default_bg,
            &mut memo,
            &mut scratch_b,
        );
        assert_eq!(
            scratch_a.backgrounds(),
            scratch_b.backgrounds(),
            "背景与旧值迭代一致"
        );
        assert_eq!(
            scratch_a.text_runs(),
            scratch_b.text_runs(),
            "文本 run 与旧值迭代一致"
        );

        // 字面期望：abc 单 run；空格切断；「中」占两列；box-drawing 不进 run；
        // X/Y 同样式合并；z 样式不同自成 run
        let runs: Vec<(usize, usize, &str)> = scratch_a
            .text_runs()
            .iter()
            .map(|run| (run.start_col, run.end_col, run.text.as_str()))
            .collect();
        assert_eq!(
            runs,
            vec![(0, 3, "abc"), (4, 6, "中"), (7, 9, "XY"), (9, 10, "z")]
        );
        // 背景合并：col7-8 蓝底合并为单矩形
        let backgrounds = scratch_a.backgrounds();
        assert_eq!(backgrounds.len(), 1);
        assert_eq!((backgrounds[0].start_col, backgrounds[0].end_col), (7, 9));
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

        let mut rects = vec![
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

        // 原地合并：缓冲由调用方持有复用
        renderer.merge_backgrounds(&mut rects);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].start_col, 0);
        assert_eq!(rects[0].end_col, 10);
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
        let mut memo: ColorMemo = Vec::new();
        let mut scratch = RowScratch::default();
        renderer.layout_row(
            0,
            cells.iter().enumerate(),
            &colors,
            default_bg,
            &mut memo,
            &mut scratch,
        );
        let runs = scratch.text_runs();
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
        let mut scratch = RowScratch::default();
        renderer.layout_row(
            0,
            cells.iter().enumerate(),
            &colors,
            default_bg,
            &mut memo,
            &mut scratch,
        );
        let runs = scratch.text_runs();
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
        let mut memo: ColorMemo = Vec::new();
        let mut scratch = RowScratch::default();
        renderer.layout_row(
            0,
            cells.iter().enumerate(),
            &colors,
            default_bg,
            &mut memo,
            &mut scratch,
        );
        let runs = scratch.text_runs();
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

    // ───────── Font 8 槽缓存（槽位索引 / 连字翻转 / 置脏闭合） ─────────

    #[test]
    fn font_slot_index_layout() {
        // bit0=bold、bit1=italic、bit2=ligatures
        assert_eq!(font_slot_index(false, false, false), 0);
        assert_eq!(font_slot_index(true, false, false), 1);
        assert_eq!(font_slot_index(false, true, false), 2);
        assert_eq!(font_slot_index(true, true, false), 3);
        assert_eq!(font_slot_index(false, false, true), 4);
        assert_eq!(font_slot_index(true, false, true), 5);
        assert_eq!(font_slot_index(false, true, true), 6);
        assert_eq!(font_slot_index(true, true, true), 7);
    }

    #[test]
    fn font_slots_switch_features_and_retain() {
        let mut renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.0,
            ColorPalette::default(),
            false, // 从连字关闭起步
            Vec::new(),
            CursorStyle::Block,
        );
        // 连字关闭期构造的槽（bit2=0）→ disable_ligatures
        let off_plain = renderer.font_for(false, false);
        let off_bold = renderer.font_for(true, false);
        assert_eq!(off_plain.features, FontFeatures::disable_ligatures());
        assert_eq!(off_bold.features, FontFeatures::disable_ligatures());

        // 开启连字：仅换槽（bit2=1），新槽必须按槽位自身位构造 default——
        // ligatures 变更不触发 invalidate_measurement（view.rs 有意不重测），
        // 若重建读运行时值或翻转时错误清槽，此处即抓到
        renderer.ligatures = true;
        let on_plain = renderer.font_for(false, false);
        let on_bold = renderer.font_for(true, false);
        assert_eq!(on_plain.features, FontFeatures::default());
        assert_eq!(on_bold.features, FontFeatures::default());
        assert_eq!(on_bold.weight, FontWeight::BOLD);

        // 翻回：旧槽保留可复用（features 仍随槽位）
        renderer.ligatures = false;
        assert_eq!(renderer.font_for(false, false), off_plain);
        assert_eq!(renderer.font_for(true, false), off_bold);

        // 置脏自闭合：invalidate_measurement（view 层 family/fallbacks 变化
        // 的统一入口）使槽位重建，新 family 生效
        renderer.font_family = "Fira Code".to_string();
        renderer.invalidate_measurement();
        let rebuilt = renderer.font_for(false, false);
        assert_eq!(rebuilt.family.as_str(), "Fira Code");
        assert_eq!(rebuilt.features, FontFeatures::disable_ligatures());
    }

    // ───────── 颜色解析 memo（与 palette.resolve 等值） ─────────

    #[test]
    fn resolve_cached_matches_palette_and_memoizes() {
        let renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.0,
            ColorPalette::default(),
            true,
            Vec::new(),
            CursorStyle::Block,
        );
        // OSC 4 覆盖索引色 1（改写为绿）：memo 仅以终端色为键，
        // 动态覆盖下也必须与 palette.resolve 逐色等值
        use alacritty_terminal::vte::ansi::Rgb;
        let mut colors = Colors::default();
        colors[1] = Some(Rgb {
            r: 0x00,
            g: 0xFF,
            b: 0x00,
        });
        let probe = [
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Red),
            Color::Indexed(1),
            Color::Indexed(42),
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
        ];
        let mut memo: ColorMemo = Vec::new();
        for color in probe {
            assert_eq!(
                resolve_cached(&mut memo, &renderer.palette, color, &colors),
                renderer.palette.resolve(color, &colors),
                "memo 解析必须与 palette.resolve 等值: {color:?}"
            );
        }
        assert_eq!(memo.len(), probe.len(), "首轮每色各入表一次");
        // 二轮全命中：memo 不增长且结果仍等值
        for color in probe {
            assert_eq!(
                resolve_cached(&mut memo, &renderer.palette, color, &colors),
                renderer.palette.resolve(color, &colors),
                "二轮命中必须等值: {color:?}"
            );
        }
        assert_eq!(memo.len(), probe.len(), "命中不得重复入表");
    }

    // ───────── 行级缓冲复用（RowScratch 跨行独立） ─────────

    #[test]
    fn layout_row_scratch_reuse_rows_stay_independent() {
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
        let mut memo: ColorMemo = Vec::new();
        let mut scratch = RowScratch::default();

        let mut row0 = vec![Cell::default(); 4];
        row0[0].c = 'a';
        row0[1].c = 'b';
        let mut row1 = vec![Cell::default(); 4];
        row1[0].c = 'x';
        row1[1].c = 'y';
        row1[2].c = 'z';

        renderer.layout_row(
            0,
            row0.iter().enumerate(),
            &colors,
            default_bg,
            &mut memo,
            &mut scratch,
        );
        let texts: Vec<&str> = scratch
            .text_runs()
            .iter()
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(texts, vec!["ab"]);

        // 同一缓冲 clear 后布局下一行：不得带上一行残留
        renderer.layout_row(
            1,
            row1.iter().enumerate(),
            &colors,
            default_bg,
            &mut memo,
            &mut scratch,
        );
        let texts: Vec<&str> = scratch
            .text_runs()
            .iter()
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(texts, vec!["xyz"], "缓冲复用不得跨行残留");
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
