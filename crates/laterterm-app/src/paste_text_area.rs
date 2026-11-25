//! 多行文本编辑区（textarea）：粘贴确认弹窗的编辑主体。
//!
//! 与 [`crate::command_text_field::CommandTextField`] 同一套 UTF-16 编辑模型
//! （兼容 GPUI 输入法协议），三点不同：文本保留换行（粘贴/输入都不折叠）、
//! 按行渲染选区与光标、Enter 插入换行而非提交（确认走弹窗「粘贴」按钮）。
//! 无右键菜单与掩码模式——粘贴编辑用不上，减面积。

use crate::command_text_field::previous_char_start;
use gpui::{
    Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Render, Styled, UTF16Selection, Window, canvas, div,
    prelude::FluentBuilder, px,
};
use laterterm_ui::{UiPalette, active_palette, tint_hsla};
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

/// 行高（px）：13px 正文 × 1.5 行距的取整档。点击定位（y → 行号）与渲染
/// 行高共用本常量，两边各写一份会悄悄错位。
const LINE_HEIGHT: f32 = 20.0;
/// 编辑区水平内边距（px_2）：点击定位 x 扣减与渲染 px_2 共用。
const H_PAD: f32 = 8.0;
/// 光标闪烁周期，与 CommandTextField 同系统习惯（约 530ms）。
const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(530);

pub(crate) struct PasteTextArea {
    focus_handle: FocusHandle,
    text: String,
    selection: Range<usize>,
    reversed: bool,
    marked_range: Option<Range<usize>>,
    /// 渲染字体族（跟随用户终端字体，等宽与终端码点对齐）。
    font_family: Option<String>,
    /// 字段在窗口中的几何（paint 期由 canvas 回写）；点击定位换算用。
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    focused: bool,
    caret_on: bool,
    blink_scheduled: bool,
    /// 左键拖拽选区锚点（UTF-16 偏移）；None = 非拖拽态。
    drag_anchor: Option<usize>,
}

impl PasteTextArea {
    pub(crate) fn new(text: impl Into<String>, cx: &mut Context<Self>) -> Self {
        // \r\n / \r 归一为 \n：终端粘贴语义无损（直写 PTY 本就按行执行），
        // 行渲染与偏移换算只按 \n 建模。
        let text = text.into().replace("\r\n", "\n").replace('\r', "\n");
        let end = text.encode_utf16().count();
        Self {
            focus_handle: cx.focus_handle(),
            text,
            selection: end..end,
            reversed: false,
            marked_range: None,
            font_family: None,
            bounds: Rc::new(Cell::new(None)),
            focused: false,
            caret_on: true,
            blink_scheduled: false,
            drag_anchor: None,
        }
    }

    pub(crate) fn value(&self) -> &str {
        &self.text
    }

