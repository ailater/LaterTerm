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
//! let mut terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);
//!
//! // Process some output (e.g., colored text)
//! terminal.process_bytes(b"\x1b[31mRed text\x1b[0m");
//! ```
//!
//! [`process_bytes`]: TerminalState::process_bytes

use crate::event::GpuiEventProxy;
use crate::osc::{OscEvent, OscSink, PromptDirection, PromptMark, PromptTracker};
use alacritty_terminal::grid::{Dimensions, Scroll};
// Grid<Cell> 类型签名仅剩测试对照 oracle（search_grid_*）引用
#[cfg(test)]
use alacritty_terminal::grid::Grid;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::term::cell::Flags;
// Cell 类型签名仅剩测试对照 oracle（search_grid_*）引用
#[cfg(test)]
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use parking_lot::Mutex;
use regex::Regex;
use std::collections::VecDeque;
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

/// 搜索选项（[`TerminalState::search_with_options`] 用）。
/// 默认值等价于 [`TerminalState::search`]（大小写不敏感的普通子串搜索）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    /// 大小写敏感。
    pub case_sensitive: bool,
    /// 按正则表达式匹配（regex crate 语法；开启时全字选项忽略，用户自写 `\b`）。
    pub regex: bool,
    /// 全字匹配（仅子串路径生效；词定义与正则 `\b` 一致：字母数字或下划线）。
    pub whole_word: bool,
}

/// 回滚搜索快照：[`TerminalState::snapshot_for_search`] 单次持锁构建的
/// 网格扁平化文本，交 [`scan_snapshot`] 在后台线程扫描（匹配全程不持
/// term 锁，锁窗口从同步实现的「构建+逐行匹配」缩为「纯复制」）。
///
/// 紧凑编码：
/// - `text`：全部绝对行文本顺序拼接（UTF-8）。空 cell 填充 `'\0'` 规范化为
///   `' '`——渲染侧按空格绘制 NUL 格（render.rs 单元绘制 `'\0'`→`' '`），
///   旧同步实现 push 原值属怪癖；规范化后搜索与渲染口径一致。含空格
///   needle 在 NUL 填充区的命中行为与旧实现存在微差（有意统一，单测
///   `snapshot_normalizes_nul_padding_to_space` 固化，不宣称逐位等值）；
/// - `line_ranges[i]`：第 i 个绝对行在 `text` 中的字节区间 `[start, end)`；
/// - `wide_maps`：仅记录含宽字符（WIDE_CHAR / WIDE_CHAR_SPACER）行的
///   「字符索引 → 起始列」全量映射（按绝对行升序）；无宽字符行恒等映射
///   （字符索引即列号），零额外内存。
///
/// 内存账（默认 scrollback = 100_000 行）：典型 ~40 字符行 ≈ 6-8MB/窗格；
/// 最坏满列 200 列 ASCII ≈ 22MB、满列 CJK ≈ 60MB 上界。扫描完成即随
/// 任务释放；视图层单飞（在飞合并 pending）保证同屏至多一份快照。
pub struct SearchSnapshot {
    /// 全部绝对行的扁平行文本（见结构注记的 NUL 规范化口径）。
    text: String,
    /// 每个绝对行的字节区间（与行序一一对应，长度 = total_lines）。
    line_ranges: Vec<(u32, u32)>,
    /// 含宽字符行的「字符索引 → 起始列」映射，键为绝对行号（升序）。
    wide_maps: Vec<(u32, Vec<(u32, u32)>)>,
    /// 网格列数（宽字符行末字符占宽兜底计算用）。
    columns: usize,
    /// 构建时的可见行数（诊断与未来分块扫描留档，扫描本体不消费）。
    #[allow(dead_code)]
    screen_lines: usize,
    /// 构建时的总行数（扫描循环按行号取 line_ranges，长度冗余校验用）。
    total_lines: usize,
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

/// 全字词字符判定：字母数字或下划线，与正则 `\b` 的 Unicode 词定义一致
/// （汉字属词字符，`\b中文\b` 在 CJK 相邻文本间不命中，同 grep -w 口径）。
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// 全字边界判定：命中首字符之前与末字符之后都不是词字符（行首尾为边界）。
/// 行尾的空 cell 填充空格天然不是词字符，无需裁剪。
fn is_word_boundary(text: &[char], start: usize, len: usize) -> bool {
    let prev_ok = start == 0 || !is_word_char(text[start - 1]);
    let end = start + len;
    let next_ok = end >= text.len() || !is_word_char(text[end]);
    prev_ok && next_ok
}

// ───────────── 快照扫描（生产路径：纯函数，后台线程执行） ─────────────

/// 在快照上执行搜索（纯函数，无锁无共享状态，供后台线程调用）。
///
/// `needle` 口径随模式分流（与旧同步实现的备料一致）：
/// - 子串/全字模式：已按 `case_sensitive` 备好的 needle 字符序列
///   （大小写不敏感时调用方先 `to_lowercase`）；
/// - 正则模式：原样查询字符序列（扫描器内拼回字符串再编译，大小写
///   不敏感加 `(?i)` 前缀；非法正则 `Err` 快速失败，错误信息与旧实现
///   同口径——regex 多行报告取尾行摘要并截断到 60 字符）。
///
/// 匹配口径与迁移前的网格直扫实现（现 `#[cfg(test)]` oracle：
/// search_grid / search_grid_exact / search_grid_regex）逐行等价；
/// 唯一有意差异是 NUL 规范化，见 [`SearchSnapshot`] 注记。
pub fn scan_snapshot(
    snapshot: &SearchSnapshot,
    needle: &[char],
    options: SearchOptions,
) -> Result<Vec<SearchMatch>, String> {
    // 构建不变式：行区间表与总行数一一对应（debug 校验，release 零开销）
    debug_assert_eq!(snapshot.line_ranges.len(), snapshot.total_lines);
    if options.regex {
        let pattern_source: String = needle.iter().collect();
        let pattern = if options.case_sensitive {
            Regex::new(&pattern_source)
        } else {
            Regex::new(&format!("(?i){pattern_source}"))
        };
        let pattern = pattern.map_err(|error| {
            // regex 的 Display 是多行 caret 对齐报告（含 (?i) 前缀回显），
            // 搜索栏只容单行：取最后一行（"error: …" 摘要）并截断。
            let text = error.to_string();
            let line = text.lines().last().unwrap_or(&text).trim();
            const MAX: usize = 60;
            if line.chars().count() <= MAX {
                line.to_string()
            } else {
                let mut cut: String = line.chars().take(MAX).collect();
                cut.push('…');
                cut
            }
        })?;
        Ok(scan_snapshot_regex(snapshot, &pattern))
    } else {
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        Ok(scan_snapshot_substring(
            snapshot,
            needle,
            options.case_sensitive,
            options.whole_word,
        ))
    }
}

/// 快照行的列换算：非宽行恒等映射（字符索引即网格列），宽行查
/// wide_maps。返回 `(起始列, 该字符占宽)`；`char_index` 越界返回 `None`
///（正则字节区间换算的防御护栏，正常匹配不会触发）。
fn snapshot_column_span(
    snapshot: &SearchSnapshot,
    map: Option<&[(u32, u32)]>,
    char_index: usize,
) -> Option<(usize, usize)> {
    match map {
        None => Some((char_index, 1)),
        Some(map) => {
            let column = map.get(char_index)?.1 as usize;
            // 占宽：下一字符起始列差 2 即宽字符（续列是 spacer）；行末
            // 字符按剩余列数兜底，恒 ≥1
            let width = map
                .get(char_index + 1)
                .map(|(_, next)| (*next as usize) - column)
                .unwrap_or_else(|| snapshot.columns - column)
                .max(1);
            Some((column, width))
        }
    }
}

/// 宽行列映射查找：wide_maps 按绝对行升序构建，二分定位；非宽行返回
/// `None`（调用方走恒等映射）。
fn snapshot_wide_map(snapshot: &SearchSnapshot, line: usize) -> Option<&[(u32, u32)]> {
    snapshot
        .wide_maps
        .binary_search_by_key(&(line as u32), |entry| entry.0)
        .ok()
        .map(|index| snapshot.wide_maps[index].1.as_slice())
}

/// 正则扫描快照。锚点语义与网格直扫 oracle 一致：`$` = 每行最后一个
/// 非空白字符之后（行尾空白先裁剪，行文本与列映射同步收缩）、`^` =
/// 每个网格行列 0、零宽命中过滤（命中语义 = 非空匹配）。
fn scan_snapshot_regex(snapshot: &SearchSnapshot, pattern: &Regex) -> Vec<SearchMatch> {
    let mut matches = Vec::new();
    for (line, &(start, end)) in snapshot.line_ranges.iter().enumerate() {
        // 尾部空白裁剪：oracle 逐字符 pop；trim_end 同为 Unicode 空白口径，
        // 等价（行首对齐不受影响，裁剪只收缩尾部）。
        let line_text = snapshot.text[start as usize..end as usize].trim_end();
        if line_text.is_empty() {
            continue;
        }
        let map = snapshot_wide_map(snapshot, line);
        for found in pattern.find_iter(line_text) {
            let start_char = line_text[..found.start()].chars().count();
            let end_char = line_text[..found.end()].chars().count();
            // 零宽命中过滤：如 a* 在词边界处的空匹配，无高亮意义
            if end_char == start_char {
                continue;
            }
            let last_char = end_char - 1;
            let (Some((column, _)), Some((last, last_width))) = (
                snapshot_column_span(snapshot, map, start_char),
                snapshot_column_span(snapshot, map, last_char),
            ) else {
                continue;
            };
            matches.push(SearchMatch {
                line,
                column,
                len: last + last_width - column,
            });
        }
    }
    matches
}

/// 子串/全字扫描快照。大小写不敏感经逐字符 `to_lowercase` 展开（'İ' 等
/// 一对多展开与「源字符索引」同步构建，列换算经索引中转），与 oracle
/// search_grid 同式；全字边界判定用（可能的）展开文本，同式。
fn scan_snapshot_substring(
    snapshot: &SearchSnapshot,
    needle: &[char],
    case_sensitive: bool,
    whole_word: bool,
) -> Vec<SearchMatch> {
    let mut matches = Vec::new();
    for (line, &(start, end)) in snapshot.line_ranges.iter().enumerate() {
        let line_text = &snapshot.text[start as usize..end as usize];
        if line_text.is_empty() {
            continue;
        }
        // haystack（匹配域）与 src（haystack 字符 → 源字符索引）同步构建；
        // 大小写敏感时二者恒等映射
        let mut haystack: Vec<char> = Vec::with_capacity(line_text.len());
        let mut src: Vec<u32> = Vec::with_capacity(line_text.len());
        if case_sensitive {
            for (index, ch) in line_text.chars().enumerate() {
                haystack.push(ch);
                src.push(index as u32);
            }
        } else {
            for (index, ch) in line_text.chars().enumerate() {
                for lower in ch.to_lowercase() {
                    haystack.push(lower);
                    src.push(index as u32);
                }
            }
        }
        if haystack.len() < needle.len() {
            continue;
        }
        let map = snapshot_wide_map(snapshot, line);
        let mut start_idx = 0;
        while start_idx + needle.len() <= haystack.len() {
            if haystack[start_idx..start_idx + needle.len()] == *needle
                && (!whole_word || is_word_boundary(&haystack, start_idx, needle.len()))
            {
                let first_src = src[start_idx] as usize;
                let last_src = src[start_idx + needle.len() - 1] as usize;
                let (Some((first, _)), Some((last, last_width))) = (
                    snapshot_column_span(snapshot, map, first_src),
                    snapshot_column_span(snapshot, map, last_src),
                ) else {
                    start_idx += 1;
                    continue;
                };
                matches.push(SearchMatch {
                    line,
                    column: first,
                    len: last + last_width - first,
                });
                // 命中后按 needle 长度步进（非重叠命中，与 oracle 同口径）
                start_idx += needle.len();
            } else {
                start_idx += 1;
            }
        }
    }
    matches
}

// ───────────── 网格直扫 oracle（仅测试对照保留） ─────────────
//
// 生产搜索已整体迁移到「快照 + 后台扫描」管道（snapshot_for_search +
// scan_snapshot）；下列四个网格直扫函数是迁移前的同步实现，保留为
// #[cfg(test)] 对照 oracle：等值测试用「旧实现 vs 新管道」逐用例比对，
// 保证扫描口径与历史行为严格等同。除测试外无消费者（不参与编译产物）。

/// 正则路径逐行匹配：提取行文本（宽字符续列跳过）→ 裁剪尾部空白 → find_iter。
///
/// 锚点语义（本实现的契约，锚点组单测钉死）：
/// - `$` = 每个网格行**最后一个非空白字符之后**——行尾空 cell 的默认填充
///   ' '（alacritty cell 默认字符）与用户敲入的空格不可区分，匹配前统一
///   裁剪，否则 `foo$`/`\d+$` 等锚定模式系统性失配；代价是 `\s+$` 等
///   显式匹配行尾空白的模式不再命中（有意取舍，行中/行首空白不受影响）；
/// - `^` = 每个网格行的列 0——软换行续行是独立 grid row，`^` 在续行列 0
///   亦命中；
/// - 零宽命中（如 `a*` 的空匹配）一律过滤，命中语义 = 非空匹配。
///
/// 字节偏移换算：regex 返回 UTF-8 字节区间，经 `text[..start].chars().count()`
/// 换算回字符下标，再查 char_columns 得网格列；len 沿用「末字符列 + 宽字符
/// 占宽 − 首列」公式（与子串路径 search_grid_exact 同式）。
#[cfg(test)]
fn search_grid_regex(grid: &Grid<Cell>, pattern: &Regex) -> Vec<SearchMatch> {
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
        // 尾部空白裁剪：行文本与列映射同步收缩，保持下标对位。
        // 填充空格与用户尾随空格不可区分，一并裁剪（见函数级锚点语义注记）。
        while text.last().is_some_and(|ch| ch.is_whitespace()) {
            text.pop();
            char_columns.pop();
        }
        if text.is_empty() {
            continue;
        }
        let line_text: String = text.iter().collect();
        for found in pattern.find_iter(&line_text) {
            let start = line_text[..found.start()].chars().count();
            let end = line_text[..found.end()].chars().count();
            // 零宽命中过滤：如 a* 在词边界处的空匹配，无高亮意义
            if end == start {
                continue;
            }
            let first = char_columns[start];
            let last = char_columns[end - 1];
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
        }
    }
    matches
}