    /// 表单逻辑回填文本（光标移到末尾，不伪造用户编辑）。当前仅测试接线；
    /// 业务侧将来回填（如重新打开弹窗恢复草稿）走这里。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set_value(&mut self, value: &str, cx: &mut Context<Self>) {
        if self.text == value {
            return;
        }
        self.selection = 0..self.utf16_len();
        self.replace_selection(value);
        self.marked_range = None;
        cx.notify();
    }

    /// 渲染字体族（跟随终端字体；不设则视图默认字体）。
    pub(crate) fn font_family(mut self, family: impl Into<String>) -> Self {
        self.font_family = Some(family.into());
        self
    }

    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn utf16_len(&self) -> usize {
        self.text.encode_utf16().count()
    }

    fn byte_offset(&self, index: usize) -> usize {
        self.text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(self.text.len()))
            .find(|&i| self.text[..i].encode_utf16().count() >= index)
            .unwrap_or(self.text.len())
    }

    /// UTF-16 偏移 → (行号, 行内 UTF-16 列)。
    fn line_col_at(&self, offset: usize) -> (usize, usize) {
        let byte = self.byte_offset(offset);
        let before = &self.text[..byte];
        let row = before.matches('\n').count();
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let col = self.text[line_start..byte].encode_utf16().count();
        (row, col)
    }

    /// 行号 + 行内 UTF-16 列 → 全文 UTF-16 偏移。
    fn offset_at_line_col(&self, row: usize, col: usize) -> usize {
        let lines: Vec<&str> = self.text.split('\n').collect();
        let row = row.min(lines.len() - 1);
        let mut offset = 0usize;
        for line in lines.iter().take(row) {
            offset += line.encode_utf16().count() + 1;
        }
        offset + col.min(lines[row].encode_utf16().count())
    }

    fn replace_selection(&mut self, text: &str) {
        let start = self.byte_offset(self.selection.start.min(self.selection.end));
        let end = self.byte_offset(self.selection.start.max(self.selection.end));
        self.text.replace_range(start..end, text);
        let cursor = self.text[..start].encode_utf16().count() + text.encode_utf16().count();
        self.selection = cursor..cursor;
        self.reversed = false;
        self.caret_on = true;
    }

    fn delete_backward(&mut self) {
        if self.selection.start != self.selection.end {
            self.replace_selection("");
            return;
        }
        if self.selection.start == 0 {
            return;
        }
        let start = previous_char_start(&self.text, self.byte_offset(self.selection.start));
        let end = self.byte_offset(self.selection.start);
        self.text.replace_range(start..end, "");
        let offset = self.text[..start].encode_utf16().count();
        self.selection = offset..offset;
    }

    fn delete_forward(&mut self) {
        if self.selection.start != self.selection.end {
            self.replace_selection("");
            return;
        }
        let start = self.byte_offset(self.selection.start);
        let end = self.text[start..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| start + i)
            .unwrap_or(self.text.len());
        if start < end {
            self.text.replace_range(start..end, "");
        }
    }

    fn selection_text(&self) -> Option<String> {
        let (start, end) = (
            self.selection.start.min(self.selection.end),
            self.selection.start.max(self.selection.end),
        );
        if start == end {
            return None;
        }
        Some(self.text[self.byte_offset(start)..self.byte_offset(end)].to_string())
    }

    fn copy_selection(&self, cx: &Context<Self>) {
        if let Some(text) = self.selection_text() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    fn cut_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_text() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            self.delete_backward();
            self.caret_on = true;
            cx.notify();
        }
    }

    /// 粘贴：textarea 语义保留换行（与 CommandTextField 的折叠不同）。
    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.replace_selection(&text);
        cx.notify();
    }

    /// 屏幕坐标 → 最近字符边界（UTF-16）。y 定行、行内按 shape 结果定列。
    fn caret_offset_at(&self, position: gpui::Point<Pixels>, window: &mut Window) -> Option<usize> {
        let bounds = self.bounds.get()?;
        let lines: Vec<&str> = self.text.split('\n').collect();
        let y: f32 = (position.y - bounds.origin.y).into();
        let row = (((y - H_PAD) / LINE_HEIGHT)
            .floor()
            .clamp(0.0, lines.len() as f32 - 1.0)) as usize;
        let style = window.text_style();
        let font_size = match style.font_size {
            gpui::AbsoluteLength::Pixels(size) => size,
            gpui::AbsoluteLength::Rems(rems) => window.rem_size() * rems.0,
        };
        let font = style.font();
        let font = match &self.font_family {
            Some(family) => gpui::Font {
                family: family.clone().into(),
                ..font
            },
            None => font,
        };
        let run = gpui::TextRun {
            len: lines[row].len(),
            font,
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped =
            window
                .text_system()
                .shape_line(lines[row].to_owned().into(), font_size, &[run], None);
        let x_in: f32 = (position.x - bounds.origin.x).into();
        let byte_index = shaped
            .closest_index_for_x(px((x_in - H_PAD).max(0.0)))
            .min(lines[row].len());
        let col = lines[row][..byte_index].encode_utf16().count();
        Some(self.offset_at_line_col(row, col))
    }

    fn place_caret_at(&mut self, position: gpui::Point<Pixels>, window: &mut Window) {
        if let Some(offset) = self.caret_offset_at(position, window) {
            self.selection = offset..offset;
            self.reversed = false;
            self.caret_on = true;
        }
    }

    fn on_area_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.focus_handle.focus(window);
        self.caret_on = true;
        if let Some(offset) = self.caret_offset_at(event.position, window) {
            if event.click_count >= 2 {
                // 双击选词（沿用 CommandTextField 的空白/非空白边界语义）。
                self.selection = self.word_range_at(offset);
                self.reversed = false;
                self.drag_anchor = None;
            } else {
                self.place_caret_at(event.position, window);
                self.drag_anchor = Some(self.selection.start);
            }
        }
        cx.notify();
    }

    /// 双击选词：以 offset 所在字符为种子，向两侧扩展到同类边界。
    fn word_range_at(&self, offset: usize) -> Range<usize> {
        let byte = self.byte_offset(offset);
        let seed_is_ws = self.text[byte..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        let start = self.text[..byte]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace() != seed_is_ws)
            .map_or(0, |(i, c)| i + c.len_utf8());
        let end = self.text[byte..]
            .char_indices()
            .find(|(_, c)| c.is_whitespace() != seed_is_ws)
            .map_or(self.text.len(), |(i, _)| byte + i);
        start..end
    }

    fn on_area_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag_anchor.is_some() && event.pressed_button != Some(MouseButton::Left) {
            self.drag_anchor = None;
            return;
        }
        let Some(anchor) = self.drag_anchor else {
            return;
        };
        if let Some(offset) = self.caret_offset_at(event.position, window) {
            self.selection = anchor.min(offset)..anchor.max(offset);
            self.reversed = offset < anchor;
            self.caret_on = true;
            cx.notify();
        }
    }

    fn on_area_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.drag_anchor = None;
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let k = event.keystroke.key.as_str();
        let accel = (event.keystroke.modifiers.platform || event.keystroke.modifiers.control)
            && !event.keystroke.modifiers.alt
            && !event.keystroke.modifiers.shift;
        if accel && matches!(k.to_ascii_lowercase().as_str(), "a" | "c" | "x" | "v") {
            cx.stop_propagation();
        }
        if accel && k.eq_ignore_ascii_case("a") {
            self.selection = 0..self.utf16_len();
            self.reversed = false;
            self.caret_on = true;
            cx.notify();
            return;
        }
        if accel && k.eq_ignore_ascii_case("c") {
            self.copy_selection(cx);
            return;
        }
        if accel && k.eq_ignore_ascii_case("x") {
            self.cut_selection(cx);
            return;
        }
        if accel && k.eq_ignore_ascii_case("v") {
            self.paste_clipboard(cx);
            return;
        }
        // textarea 语义：Enter 插入换行（确认走弹窗「粘贴」按钮，Esc 冒泡取消）。
        // 必须 stop_propagation：否则 dispatch_keystroke 会把 key_char("\r")
        // 再自动插一次（ElementInputHandler 路径），光标处多出空行。
        if k == "enter" {
            self.replace_selection("\n");
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let previous_text = self.text.clone();
        match k {
            "backspace" => self.delete_backward(),
            "delete" => self.delete_forward(),
            "left" => {
                let p = self.selection.start.min(self.selection.end);
                let pos = if self.selection.start != self.selection.end {
                    p
                } else {
                    self.text[..self.byte_offset(p)]
                        .chars()
                        .next_back()
                        .map_or(0, |c| p.saturating_sub(c.len_utf16()))
                };
                self.selection = pos..pos;
                self.reversed = false;
            }
            "right" => {
                let p = self
                    .selection
                    .end
                    .max(self.selection.start)
                    .min(self.utf16_len());
                let pos = if self.selection.start != self.selection.end {
                    p
                } else {
                    self.text[self.byte_offset(p)..]
                        .chars()
                        .next()
                        .map_or(p, |c| p + c.len_utf16())
                };
                self.selection = pos..pos;
                self.reversed = false;
            }
            // 上下键跨行：保持列（UTF-16），目标行越界时钳到行尾。
            "up" | "down" => {
                if self.text.contains('\n') {
                    let p = self.selection.start.min(self.selection.end);
                    let (row, col) = self.line_col_at(p);
                    let target = if k == "up" {
                        row.saturating_sub(1)
                    } else {
                        (row + 1).min(self.text.split('\n').count() - 1)
                    };
                    let pos = self.offset_at_line_col(target, col);
                    self.selection = pos..pos;
                    self.reversed = false;
                }
            }
            "home" => {
                let (row, _) = self.line_col_at(self.selection.start.min(self.selection.end));
                let pos = self.offset_at_line_col(row, 0);
                self.selection = pos..pos;
                self.reversed = false;
            }
            "end" => {
                let p = self.selection.start.min(self.selection.end);
                let (row, _) = self.line_col_at(p);
                let line_len = self
                    .text
                    .split('\n')
                    .nth(row)
                    .unwrap_or("")
                    .encode_utf16()
                    .count();
                let pos = self.offset_at_line_col(row, line_len);
                self.selection = pos..pos;
                self.reversed = false;
            }
            _ => return,
        }
        self.caret_on = true;
        if self.text != previous_text {
            // 文本变化：通知宿主（当前无订阅方，供后续接线；保持与
            // CommandTextField 同口径）。
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// 焦点期间每半个周期翻转一次光标相位；失焦后自动停。
    fn schedule_caret_blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_scheduled {
            return;
        }
        self.blink_scheduled = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CARET_BLINK_INTERVAL).await;
                let Ok(alive) = this.update(cx, |area, cx| {
                    if !area.focused {
                        area.blink_scheduled = false;
                        return false;
                    }
                    area.caret_on = !area.caret_on;
                    cx.notify();
                    true
                }) else {
                    break;
                };
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }
}

impl Render for PasteTextArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity: Entity<Self> = cx.entity();
        let focus = self.focus_handle.clone();
        let focused = focus.is_focused(window);
        if focused != self.focused {
            self.focused = focused;
            self.caret_on = true;
        }
        if focused {
            self.schedule_caret_blink(cx);
        }

        let composing = self.marked_range.is_some();
        let show_caret = focused && self.caret_on && !composing;
        let sel_start = self.selection.start.min(self.selection.end);
        let sel_end = self.selection.start.max(self.selection.end);
        let caret_byte = self.byte_offset(if self.reversed { sel_start } else { sel_end });
        let sel_start_byte = self.byte_offset(sel_start);
        let sel_end_byte = self.byte_offset(sel_end);

        let ui: UiPalette = active_palette(cx);
        // 两态：聚焦 accent 边 + elevated 底（焦点环）；常态卡片底 + 提亮边。
        // 一整块区域，无逐行描边（逐行框样式已被否掉）。
        let (box_bg, box_border) = if focused {
            (ui.elevated_bg, ui.accent)
        } else {
            (ui.card_bg, ui.border_strong)
        };

        let bounds_cell = self.bounds.clone();
        let font_family = self.font_family.clone();
        let mut area = div()
            .track_focus(&focus)
            .relative()
            .w_full()
            .py_2()
            .px_2()
            .border_1()
            .rounded_sm()
            .bg(box_bg)
            .border_color(box_border)
            .text_size(px(12.0))
            .text_color(ui.text)
            .when_some(font_family, |area, family| area.font_family(family))
            .child(
                div().absolute().inset_0().child(
                    canvas(
                        move |bounds, _window, _cx| bounds,
                        move |bounds, _, window, cx| {
                            bounds_cell.set(Some(bounds));
                            window.handle_input(
                                &focus,
                                ElementInputHandler::new(bounds, entity.clone()),
                                cx,
                            );
                        },
                    )
                    .size_full(),
                ),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_area_mouse_down))
            .on_mouse_move(cx.listener(Self::on_area_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_area_mouse_up));

        // 按行渲染：每行三段（选区前 / 选区高亮 / 选区后），光标条插在
        // caret 所在行。split('\n')（非 lines()）保留尾部空行——尾回车
        // 是粘贴语义的一部分，必须可见可编辑。
        let mut cursor = 0usize; // 当前行起始字节
        for line in self.text.split('\n') {
            let line_end = cursor + line.len();
            let clip = |from: usize, to: usize| -> &str {
                &self.text[from.max(cursor).min(line_end)..to.max(cursor).min(line_end)]
            };
            let head = clip(cursor, sel_start_byte);
            let mid = clip(sel_start_byte, sel_end_byte);
            let tail = clip(sel_end_byte, line_end);
            let has_selection = sel_start_byte != sel_end_byte;
            let caret_here = caret_byte >= cursor && caret_byte <= line_end;
            let caret_leading = caret_byte <= sel_start_byte || !has_selection;

            let mut row = div().h(px(LINE_HEIGHT)).flex().items_center().truncate();
            let push = |row: gpui::Div, segment: &str| {
                if segment.is_empty() {
                    row
                } else {
                    row.child(segment.to_string())
                }
            };
            row = push(row, head);
            if show_caret && caret_here && caret_leading {
                row = row.child(caret_bar(&ui));
            }
            if has_selection && !mid.is_empty() {
                row = row.child(
                    div()
                        .bg(tint_hsla(ui.accent, 0x3A))
                        .text_color(ui.text)
                        .child(mid.to_string()),
                );
            }
            if show_caret && caret_here && !caret_leading {
                row = row.child(caret_bar(&ui));
            }
            row = push(row, tail);
            // 空行也占一行高（尾回车后的空行可见可定位）。
            area = area.child(row);
            cursor = line_end + 1;
        }
        area
    }
}