/// 大小写敏感可切换的行搜索（大小写不敏感时与 search_grid 同口径）。
#[cfg(test)]
fn search_grid_sensitivity(
    grid: &Grid<Cell>,
    needle: &[char],
    case_sensitive: bool,
    whole_word: bool,
) -> Vec<SearchMatch> {
    if case_sensitive {
        search_grid_exact(grid, needle, whole_word)
    } else {
        search_grid(grid, needle, whole_word)
    }
}

/// 大小写敏感的行搜索（不 lowercase）。
#[cfg(test)]
fn search_grid_exact(grid: &Grid<Cell>, needle: &[char], whole_word: bool) -> Vec<SearchMatch> {
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
            // 全字模式：在既有命中处加词边界判定（行尾填充空格天然是边界）
            if text[start..start + needle.len()] == *needle
                && (!whole_word || is_word_boundary(&text, start, needle.len()))
            {
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

/// 大小写不敏感的行搜索（逐字符小写化 + 列映射同步展开）。
#[cfg(test)]
fn search_grid(grid: &Grid<Cell>, needle: &[char], whole_word: bool) -> Vec<SearchMatch> {
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
            // 全字模式：边界判定用小写化后的扩展文本（多对一展开时相邻
            // 扩展字符同属一词，逐字符判定结果与原文本一致）
            if haystack[start..start + needle.len()] == *needle
                && (!whole_word || is_word_boundary(&haystack, start, needle.len()))
            {
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

/// 旁路事件入队：容量封顶丢最老（无消费者的最坏情形下限内存，
/// cwd/notify 天然只关心新值）。独立自由函数而非方法：process_bytes 持
/// term 锁期间调用，方法调用会整体借 `self` 与锁守卫冲突（字段级分裂
/// 借用才可行）。
fn queue_osc_event(queue: &mut VecDeque<OscEvent>, event: OscEvent) {
    const OSC_EVENT_QUEUE_CAP: usize = 1024;
    if queue.len() >= OSC_EVENT_QUEUE_CAP {
        queue.pop_front();
    }
    queue.push_back(event);
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

    /// 回滚上限（new 传入，update_term_config 重建 alacritty Config 时回填，
    /// 防 set_options 把滚动上限重置为默认）。
    scrollback: usize,

    /// OSC 旁路解析器（cwd/notify/提示符标记共用底座，见 [`crate::osc`]）。
    /// 与主 parser 喂同一字节流，只认内置 Handler 不路由的 OSC。
    osc_sink: OscSink,

    /// 旁路解析出的非 133 事件（OSC 7/9/777），排队供上层
    /// [`take_osc_events`](Self::take_osc_events) 消费（容量封顶丢最老）。
    osc_events: VecDeque<OscEvent>,

    /// 最近一次 OSC 7 上报的 cwd（file:// 校验 + percent 解码后的绝对路径，
    /// 见 [`parse_osc7_cwd`](crate::osc)）。仅记录，不做本地性判定——
    /// SSH 会话的对外口径由 [`TerminalView::cwd`](crate::view::TerminalView::
    /// cwd) 按后端门控。新标签/分屏继承焦点窗格目录的数据源。
    cwd: Option<String>,

    /// 提示符标记 + 滚动淘汰计数 + 命令运行窗态。独立互斥体、不挂 Term
    /// 互斥体内；锁序固定 **term → tracker**（process_bytes 持 term 锁期间
    /// 末段取本锁；作废路径只取本锁），防交叉加锁死锁。
    tracker: Mutex<PromptTracker>,

    /// 最近一次 OSC 9;4 进度上报（ConEmu）。v1 仅暴露 getter，UI 不呈现。
    progress: Option<crate::osc::ProgressState>,

    /// 桌面通知发送通道（OSC 9/777 → DesktopNotice）。构造后由 TerminalView
    /// 注入；测试场景（无视图）为 None，通知事件仅入 osc_events 队列。
    notice_tx: Option<std::sync::mpsc::Sender<crate::event::TerminalEvent>>,

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
    /// * `kitty_keyboard` - kitty keyboard protocol 支持开关（默认 false）
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
    /// let terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);
    /// ```
    pub fn new(
        cols: usize,
        rows: usize,
        scrollback: usize,
        kitty_keyboard: bool,
        event_proxy: GpuiEventProxy,
    ) -> Self {
        // Configure the history buffer before creating the terminal grids.
        let config = Config {
            scrolling_history: scrollback,
            // kitty keyboard protocol 支持开关（默认 false；运行期切换走
            // update_term_config → Term::set_options）
            kitty_keyboard,
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
            scrollback,
            osc_sink: OscSink::new(),
            osc_events: VecDeque::new(),
            cwd: None,
            tracker: Mutex::new(PromptTracker::new()),
            progress: None,
            notice_tx: None,
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
    /// # let mut terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);
    /// // Process some output from the PTY
    /// terminal.process_bytes(b"Hello, world!\r\n");
    /// ```
    pub fn process_bytes(&mut self, bytes: &[u8]) {
        let mut term = self.term.lock();
        // 滚动基准：喂入前后的 history（topmost_line = Line(-history)）与
        // total 差值。预填充期 total 随 history 同步增长（新行补位、无挤出，
        // 修正量为 0 = 逐行精确）；缓冲打满后 alacritty 环形复用行，挤出量
        // 对外不可观测（Term 无滚动计数外泄），修正退化为下界——最老标记
        // 可能滞留偏下，见 crate::osc 模块注记（v1 已知边界）。
        let history_before = -term.topmost_line().0 as i64;
        let total_before = term.total_lines() as i64;
        // The parser.advance method calls handler methods on the Term
        // The Term implements the Handler trait from the VTE crate
        self.parser.advance(&mut *term, bytes);
        // OSC 旁路：同一字节流喂独立 vte 状态机（只认 OSC，见 crate::osc 模块注记）
        self.osc_sink.feed(bytes);
        let evicted_delta = ((-term.topmost_line().0 as i64 - history_before)
            - (term.total_lines() as i64 - total_before))
            .max(0) as u64;
        // ALT_SCREEN 以喂入结束时的模式为准：期间 grid() 是备用屏
        //（alacritty swap_alt 交换主备网格），主屏标记锚定必错位 → A/C 冻结
        let alt_screen = term.mode().contains(TermMode::ALT_SCREEN);
        let events = self.osc_sink.take_events();
        self.data_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 无 OSC 事件且无滚动：不取 tracker 锁（字节热路径零额外锁开销）
        if evicted_delta == 0 && events.is_empty() {
            return;
        }
        // 锁序固定 term → tracker（见 tracker 字段注记）
        let mut tracker = self.tracker.lock();
        tracker.note_scroll(evicted_delta);
        for event in events {
            match event {
                OscEvent::PromptStart => {
                    // 受理行号 = total-1-(bottommost-cursor)（与 SearchMatch.line
                    // 同口径，0 = 最老历史行）。ALT_SCREEN 期间 A 不入库。
                    if !alt_screen {
                        let grid = term.grid();
                        let abs_line = grid.total_lines()
                            - 1
                            - (grid.bottommost_line().0 - grid.cursor.point.line.0) as usize;
                        tracker.push_prompt(abs_line, grid.total_lines());
                    }
                }
                // B 仅解析（v1 锚点取 A，多行提示符场景 A 行足够定位）
                OscEvent::CommandInputStart => {}
                OscEvent::CommandStart => tracker.note_command_start(alt_screen),
                // D 的失败判定不依赖行号：ALT_SCREEN 期间照常更新（冻结例外）
                OscEvent::CommandFinished { exit_code } => {
                    tracker.note_command_finished(exit_code);
                }
                // OSC 7 cwd：记录供「新标签/分屏继承焦点窗格目录」（file://
                // 校验与 percent 解码已在旁路解析收口）；事件照常入队
                OscEvent::Cwd { path } => {
                    self.cwd = Some(path.clone());
                    queue_osc_event(&mut self.osc_events, OscEvent::Cwd { path });
                }
                // ConEmu 进度：存最近值（Remove 清除），事件不入队——进度
                // 是状态而非通知，上层经 progress() getter 读，无桌面通知
                OscEvent::Progress(state) => {
                    self.progress = if state == crate::osc::ProgressState::Remove {
                        None
                    } else {
                        Some(state)
                    };
                }
                // 桌面通知：直发 TerminalEvent 通道（视图 process_events 消费），
                // 同时入 osc_events 队列供上层经 take_osc_events 兜底
                OscEvent::Notify { body } => {
                    self.send_desktop_notice(String::new(), &body);
                    queue_osc_event(&mut self.osc_events, OscEvent::Notify { body });
                }
                OscEvent::NotifyWithTitle { title, body } => {
                    self.send_desktop_notice(title.clone(), &body);
                    queue_osc_event(
                        &mut self.osc_events,
                        OscEvent::NotifyWithTitle { title, body },
                    );
                }
            }
        }
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
    /// # let mut terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);
    /// // Resize to 120x30
    /// terminal.resize(120, 30);
    /// ```
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let changed = self.cols != cols || self.rows != rows;
        self.cols = cols;
        self.rows = rows;

        let mut term = self.term.lock();

        // Create dimensions for the resize
        let dimensions = TermDimensions::new(cols, rows);

        // Resize the terminal
        term.resize(dimensions);
        if changed {
            // 列变化 reflow 重排历史行、行变化在历史与视口间搬移——绝对行号
            // 体系整体失效，重映射无稳定锚点（reflow 语义下不可恢复），整体
            // 作废。缩放后需重新等一条命令才恢复标记，属可接受行为。
            self.tracker.lock().invalidate();
        }
    }

    /// 运行期更新 Term 级配置（目前仅 kitty keyboard protocol 开关）：
    /// 走 `Term::set_options`，kitty 位变更时 alacritty 自动清空协议模式栈
    /// 并摘除 KITTY_KEYBOARD_PROTOCOL 位（term/mod.rs set_options）。滚动
    /// 上限回填 new() 时的值（set_options 整体重建 Config，漏填会重置回
    /// 默认 10_000）。
    pub fn update_term_config(&self, kitty_keyboard: bool) {
        let mut term = self.term.lock();
        let config = Config {
            scrolling_history: self.scrollback,
            kitty_keyboard,
            ..Default::default()
        };
        term.set_options(config);
    }

    /// Update the scrollback history capacity of the active grid.
    pub fn set_scrollback(&mut self, scrollback: usize) {
        // 同步回写本端缓存：update_term_config 用它重建 Config，漏写的话
        // 之后的 kitty 开关切换会让滚动上限静默回落旧值并截断历史
        self.scrollback = scrollback;
        let mut term = self.term.lock();
        let history_before = term.grid().history_size();
        term.grid_mut().update_history(scrollback);
        // 缩小滚动上限会把最老行挤出缓冲（shrink_lines），绝对行号整体前移
        // → 作废；放大只抬高未来上限、不动既有行，标记仍有效。
        if term.grid().history_size() < history_before {
            self.tracker.lock().invalidate();
        }
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
    /// # let terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);
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

    // ───────────── OSC 133 提示符标记 / 旁路 OSC 事件 ─────────────

    /// 提示符标记快照（升序）。已按受理后滚动淘汰量修正绝对行号；
    /// 出界标记在表内真删（见 [`PromptTracker::visible_marks`]）。
    /// ALT_SCREEN 期间返回空（查询冻结与跳转拒绝同一口径：备用屏激活时
    /// 主屏标记暂不可用，退出后原样恢复）。
    pub fn prompt_marks(&self) -> Vec<PromptMark> {
        let term = self.term.lock();
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return Vec::new();
        }
        let total = term.grid().total_lines();
        // 锁序 term → tracker
        self.tracker.lock().visible_marks(total)
    }

    /// 上一/下一提示符跳转：滚到目标标记（对齐视口上部 1/3，与搜索跳转
    /// 同式）。无可用标记或处于 ALT_SCREEN（备用屏无主屏标记）时返回
    /// `false` 且不动视口。
    pub fn jump_prompt(&self, direction: PromptDirection) -> bool {
        let mut term = self.term.lock();
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return false;
        }
        let (marks, history, screen, display_offset) = {
            let grid = term.grid();
            // 锁序 term → tracker
            let marks = self.tracker.lock().visible_marks(grid.total_lines());
            (
                marks,
                grid.history_size() as i64,
                grid.screen_lines() as i64,
                grid.display_offset() as i64,
            )
        };
        // 参考位 = 视口顶行的绝对行号：Previous 取其上方最近的标记，
        // Next 取视口底行（top + screen - 1）之下最近的标记——屏内标记不跳
        let top_abs = history - display_offset;
        let target = match direction {
            PromptDirection::Previous => marks
                .iter()
                .rev()
                .find(|mark| (mark.abs_line as i64) < top_abs)
                .map(|mark| mark.abs_line),
            PromptDirection::Next => marks
                .iter()
                .find(|mark| mark.abs_line as i64 > top_abs + screen - 1)
                .map(|mark| mark.abs_line),
        };
        let Some(target) = target else {
            return false;
        };
        // 目标行放视口上部 1/3（与 view.rs scroll_to_current_match 同式钳位）
        let desired = (history + screen / 3 - target as i64).clamp(0, history) as i32;
        let delta = desired - display_offset as i32;
        if delta != 0 {
            term.scroll_display(Scroll::Delta(delta));
        }
        true
    }

    /// 最近一条已结束命令是否失败（OSC 133;D exit≠0；D 不带退出码不改判定）。
    /// 失败红点（标签栏）与提示符跳转的上层数据源。
    pub fn last_command_failed(&self) -> bool {
        self.tracker.lock().last_command_failed()
    }

    /// 当前是否有命令运行中（OSC 133;C 与 D 之间的窗口态）。v1 只暴露数据，
    /// 不驱动任何 UI。
    pub fn command_running(&self) -> bool {
        self.tracker.lock().command_running()
    }

    /// 作废全部提示符标记并重置滚动计数基准。两处 resize 路径（本结构
    /// [`resize`](Self::resize) 与 view.rs canvas 直连 `Term::resize`）调用：
    /// reflow 重排历史行、绝对行号整体失效，重映射不可行故选作废。
    pub fn invalidate_prompt_marks(&self) {
        self.tracker.lock().invalidate();
    }

    /// 取走旁路解析出的非 133 事件（OSC 7 cwd / OSC 9·777 notify），
    /// 供后续子项消费；取走即清空。
    pub fn take_osc_events(&mut self) -> Vec<OscEvent> {
        self.osc_events.drain(..).collect()
    }

    /// 最近一次 OSC 7 上报的 cwd（percent 解码后的绝对路径）；从未上报为
    /// None（窗格刚建立、非集成 shell 或 shell_integration 关闭时即此态，
    /// 调用方回退为不继承）。
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// 最近一次 OSC 9;4 进度上报（ConEmu）。v1 仅暴露 getter，UI 不呈现
    ///（设计阶段随 osc133 角标统一决定）；Remove 上报清除为 None。
    pub fn progress(&self) -> Option<crate::osc::ProgressState> {
        self.progress
    }

    /// 注入桌面通知发送通道（TerminalView 构造后调用）。
    pub fn set_notice_tx(&mut self, tx: std::sync::mpsc::Sender<crate::event::TerminalEvent>) {
        self.notice_tx = Some(tx);
    }

    /// 向 TerminalEvent 通道发送桌面通知（OSC 9/777 → DesktopNotice）。
    /// 无通道（测试）时静默丢弃，事件队列仍保留。
    fn send_desktop_notice(&self, title: String, body: &str) {
        if let Some(tx) = &self.notice_tx {
            let _ = tx.send(crate::event::TerminalEvent::DesktopNotice {
                title,
                body: body.to_owned(),
            });
        }
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
    /// # let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);
    /// terminal.process_bytes(b"hello world\r\n");
    /// let matches = terminal.search("WORLD");
    /// assert_eq!(matches.len(), 1);
    /// assert_eq!(matches[0].column, 6);
    /// ```
    pub fn search(&self, query: &str) -> Vec<SearchMatch> {
        self.search_with(query, false)
    }

    /// 大小写敏感可切换的搜索（`case_sensitive=false` 与 [`Self::search`] 同口径）。
    /// 实现委托 [`Self::search_with_options`]（快照管道，见其注记）。
    pub fn search_with(&self, query: &str, case_sensitive: bool) -> Vec<SearchMatch> {
        self.search_with_options(
            query,
            SearchOptions {
                case_sensitive,
                ..Default::default()
            },
        )
        .unwrap_or_default()
    }

    /// 带选项的搜索：普通子串 / 全字 / 正则三模式（同步入口）。
    ///
    /// 实现已迁移到「快照 + 扫描」管道：先 [`Self::snapshot_for_search`]
    /// 单次持锁取快照，再 [`scan_snapshot`] 在调用线程扫描。视图层防抖/
    /// Enter 提交直接复用同一管道并把扫描移到后台线程（见 view.rs
    /// `run_pending_search`），本方法是等价的同步便捷入口。
    ///
    /// - 子串与全字路径大小写不敏感时按字符小写化匹配（与 [`Self::search`]
    ///   同口径，避免 UTF-8 字节索引在中文场景错位）；行尾空 cell 填充空格
    ///   不裁剪（全字模式下天然是词边界）。
    /// - 正则路径的锚点语义（`$` = 行尾最后一个非空白字符之后、`^` = 每个
    ///   网格行列 0、零宽命中过滤）见 [`scan_snapshot_regex`] 的契约注记；
    ///   大小写不敏感经 `(?i)` 前缀实现。
    /// - 非法正则返回 `Err(错误信息)`（供搜索栏内联红字展示），**绝不
    ///   panic**；调用方应保留上次有效结果。
    pub fn search_with_options(
        &self,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<SearchMatch>, String> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        // needle 备料与 scan_snapshot 的契约一致：正则传原样查询（扫描器
        // 内拼回编译），子串按大小写敏感性预 lowercase
        let needle: Vec<char> = if options.regex || options.case_sensitive {
            query.chars().collect()
        } else {
            query.to_lowercase().chars().collect()
        };
        let snapshot = self.snapshot_for_search();
        scan_snapshot(&snapshot, &needle, options)
    }

    /// 构建回滚搜索快照（主线程单次持锁；构建后匹配全部移后台线程）。
    ///
    /// 锁窗口从同步实现的「构建 + 逐行匹配」缩为「纯复制」：扫描期间终端
    /// 新输出不影响已建快照——命中反映「扫描完成时刻之前」的网格状态，
    /// 与同步实现的「执行时刻」语义一致（同步实现的 matches 同样只反映
    /// 其持锁扫描瞬间的网格，锁释放后 PTY 新输出立即使结果过时）。
    /// 紧凑编码与内存账见 [`SearchSnapshot`] 注记。
    pub fn snapshot_for_search(&self) -> SearchSnapshot {
        self.with_term(|term| {
            let grid = term.grid();
            let total_lines = grid.total_lines();
            let history = total_lines - grid.screen_lines();
            let columns = grid.columns();
            let mut text = String::with_capacity(total_lines * (columns + 1));
            let mut line_ranges = Vec::with_capacity(total_lines);
            let mut wide_maps = Vec::new();
            for absolute in 0..total_lines {
                let line = Line(absolute as i32 - history as i32);
                let start = text.len() as u32;
                // 行内「字符索引 → 起始列」映射：无条件构建（宽字符与
                // spacer 混排后字符索引不再等于列号）；非宽行整表丢弃，
                // 扫描侧走恒等映射（紧凑编码的关键省内存点）
                let mut char_columns: Vec<(u32, u32)> = Vec::new();
                let mut has_wide = false;
                for column in 0..columns {
                    let cell = &grid[AlacPoint::new(line, Column(column))];
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        has_wide = true;
                        continue;
                    }
                    has_wide |= cell.flags.contains(Flags::WIDE_CHAR);
                    char_columns.push((char_columns.len() as u32, column as u32));
                    // NUL 规范化：空 cell 填充 '\0' 按渲染口径记为 ' '
                    //（render.rs 单元绘制 '\0'→' '，搜索随渲染口径）
                    text.push(if cell.c == '\0' { ' ' } else { cell.c });
                }
                line_ranges.push((start, text.len() as u32));
                if has_wide {
                    wide_maps.push((absolute as u32, char_columns));
                }
            }
            SearchSnapshot {
                text,
                line_ranges,
                wide_maps,
                columns,
                screen_lines: grid.screen_lines(),
                total_lines,
            }
        })
    }

    /// 从网格中提取点击位置处的 URL。
    ///
    /// `row` 是视口行（0 = 视口顶行，回滚时随视口上移），`col` 是网格列。
    /// 行换算与渲染快照同口径：`Line(row - display_offset)`（负行进历史区）。
    /// 越界坐标（鼠标滑出网格边缘）返回 None 而非索引网格——pixel_to_cell
    /// 只钳下限，这里的防御是 URL 悬停/右键/Ctrl+点击三条路径的唯一护栏，
    /// 缺了会在 `grid[Line]` 的 debug_assert 上 panic（storage.rs compute_index）。
    /// 返回 `(url, 起始列, 结束列)`；无命中为 None。结束列为开区间。
    pub fn extract_url_span_at(&self, row: usize, col: usize) -> Option<(String, usize, usize)> {
        self.with_term(|term| {
            let grid = term.grid();
            let screen_lines = grid.screen_lines();
            let columns = grid.columns();
            if row >= screen_lines || col >= columns {
                return None;
            }
            let display_offset = grid.display_offset() as i32;
            let line = Line(row as i32 - display_offset);

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

    /// OSC 8 显式超链接：命中 `(row, col)` 所在超链接段并返回其 URI。
    ///
    /// 坐标换算与 [`Self::extract_url_span_at`] 同口径（`row` = 视口行，
    /// 越界一律 `None`）。命中后按 `Hyperlink` 的值相等（PartialEq 按
    /// id+uri，alacritty cell.rs）向两侧扩至段边界；段内宽字符的
    /// WIDE_CHAR_SPACER 续列并入段（与文本检测跳过 spacer 列同口径，
    /// 由首列宽字符代表）。注意：隐式换行把超链接折到多行时只走本行
    /// ——与既有文本检测同为单行口径，跨行段 v1 不做。
    pub fn hyperlink_span_at(&self, row: usize, col: usize) -> Option<String> {
        self.hyperlink_span_extent_at(row, col)
            .map(|(uri, _, _)| uri)
    }

    /// 同 [`Self::hyperlink_span_at`] 但附带段的列范围（悬停下划线覆盖层
    /// 用；`(起始列, 结束列)` 结束列开区间，与 extract_url_span_at 同口径）。
    pub fn hyperlink_span_extent_at(
        &self,
        row: usize,
        col: usize,
    ) -> Option<(String, usize, usize)> {
        self.with_term(|term| {
            let grid = term.grid();
            let screen_lines = grid.screen_lines();
            let columns = grid.columns();
            if row >= screen_lines || col >= columns {
                return None;
            }
            let display_offset = grid.display_offset() as i32;
            let line = Line(row as i32 - display_offset);

            // 命中判定：起点格带超链接；起点是段内宽字符的续列（spacer）
            // 时，以其左邻宽字符的超链接为准（spacer 本体是否携带随
            // alacritty 写入路径而定，两种情况都命中）
            let mut target = grid[AlacPoint::new(line, Column(col))].hyperlink();
            if target.is_none()
                && col > 0
                && grid[AlacPoint::new(line, Column(col))]
                    .flags
                    .contains(Flags::WIDE_CHAR_SPACER)
            {
                target = grid[AlacPoint::new(line, Column(col - 1))].hyperlink();
            }
            let target = target?;

            // 向左扩：段内格逐个值相等（Hyperlink PartialEq 按 id+uri）
            let mut start = col;
            while start > 0
                && grid[AlacPoint::new(line, Column(start - 1))]
                    .hyperlink()
                    .as_ref()
                    == Some(&target)
            {
                start -= 1;
            }
            // 向右扩：值相等直入；宽字符续列 spacer 随其左邻并入段
            let mut end = col + 1;
            while end < columns {
                let cell = &grid[AlacPoint::new(line, Column(end))];
                let in_segment = cell.hyperlink().as_ref() == Some(&target)
                    || (cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                        && grid[AlacPoint::new(line, Column(end - 1))]
                            .hyperlink()
                            .as_ref()
                            == Some(&target));
                if !in_segment {
                    break;
                }
                end += 1;
            }
            Some((target.uri().to_owned(), start, end))
        })
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
    /// # let terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);
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
    /// # let terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);
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

    /// 快照路径扫描助手：与视图层 run_pending_search 相同的备料与调用
    /// 形态（正则传原样、子串按大小写预 lowercase）。既有搜索断言经此
    /// 迁移到 snapshot→scan 管道，期望值保持迁移前同步实现的字面结果。
    fn scan_via_snapshot(
        terminal: &TerminalState,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<SearchMatch>, String> {
        let needle: Vec<char> = if options.regex || options.case_sensitive {
            query.chars().collect()
        } else {
            query.to_lowercase().chars().collect()
        };
        let snapshot = terminal.snapshot_for_search();
        scan_snapshot(&snapshot, &needle, options)
    }

    #[test]
    fn test_terminal_creation() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);

        assert_eq!(terminal.cols(), 80);
        assert_eq!(terminal.rows(), 24);
    }

    #[test]
    fn test_process_bytes() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);

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
        let mut terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);

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
        let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);

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
        let mut terminal = TerminalState::new(80, 30, 100, false, GpuiEventProxy::new(tx));
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
        let mut terminal = TerminalState::new(80, 24, 100, false, GpuiEventProxy::new(tx));
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
        let mut terminal = TerminalState::new(80, 24, 100, false, GpuiEventProxy::new(tx));
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
        let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);

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

    /// URL 坐标护栏：`pixel_to_cell` 只钳下限，鼠标滑出网格底/右缘或回滚
    /// 后悬停都会送进越界 row/col——必须返回 None 而非在 `grid[Line]` 的
    /// debug_assert 上 panic（storage.rs compute_index）。
    #[test]
    fn extract_url_span_at_rejects_out_of_bounds_coordinates() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 4, 100, false, GpuiEventProxy::new(tx));
        terminal.process_bytes(b"$ curl https://example.com\r\n");

        // 鼠标滑出网格底缘/右缘：越界一律 None。
        // 注意 screen_lines 须先取出再调用——with_term 持锁期间重入
        // extract_url_span_at（内部再 with_term）会自死锁。
        let screen_lines = terminal.with_term(|term| term.grid().screen_lines());
        assert_eq!(terminal.extract_url_span_at(screen_lines + 3, 10), None);
        assert_eq!(terminal.extract_url_span_at(0, 999), None);

        // 回滚后（display_offset > 0）视口行换算进历史区：合法行不 panic、越界仍 None
        for line in 0..20 {
            terminal.process_bytes(format!("history-{line}\r\n").as_bytes());
        }
        terminal.with_term_mut(|term| {
            term.scroll_display(alacritty_terminal::grid::Scroll::Delta(2));
        });
        assert!(terminal.with_term(|term| term.grid().display_offset()) > 0);
        assert_eq!(terminal.extract_url_span_at(screen_lines - 1, 0), None);
        assert_eq!(terminal.extract_url_span_at(screen_lines + 1, 0), None);
    }

    #[test]
    fn test_ime_anchor_follows_cursor_and_clamps_when_scrolled() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);
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
        let mut terminal = TerminalState::new(60, 4, 32, false, event_proxy);
        terminal.process_bytes(b"See https://example.com/page\r\n");

        // 先验证搜索能找到 URL（确保文本在网格里；快照管道口径）
        let matches = scan_via_snapshot(&terminal, "https", SearchOptions::default()).unwrap();
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
        let mut terminal = TerminalState::new(60, 4, 32, false, event_proxy);
        terminal.process_bytes(b"Link: https://example.com.,\r\n");
        let matches = scan_via_snapshot(&terminal, "https", SearchOptions::default()).unwrap();
        assert!(!matches.is_empty());
        let url = terminal.extract_url_at(0, matches[0].column + 2);
        assert_eq!(url.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn test_extract_url_www_prefix() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(60, 4, 32, false, event_proxy);
        terminal.process_bytes(b"Visit www.example.com today\r\n");
        let matches = scan_via_snapshot(&terminal, "www", SearchOptions::default()).unwrap();
        assert!(!matches.is_empty());
        let url = terminal.extract_url_at(0, matches[0].column + 2);
        assert_eq!(url.as_deref(), Some("https://www.example.com"));
    }

    #[test]
    fn test_extract_url_none_in_plain_text() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, false, event_proxy);
        terminal.process_bytes("中文内容没有链接\r\nplain text line\r\n".as_bytes());
        assert!(terminal.extract_url_at(0, 2).is_none());
        assert!(terminal.extract_url_at(1, 5).is_none());
    }

    #[test]
    fn test_search_visible_line_and_column() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);
        terminal.process_bytes(b"hello world\r\n");

        let matches = scan_via_snapshot(&terminal, "WORLD", SearchOptions::default()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].column, 6);
        assert_eq!(matches[0].len, 5);

        // 空查询无命中
        assert!(
            scan_via_snapshot(&terminal, "", SearchOptions::default())
                .unwrap()
                .is_empty()
        );
        assert!(
            scan_via_snapshot(&terminal, "zzz", SearchOptions::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_search_scrollback_ordering() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);
        for line in 0..40 {
            terminal.process_bytes(format!("line-{line}\r\n").as_bytes());
        }

        let matches = scan_via_snapshot(&terminal, "line-3", SearchOptions::default()).unwrap();
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
        let mut terminal = TerminalState::new(40, 4, 32, false, event_proxy);
        terminal.process_bytes("中文搜索测试\r\n第二行 中文\n".as_bytes());

        let matches = scan_via_snapshot(&terminal, "中文", SearchOptions::default()).unwrap();
        assert_eq!(matches.len(), 2);
        // 每个汉字占两列：第二行命中从列 4 开始（"第二行 " = 3 汉字 6 列 + 1 空格）
        assert_eq!(matches[1].column, 7);
        assert_eq!(matches[1].len, 4);
    }

    #[test]
    fn test_search_matches_on_same_line() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, false, event_proxy);
        terminal.process_bytes(b"ab ab ab\n");

        let matches = scan_via_snapshot(&terminal, "ab", SearchOptions::default()).unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].column, 0);
        assert_eq!(matches[1].column, 3);
        assert_eq!(matches[2].column, 6);
    }

    // ───────────── search_with_options：正则 / 全字 / 锚点 ─────────────

    /// 正则模式选项快捷构造（大小写不敏感默认）。
    fn regex_opts(case_sensitive: bool) -> SearchOptions {
        SearchOptions {
            case_sensitive,
            regex: true,
            whole_word: false,
        }
    }

    #[test]
    fn test_search_regex_basic() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, false, event_proxy);
        terminal.process_bytes(b"foo=1 bar=22\n");

        let matches = scan_via_snapshot(&terminal, r"bar=\d+", regex_opts(false)).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line, 0);
        // f(0)o(1)o(2)=(3)1(4)␣(5)b(6)a(7)r(8)=(9)2(10)2(11)
        assert_eq!(matches[0].column, 6);
        assert_eq!(matches[0].len, 6);
    }

    #[test]
    fn test_search_regex_cjk_column_width() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, false, event_proxy);
        terminal.process_bytes("第二行 中文\n".as_bytes());

        let matches = scan_via_snapshot(&terminal, "中文", regex_opts(false)).unwrap();
        assert_eq!(matches.len(), 1);
        // 列宽口径同 test_search_cjk_text：3 汉字 6 列 + 1 空格 → 列 7，
        // 两个宽字符共占 4 列
        assert_eq!(matches[0].column, 7);
        assert_eq!(matches[0].len, 4);
    }

    #[test]
    fn test_search_regex_case_sensitivity() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, false, event_proxy);
        terminal.process_bytes(b"abc ABC\n");

        // 大小写不敏感（默认）：经 (?i) 前缀，两组都命中
        assert_eq!(
            scan_via_snapshot(&terminal, "abc", regex_opts(false))
                .unwrap()
                .len(),
            2
        );
        // 大小写敏感：只命中各自字面
        let lower = scan_via_snapshot(&terminal, "abc", regex_opts(true)).unwrap();
        assert_eq!(lower.len(), 1);
        assert_eq!(lower[0].column, 0);
        let upper = scan_via_snapshot(&terminal, "ABC", regex_opts(true)).unwrap();
        assert_eq!(upper.len(), 1);
        assert_eq!(upper[0].column, 4);
    }

    #[test]
    fn test_search_regex_invalid_returns_err() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, false, event_proxy);
        terminal.process_bytes(b"hello world\n");

        // 非法正则返回 Err（供搜索栏内联红字展示），绝不 panic
        let error = scan_via_snapshot(&terminal, "(unclosed", regex_opts(false)).unwrap_err();
        assert!(!error.is_empty(), "错误信息供搜索栏红字展示");
        // 错误信息必须单行且短：regex 原生 Display 是多行 caret 报告，
        // 直塞单行搜索栏会撑爆布局（取尾行摘要 + 60 字符截断）
        assert!(!error.contains('\n'), "错误串必须单行: {error:?}");
        assert!(
            error.starts_with("error: "),
            "应取 regex 报告的尾行摘要: {error:?}"
        );
        assert!(error.chars().count() <= 61, "超长错误应截断: {error:?}");
        // 大小写不敏感路径同样单行（(?i) 前缀不外泄）
        let error_ci = scan_via_snapshot(&terminal, "(unclosed", regex_opts(true)).unwrap_err();
        assert!(!error_ci.contains('\n'), "大小写路径同样单行: {error_ci:?}");
        // 子串路径同查询不受影响（括号是字面字符，无命中）
        assert_eq!(
            scan_via_snapshot(&terminal, "(unclosed", SearchOptions::default()).unwrap(),
            Vec::<SearchMatch>::new()
        );
    }

    #[test]
    fn test_search_whole_word_boundaries() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, false, event_proxy);
        terminal.process_bytes(b"ab ab ab\nxab abc\n");
        let options = SearchOptions {
            whole_word: true,
            ..Default::default()
        };

        let matches = scan_via_snapshot(&terminal, "ab", options).unwrap();
        // 仅独立成词的三处（行首行尾与空格两侧是边界）；
        // xab 的 ab（前邻 x）与 abc 的 ab（后邻 c）不命中
        assert_eq!(matches.len(), 3);
        assert!(matches.iter().all(|m| m.line == 0));
        // 非全字默认路径不受影响：xab/abc 中的 ab 也命中
        assert_eq!(
            scan_via_snapshot(&terminal, "ab", SearchOptions::default())
                .unwrap()
                .len(),
            5
        );

        // 全字 CJK：汉字属词字符（与正则 \b 一致），「中文」邻接「测试」
        // 不命中，行尾填充空格天然是边界
        let (tx, _rx) = channel();
        let mut cjk_terminal = TerminalState::new(40, 4, 32, false, GpuiEventProxy::new(tx));
        cjk_terminal.process_bytes("中文测试\n中文\n".as_bytes());
        let cjk = scan_via_snapshot(&cjk_terminal, "中文", options).unwrap();
        assert_eq!(cjk.len(), 1);
        assert_eq!(cjk[0].line, 1);
    }

    #[test]
    fn test_search_regex_zero_width_matches_filtered() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, false, event_proxy);
        terminal.process_bytes(b"bab\n");

        // a* 会在位置 0/2/3 产生零宽空匹配，只保留非空命中「a」
        let matches = scan_via_snapshot(&terminal, "a*", regex_opts(false)).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].column, 1);
        assert_eq!(matches[0].len, 1);
    }

    #[test]
    fn test_search_regex_order_and_options_default_compat() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(40, 4, 32, false, event_proxy);
        terminal.process_bytes(b"alpha one\nbeta two\nalpha three\n");

        // 正则路径与子串路径同 push 顺序：按 absolute line 升序，结果一致
        let by_regex = scan_via_snapshot(&terminal, "alpha", regex_opts(false)).unwrap();
        assert_eq!(by_regex, terminal.search("alpha"));
        assert!(by_regex.windows(2).all(|w| w[0].line <= w[1].line));
        // 默认选项（关正则/关全字）与既有 search() 完全同口径
        assert_eq!(
            scan_via_snapshot(&terminal, "alpha", SearchOptions::default()).unwrap(),
            terminal.search("alpha")
        );
    }

    /// 锚点组：$ = 每行最后一个非空白字符之后（行尾空 cell 填充空格
    /// 先裁剪，见 search_grid_regex 契约注记），^ = 每个网格行列 0。
    #[test]
    fn test_search_regex_end_anchor_ignores_cell_padding() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);
        // 20 列网格：「中文abc」后有 13 个填充空格，不裁剪时 c$ 失配
        terminal.process_bytes("中文abc\r\n".as_bytes());

        let matches = scan_via_snapshot(&terminal, "c$", regex_opts(false)).unwrap();
        assert_eq!(matches.len(), 1);
        // 中(0-1) 文(2-3) a(4) b(5) c(6)：命中在列 6，单字符宽 1
        assert_eq!(matches[0].line, 0);
        assert_eq!(matches[0].column, 6);
        assert_eq!(matches[0].len, 1);
    }

    #[test]
    fn test_search_regex_digits_end_anchor() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);
        terminal.process_bytes(b"error=42\r\n");

        let matches = scan_via_snapshot(&terminal, r"\d+$", regex_opts(false)).unwrap();
        assert_eq!(matches.len(), 1);
        // e(0)r(1)r(2)o(3)r(4)=(5)4(6)2(7)：命中行尾数字「42」
        assert_eq!(matches[0].column, 6);
        assert_eq!(matches[0].len, 2);
    }

    #[test]
    fn test_search_regex_start_anchor_at_row_start() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);
        terminal.process_bytes("中文abc\r\n".as_bytes());

        let matches = scan_via_snapshot(&terminal, "^中文", regex_opts(false)).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line, 0);
        assert_eq!(matches[0].column, 0);
        assert_eq!(matches[0].len, 4); // 两个宽字符占 4 列
    }

    #[test]
    fn test_search_regex_start_anchor_hits_wrapped_continuation_row() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);
        // 22 个 a：软换行把后 2 个折到续行——续行是独立 grid row，
        // ^ 在续行列 0 亦命中（锚点语义契约）
        terminal.process_bytes(b"aaaaaaaaaaaaaaaaaaaaaa\r\n");

        let matches = scan_via_snapshot(&terminal, "^a+$", regex_opts(false)).unwrap();
        assert_eq!(matches.len(), 2, "首行与续行各命中一次 ^a+$");
        assert_eq!(matches[0].line, 0);
        assert_eq!(matches[0].column, 0);
        assert_eq!(matches[0].len, 20);
        assert_eq!(matches[1].line, 1);
        assert_eq!(matches[1].column, 0);
        assert_eq!(matches[1].len, 2);
    }

    #[test]
    fn test_search_regex_inline_flag_with_end_anchor() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(20, 4, 32, false, event_proxy);
        terminal.process_bytes("中文ABC\r\n".as_bytes());

        // (?i) 内联大小写开关 × $ 锚点组合：大小写敏感开启时内联标志仍生效
        let matches = scan_via_snapshot(&terminal, "(?i)c$", regex_opts(true)).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].column, 6);
        assert_eq!(matches[0].len, 1);
    }

    // ───────────── 快照管道等值对照（Grid → snapshot → scan vs 网格直扫 oracle） ─────────────

    /// 用例集上的等值对照：迁移前同步实现（#[cfg(test)] oracle，网格直扫）
    /// 与快照管道（Grid → snapshot_for_search → scan_snapshot）逐用例
    /// 产出完全一致——覆盖宽字符、全字、大小写、正则锚点、回滚排序。
    #[test]
    fn scan_snapshot_equivalent_to_grid_oracle() {
        let cases: Vec<(&str, &str, SearchOptions)> = vec![
            // 宽字符列宽（CJK 占两列，命中列经 wide_maps 换算）
            (
                "中文搜索测试\r\n第二行 中文\n",
                "中文",
                SearchOptions::default(),
            ),
            // 全字 + 宽字符（汉字属词字符）
            (
                "中文测试\n中文\n",
                "中文",
                SearchOptions {
                    whole_word: true,
                    ..Default::default()
                },
            ),
            // 大小写不敏感 / 敏感子串
            ("abc ABC\n", "abc", SearchOptions::default()),
            (
                "abc ABC\n",
                "abc",
                SearchOptions {
                    case_sensitive: true,
                    ..Default::default()
                },
            ),
            (
                "abc ABC\n",
                "ABC",
                SearchOptions {
                    case_sensitive: true,
                    ..Default::default()
                },
            ),
            // 全字边界（前后邻词字符排除）
            (
                "ab ab ab\nxab abc\n",
                "ab",
                SearchOptions {
                    whole_word: true,
                    ..Default::default()
                },
            ),
            // 混合行宽 + 多行排序
            (
                "alpha one\nbeta two\nalpha three\n",
                "alpha",
                SearchOptions::default(),
            ),
            // 正则：基础 / 锚点 / 宽字符列宽 / 零宽过滤
            ("foo=1 bar=22\n", r"bar=\d+", regex_opts(false)),
            ("error=42\r\n", r"\d+$", regex_opts(false)),
            ("中文abc\r\n", "^中文", regex_opts(false)),
            ("中文abc\r\n", "c$", regex_opts(false)),
            ("bab\n", "a*", regex_opts(false)),
            ("aaaaaaaaaaaaaaaaaaaaaa\r\n", "^a+$", regex_opts(false)),
            // 大小写切换的正则
            ("abc ABC\n", "abc", regex_opts(true)),
            // 大小写不敏感展开（İ 一对多）不 panic 且列映射对位
            ("İstanbul istanbul\n", "istanbul", SearchOptions::default()),
        ];
        for (content, query, options) in cases {
            let (tx, _rx) = channel();
            let mut terminal = TerminalState::new(40, 4, 128, false, GpuiEventProxy::new(tx));
            terminal.process_bytes(content.as_bytes());

            // oracle：迁移前的网格直扫（needle 备料与旧 search_with_options 一致）
            let oracle = if options.regex {
                let pattern = if options.case_sensitive {
                    Regex::new(query)
                } else {
                    Regex::new(&format!("(?i){query}"))
                }
                .expect("用例正则必须合法");
                terminal.with_term(|term| search_grid_regex(term.grid(), &pattern))
            } else {
                let needle: Vec<char> = if options.case_sensitive {
                    query.chars().collect()
                } else {
                    query.to_lowercase().chars().collect()
                };
                terminal.with_term(|term| {
                    search_grid_sensitivity(
                        term.grid(),
                        &needle,
                        options.case_sensitive,
                        options.whole_word,
                    )
                })
            };
            let via_snapshot = scan_via_snapshot(&terminal, query, options).unwrap();
            assert_eq!(
                via_snapshot, oracle,
                "快照管道与网格直扫不等值: query={query:?} options={options:?}"
            );
        }
    }

    /// 同步入口委托一致性：`search_with_options`（同步便捷入口）与
    /// 视图层使用的 scan_via_snapshot 管道同源——结果必须逐位一致。
    #[test]
    fn search_entry_delegates_to_snapshot_pipeline() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(40, 4, 32, false, GpuiEventProxy::new(tx));
        terminal.process_bytes("hello 世界\r\nhello again\n".as_bytes());
        for options in [
            SearchOptions::default(),
            SearchOptions {
                case_sensitive: true,
                ..Default::default()
            },
            SearchOptions {
                whole_word: true,
                ..Default::default()
            },
            regex_opts(false),
        ] {
            assert_eq!(
                terminal.search_with_options("hello", options),
                scan_via_snapshot(&terminal, "hello", options),
                "同步入口必须与快照管道同源: {options:?}"
            );
        }
    }

    /// NUL 规范化差异固化：快照把空 cell 填充 '\0' 记为 ' '（随渲染口径，
    /// render.rs 单元绘制 '\0'→' '）；旧网格直扫保留原值。含空格 needle
    /// 在 NUL 填充区的命中行为因此存在有意微差，本测试双侧断言固化。
    #[test]
    fn snapshot_normalizes_nul_padding_to_space() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(20, 4, 32, false, GpuiEventProxy::new(tx));
        terminal.process_bytes(b"ab");
        // VTE 常规路径不产生 NUL 格，直接改写 cell 构造目标场景
        terminal.with_term_mut(|term| {
            term.grid_mut()[AlacPoint::new(Line(0), Column(2))].c = '\0';
        });

        let options = SearchOptions::default();
        // 快照路径：NUL 已规范化 → 「b␣」命中 b + NUL 格
        let snapshot = terminal.snapshot_for_search();
        let needle: Vec<char> = "b ".chars().collect();
        let matches = scan_snapshot(&snapshot, &needle, options).unwrap();
        assert_eq!(matches.len(), 1, "规范化后含空格 needle 应命中 NUL 填充位");
        assert_eq!(matches[0].column, 1);
        // 旧口径（网格直扫 oracle）：'\0' ≠ ' ' → 不命中（差异固化）
        let oracle = terminal.with_term(|term| search_grid(term.grid(), &needle, false));
        assert!(oracle.is_empty(), "旧实现不把 NUL 当空格（有意差异）");
    }

    #[test]
    fn test_mode() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);

        let mode = terminal.mode();
        // Mode should be a valid TermMode value (just verify we can get it)
        let _bits = mode.bits();
    }

    #[test]
    fn test_with_term() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);

        let cols = terminal.with_term(|term| term.grid().columns());
        assert_eq!(cols, 80);
    }

    #[test]
    fn test_with_term_mut() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);

        terminal.with_term_mut(|term| {
            // Just verify we can get mutable access
            let _grid = term.grid_mut();
        });
    }

    #[test]
    fn test_term_arc() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, 10_000, false, event_proxy);

        let arc1 = terminal.term_arc();
        let arc2 = terminal.term_arc();

        // Both Arcs should point to the same terminal
        assert!(Arc::ptr_eq(&arc1, &arc2));
    }

    // ───────── OSC 133 提示符标记（旁路解析 → 锚定 → 跳转 → 生命周期） ─────────

    /// OSC 133;A 的完整字节（BEL 结尾）。
    fn prompt_a() -> &'static [u8] {
        b"\x1b]133;A\x07"
    }

    #[test]
    fn osc133_prompt_mark_line_anchoring_with_scrollback() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(20, 4, 32, false, GpuiEventProxy::new(tx));

        // 光标在初始顶行：第一个 A 锚在绝对行 0
        terminal.process_bytes(prompt_a());
        let marks = terminal.prompt_marks();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].abs_line, 0, "首个提示符应锚在绝对行 0");
        assert_eq!(marks[0].total_at_receipt, 4, "受理时 total = 初始 4 行");
        assert_eq!(marks[0].exit_code, None);

        // 继续输出 4 行 + 新提示符：预填充期 total 增长、旧行不动，
        // 旧标记绝对行号保持，新标记锚在受理时行（4 行内容后的新底行）
        terminal.process_bytes(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        terminal.process_bytes(prompt_a());
        let marks = terminal.prompt_marks();
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].abs_line, 0, "滚动未满上限：旧行号不漂移");
        assert_eq!(
            marks[1].abs_line, 4,
            "新提示符锚在受理时绝对行（4 行内容之后）"
        );
        assert_eq!(marks[1].total_at_receipt, 5);

        // 大量输出打满回滚（cap=32 → total 上限 36）再出新提示符：
        // 已知边界——打满后挤出量不可观测，标记保持受理值（不漂移也不误删）
        for line in 0..40 {
            terminal.process_bytes(format!("row-{line}\r\n").as_bytes());
        }
        terminal.process_bytes(prompt_a());
        let marks = terminal.prompt_marks();
        assert!(marks.len() >= 3);
        let total_now = terminal.with_term(|term| term.grid().total_lines());
        assert!(marks.iter().all(|mark| mark.abs_line < total_now));
    }

    #[test]
    fn osc133_marks_invalidated_by_resize_and_kept_on_noop_resize() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(20, 4, 32, false, GpuiEventProxy::new(tx));
        terminal.process_bytes(prompt_a());
        assert_eq!(terminal.prompt_marks().len(), 1);

        // 同尺寸 resize：grid 不 reflow，标记保留
        terminal.resize(20, 4);
        assert_eq!(terminal.prompt_marks().len(), 1, "同尺寸 resize 不作废");

        // 实际尺寸变化：绝对行号体系整体失效 → 全部作废 + 计数基准重置
        terminal.resize(20, 6);
        assert!(terminal.prompt_marks().is_empty(), "resize 后标记应作废");
        // 作废后重新受理，按新基准工作（6 行屏：x 在行 0、光标到行 1）
        terminal.process_bytes(b"x\r\n");
        terminal.process_bytes(prompt_a());
        let marks = terminal.prompt_marks();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].abs_line, 1, "新提示符锚在 resize 后的受理行");
    }

    #[test]
    fn osc133_alt_screen_freeze_with_d_exception() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(20, 4, 32, false, GpuiEventProxy::new(tx));
        terminal.process_bytes(prompt_a());
        assert_eq!(terminal.prompt_marks().len(), 1);

        // 进备用屏（vim/less 形态）：grid() 换成备用屏，A/C 冻结（查询同口径返回空）
        terminal.process_bytes(b"\x1b[?1049h");
        terminal.process_bytes(prompt_a());
        terminal.process_bytes(b"\x1b]133;C\x07");
        assert!(
            terminal.prompt_marks().is_empty(),
            "ALT_SCREEN 期间查询冻结返回空"
        );
        assert!(!terminal.command_running(), "ALT_SCREEN 期间 C 冻结");

        // D 例外：失败判定不依赖行号，照常更新
        terminal.process_bytes(b"\x1b]133;D;1\x07");
        assert!(terminal.last_command_failed(), "D 在备用屏也更新失败标记");
        assert!(!terminal.command_running(), "D 收束运行窗不受冻结影响");

        // 退备用屏：原标记原样恢复；随后再受理一条 A 只多一条
        //（若备用屏内的 A 曾入库，这里会是 3 —— 以此钉死 A 冻结）
        terminal.process_bytes(b"\x1b[?1049l");
        assert_eq!(terminal.prompt_marks().len(), 1, "退出备用屏后原标记恢复");
        terminal.process_bytes(b"next\r\n");
        terminal.process_bytes(prompt_a());
        assert_eq!(terminal.prompt_marks().len(), 2, "备用屏内的 A 未入库");
        // 且退出码回填到备用屏前那条标记（最近未关闭）
        let marks = terminal.prompt_marks();
        assert_eq!(marks[0].exit_code, Some(1));
    }

    #[test]
    fn osc133_jump_prompt_scrolls_and_refuses_in_alt_screen() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(20, 4, 32, false, GpuiEventProxy::new(tx));
        terminal.process_bytes(prompt_a());
        for line in 0..20 {
            terminal.process_bytes(format!("row-{line}\r\n").as_bytes());
        }
        terminal.process_bytes(prompt_a());
        assert!(terminal.with_term(|term| term.grid().display_offset() == 0));

        // 上一条提示符：滚回旧标记，display_offset 离开底部
        assert!(terminal.jump_prompt(PromptDirection::Previous));
        let offset = terminal.with_term(|term| term.grid().display_offset());
        assert!(offset > 0, "跳转应滚动视口，实际 offset={offset}");

        // 视口已停在旧提示符上方：再往上一条无目标 → false 且视口不动
        let offset = terminal.with_term(|term| term.grid().display_offset());
        assert!(!terminal.jump_prompt(PromptDirection::Previous));
        assert_eq!(
            terminal.with_term(|term| term.grid().display_offset()),
            offset
        );

        // 下一条：滚回新提示符
        assert!(terminal.jump_prompt(PromptDirection::Next));
        let next_offset = terminal.with_term(|term| term.grid().display_offset());
        assert!(next_offset < offset, "Next 应向底部回滚");

        // ALT_SCREEN：跳转直接 false（备用屏没有主屏标记）
        terminal.process_bytes(b"\x1b[?1049h");
        assert!(!terminal.jump_prompt(PromptDirection::Previous));
        terminal.process_bytes(b"\x1b[?1049l");
        assert!(terminal.jump_prompt(PromptDirection::Previous));
    }

    #[test]
    fn osc133_exit_code_backfill_and_running_window() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(20, 4, 32, false, GpuiEventProxy::new(tx));

        // 完整生命周期：A → C（运行窗开）→ D;127（失败）
        terminal.process_bytes(prompt_a());
        terminal.process_bytes(b"$ ");
        terminal.process_bytes(b"\x1b]133;C\x07");
        assert!(terminal.command_running(), "C 与 D 之间应为运行窗");
        terminal.process_bytes(b"exit 127\r\n");
        terminal.process_bytes(b"\x1b]133;D;127\x07");
        assert!(!terminal.command_running());
        assert!(terminal.last_command_failed());
        let marks = terminal.prompt_marks();
        assert_eq!(marks[0].exit_code, Some(127), "退出码回填到所属提示符");

        // 下一条成功：失败态清零、新标记退出码 0、旧标记保持
        terminal.process_bytes(b"\x1b]133;D;0\x07"); // 重复 D 只收束、置成功
        terminal.process_bytes(prompt_a());
        terminal.process_bytes(b"\x1b]133;C\x07");
        terminal.process_bytes(b"\x1b]133;D;0\x07");
        assert!(!terminal.last_command_failed());
        let marks = terminal.prompt_marks();
        assert_eq!(marks[0].exit_code, Some(127));
        assert_eq!(marks[1].exit_code, Some(0));
    }

    #[test]
    fn osc_bypass_events_queued_and_taken() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(20, 4, 32, false, GpuiEventProxy::new(tx));
        // OSC 7（cwd）与 133 混在同一字节流：133 进标记、7 进事件队列 + cwd 记录
        assert!(
            terminal.cwd().is_none(),
            "未上报时 cwd 为 None（回退不继承）"
        );
        terminal.process_bytes(b"\x1b]7;file:///home/u\x07");
        terminal.process_bytes(prompt_a());
        terminal.process_bytes("\x1b]9;完成\x07".as_bytes());
        assert_eq!(terminal.prompt_marks().len(), 1, "133 不进事件队列");
        assert_eq!(terminal.cwd(), Some("/home/u"), "OSC 7 记录解码后的路径");
        assert_eq!(
            terminal.take_osc_events(),
            vec![
                crate::osc::OscEvent::Cwd {
                    path: "/home/u".into()
                },
                crate::osc::OscEvent::Notify {
                    body: "完成".into()
                },
            ]
        );
        // take 走即清空；cwd 记录不受 take 影响（记录与队列分离）
        assert!(terminal.take_osc_events().is_empty());
        assert_eq!(terminal.cwd(), Some("/home/u"));
        // 非法上报（非 file://）不产事件也不动既有 cwd
        terminal.process_bytes(b"\x1b]7;kitty://x\x07");
        assert!(terminal.take_osc_events().is_empty());
        assert_eq!(terminal.cwd(), Some("/home/u"), "非法上报不覆盖既有值");
        // 合法新上报覆盖旧值（跟手：cd 到哪记哪）
        terminal.process_bytes(b"\x1b]7;file:///opt\x07");
        assert_eq!(terminal.cwd(), Some("/opt"));
    }

    #[test]
    fn osc_progress_state_stored_and_invalid_retained() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(20, 4, 32, false, GpuiEventProxy::new(tx));
        assert!(terminal.progress().is_none(), "未上报时无进度");

        terminal.process_bytes(b"\x1b]9;4;1;42\x07");
        assert_eq!(
            terminal.progress(),
            Some(crate::osc::ProgressState::Set { percent: 42 })
        );
        // 非法序列回退：状态保持上一次值，不被垃圾序列清掉
        terminal.process_bytes(b"\x1b]9;4;9;1\x07");
        terminal.process_bytes(b"\x1b]9;4;1;250\x07");
        assert_eq!(
            terminal.progress(),
            Some(crate::osc::ProgressState::Set { percent: 42 }),
            "非法 state/progress 应回退保留旧值"
        );
        // 出错态 → Remove 清除
        terminal.process_bytes(b"\x1b]9;4;2\x07");
        assert_eq!(terminal.progress(), Some(crate::osc::ProgressState::Error));
        terminal.process_bytes(b"\x1b]9;4;0;0\x07");
        assert!(terminal.progress().is_none(), "Remove 清除进度");
    }

    #[test]
    fn osc_notice_forwarded_to_desktop_channel() {
        let (proxy_tx, _proxy_rx) = channel();
        let (notice_tx, notice_rx) = channel();
        let mut terminal = TerminalState::new(20, 4, 32, false, GpuiEventProxy::new(proxy_tx));
        terminal.set_notice_tx(notice_tx);

        terminal.process_bytes("\x1b]777;notify;构建;完成\x07".as_bytes());
        terminal.process_bytes(b"\x1b]9;\xe6\x9e\x84\xe5\xbb\xba\xe5\xae\x8c\xe6\x88\x90\x07");
        // 直发 DesktopNotice（OSC 9 无标题 → 空串，展示层填默认）
        match notice_rx.recv() {
            Ok(crate::event::TerminalEvent::DesktopNotice { title, body }) => {
                assert_eq!(title, "构建");
                assert_eq!(body, "完成");
            }
            other => panic!("期望 DesktopNotice，实际 {other:?}"),
        }
        match notice_rx.recv() {
            Ok(crate::event::TerminalEvent::DesktopNotice { title, body }) => {
                assert_eq!(title, "");
                assert_eq!(body, "构建完成");
            }
            other => panic!("期望 DesktopNotice，实际 {other:?}"),
        }
        // 事件队列仍保留兜底副本（take 语义不变）
        assert_eq!(terminal.take_osc_events().len(), 2);
    }

    // ───────── OSC 8 显式超链接（Cell 级 hyperlink 段行走） ─────────

    #[test]
    fn osc8_hyperlink_span_walks_and_respects_boundaries() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(40, 4, 32, false, GpuiEventProxy::new(tx));
        // OSC 8 开 → 「link」 → 关；随后普通文本（不属于段）
        terminal.process_bytes(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ tail");
        // 段内任意列命中并返回 URI
        assert_eq!(
            terminal.hyperlink_span_at(0, 0).as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            terminal.hyperlink_span_at(0, 3).as_deref(),
            Some("https://example.com")
        );
        // 段边界：关闭序列之后的文本不再命中
        assert_eq!(terminal.hyperlink_span_at(0, 4), None, "段外不应命中");
        assert_eq!(terminal.hyperlink_span_at(0, 8), None);
        // 越界防御与 extract_url_span_at 同口径
        assert_eq!(terminal.hyperlink_span_at(4, 0), None);
        assert_eq!(terminal.hyperlink_span_at(0, 40), None);
    }

    #[test]
    fn osc8_adjacent_segments_do_not_merge() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(40, 4, 32, false, GpuiEventProxy::new(tx));
        // 相邻两个不同 URI：AA（列 0-1）与 BB（列 2-3），段按值相等不串
        terminal.process_bytes(b"\x1b]8;;https://a\x1b\\AA\x1b]8;;https://b\x1b\\BB\x1b]8;;\x1b\\");
        assert_eq!(
            terminal.hyperlink_span_at(0, 0).as_deref(),
            Some("https://a")
        );
        assert_eq!(
            terminal.hyperlink_span_at(0, 1).as_deref(),
            Some("https://a")
        );
        assert_eq!(
            terminal.hyperlink_span_at(0, 2).as_deref(),
            Some("https://b")
        );
        assert_eq!(
            terminal.hyperlink_span_at(0, 3).as_deref(),
            Some("https://b")
        );
        assert_eq!(terminal.hyperlink_span_at(0, 4), None);
    }

    #[test]
    fn osc8_span_extent_covers_segment_columns() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(40, 4, 32, false, GpuiEventProxy::new(tx));
        terminal.process_bytes(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");
        // 段列范围（结束列开区间）：段内任意列点击都拿到同一段
        assert_eq!(
            terminal.hyperlink_span_extent_at(0, 2),
            Some(("https://example.com".to_owned(), 0, 4))
        );
        assert_eq!(
            terminal.hyperlink_span_extent_at(0, 0),
            Some(("https://example.com".to_owned(), 0, 4))
        );
    }

    #[test]
    fn osc8_wide_char_spacer_columns_hit_and_stay_in_segment() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(40, 4, 32, false, GpuiEventProxy::new(tx));
        // 段内是两个宽字符「中文」（各占 2 列）：首列 + 续列都应命中，
        // 且段范围覆盖全部 4 列（spacer 同口径并入段）
        terminal.process_bytes("\x1b]8;;https://例.example\x1b\\中文\x1b]8;;\x1b\\".as_bytes());
        for col in 0..4 {
            assert_eq!(
                terminal.hyperlink_span_at(0, col).as_deref(),
                Some("https://例.example"),
                "宽字符段内列 {col} 应命中（含 spacer 续列）"
            );
        }
        assert_eq!(terminal.hyperlink_span_at(0, 4), None, "段外不应命中");
    }

    #[test]
    fn osc8_text_detection_stays_independent_for_priority_layering() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(40, 4, 32, false, GpuiEventProxy::new(tx));
        // 并存用例：URL 样文本被 OSC 8 包住但指向不同 URI——hyperlink
        // 返回声明 URI；文本自动检测仍返回字面文本（两者分层，view 侧
        // resolve_url_at 以 hyperlink 优先取用）
        terminal
            .process_bytes(b"\x1b]8;;https://real.example\x1b\\https://fake.example\x1b]8;;\x1b\\");
        assert_eq!(
            terminal.hyperlink_span_at(0, 0).as_deref(),
            Some("https://real.example"),
            "OSC 8 段内应返回声明 URI"
        );
        assert_eq!(
            terminal.extract_url_at(0, 0).as_deref(),
            Some("https://fake.example"),
            "文本自动检测保持独立口径（view 侧 hyperlink 优先）"
        );
        // 无 OSC 8 的纯文本 URL：hyperlink 不命中，文本检测照常（回落路径）
        terminal.process_bytes(b"\r\nplain https://text.example tail");
        assert_eq!(terminal.hyperlink_span_at(1, 6), None);
        assert_eq!(
            terminal.extract_url_at(1, 6).as_deref(),
            Some("https://text.example")
        );
    }
}