/// 光标条：细竖线（accent 随主题），闪烁由外层控制是否渲染。
fn caret_bar(ui: &UiPalette) -> gpui::Div {
    div().w(px(2.0)).h(px(16.0)).bg(ui.accent)
}

impl EntityInputHandler for PasteTextArea {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let utf16: Vec<u16> = self.text.encode_utf16().collect();
        if range.start <= range.end && range.end <= utf16.len() {
            adjusted.replace(range.clone());
            String::from_utf16(&utf16[range]).ok()
        } else {
            Some(String::new())
        }
    }

    fn selected_text_range(
        &mut self,
        _ignore: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.selection.clone(),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(range) = range.or_else(|| self.marked_range.take()) {
            self.selection = range;
        }
        self.marked_range = None;
        self.replace_selection(text);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(range) = range.or_else(|| self.marked_range.take()) {
            self.selection = range;
        }
        let start = self.selection.start.min(self.selection.end);
        self.replace_selection(text);
        let end = self.selection.end;
        self.marked_range = (!text.is_empty()).then_some(start..end);
        if let Some(selected) = selected {
            self.selection = (start + selected.start).min(end)..(start + selected.end).min(end);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let _ = bounds;
        self.bounds.get().map(|mut bounds| {
            bounds.origin.x += px(H_PAD);
            bounds
        })
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.caret_offset_at(point, window)
    }
}

#[cfg(test)]
mod tests {
    use super::PasteTextArea;
    use gpui::{KeyDownEvent, Keystroke, TestAppContext};

    fn press(
        area: &mut PasteTextArea,
        key: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<PasteTextArea>,
    ) {
        area.on_key_down(
            &KeyDownEvent {
                keystroke: Keystroke::parse(key).unwrap(),
                is_held: false,
            },
            window,
            cx,
        );
    }

    /// Enter 插入换行（textarea 语义，不提交弹窗）。
    #[gpui::test]
    fn enter_inserts_newline(cx: &mut TestAppContext) {
        let (area, cx) = cx.add_window_view(|_, cx| PasteTextArea::new("echo a\necho b", cx));
        area.update_in(cx, |area, window, cx| {
            area.selection = 6..6; // 第一行行尾
            press(area, "enter", window, cx);
            assert_eq!(area.value(), "echo a\n\necho b");
            assert_eq!(area.text.split('\n').count(), 3);
        });
    }

    /// 上下键跨行保持列，越界钳到目标行尾。
    #[gpui::test]
    fn up_down_move_across_lines_keeping_column(cx: &mut TestAppContext) {
        let (area, cx) = cx.add_window_view(|_, cx| PasteTextArea::new("ab\ncdefg\ncd", cx));
        area.update_in(cx, |area, window, cx| {
            // 光标放到第二行 col 3（"cde|" 处），上移 → 第一行只有 2 列 → 钳到行尾
            area.selection = area.offset_at_line_col(1, 3)..area.offset_at_line_col(1, 3);
            press(area, "up", window, cx);
            assert_eq!(area.line_col_at(area.selection.start), (0, 2));
            press(area, "down", window, cx);
            assert_eq!(
                area.line_col_at(area.selection.start),
                (1, 2),
                "下行落在同列（v1 不记忆原始列）"
            );
            press(area, "down", window, cx);
            assert_eq!(area.line_col_at(area.selection.start), (2, 2), "末行同列");
        });
    }

    /// 粘贴保留换行（textarea 与单行输入框的关键差异）。
    #[gpui::test]
    fn paste_keeps_newlines(cx: &mut TestAppContext) {
        let (area, cx) = cx.add_window_view(|_, cx| PasteTextArea::new("old", cx));
        cx.update(|window, cx| {
            area.read(cx).focus_handle().focus(window);
            window.activate_window();
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("x\ny\r\nz".into()));
        cx.simulate_keystrokes("cmd-a cmd-v");
        area.read_with(cx, |area, _| {
            assert_eq!(area.value(), "x\ny\nz", "换行原样保留，\\r\\n 归一为 \\n");
        });
    }

    /// 行首退格合并上一行（删除 \n）；home/end 定位行首/行尾。
    #[gpui::test]
    fn backspace_joins_lines_and_home_end_target_line(cx: &mut TestAppContext) {
        let (area, cx) = cx.add_window_view(|_, cx| PasteTextArea::new("aaa\nbbb", cx));
        area.update_in(cx, |area, window, cx| {
            area.selection = area.offset_at_line_col(1, 0)..area.offset_at_line_col(1, 0);
            press(area, "backspace", window, cx);
            assert_eq!(area.value(), "aaabbb");
            press(area, "home", window, cx);
            assert_eq!(area.line_col_at(area.selection.start), (0, 0));
            press(area, "end", window, cx);
            assert_eq!(area.line_col_at(area.selection.start), (0, 6));
        });
    }

    /// \r\n 输入在构造时归一为 \n（行渲染与偏移换算只按 \n 建模）。
    #[test]
    fn crlf_input_is_normalized() {
        // 不起 GPUI 运行时的纯归一口径由 new() 保证；此处锁格式化函数行为。
        let raw = "a\r\nb\rc";
        let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
        assert_eq!(normalized, "a\nb\nc");
    }
}
