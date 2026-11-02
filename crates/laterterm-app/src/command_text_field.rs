use gpui::{
    Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Render, Styled, UTF16Selection, Window, anchored, canvas, deferred, div,
    prelude::FluentBuilder, px, rgb,
};
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

pub(crate) struct CommandTextChanged;

/// 用户在输入框里按下 Enter（对话框用它触发主按钮动作）。
pub(crate) struct FieldSubmit;

/// 光标闪烁周期，与系统习惯（约 530ms）一致。
const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// 可复用的单行文本输入框。文本编辑使用 UTF-16 偏移，兼容 GPUI 输入法协议；
/// 获得焦点时显示闪烁光标与高亮边框。
pub(crate) struct CommandTextField {
    focus_handle: FocusHandle,
    text: String,
    selection: Range<usize>,
    reversed: bool,
    marked_range: Option<Range<usize>>,
    placeholder: String,
    /// 掩码模式（密码/密钥输入）：显示圆点，占位提示保持明文。
    masked: bool,
    /// 字段在窗口中的几何（paint 期由 canvas 回写）；点击定位与右键菜单换算用。
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 右键菜单原点（相对字段左上角）；None = 关闭。
    context_menu: Option<gpui::Point<Pixels>>,
    /// 焦点态缓存：render 时与 `focus_handle.is_focused(window)` 对账。
    focused: bool,
    /// 光标可见相位（闪烁用）；输入动作会重置为可见。
    caret_on: bool,
    /// 是否已有闪烁任务在跑，避免 render 反复 spawn。
    blink_scheduled: bool,
}

impl EventEmitter<CommandTextChanged> for CommandTextField {}
impl EventEmitter<FieldSubmit> for CommandTextField {}

impl CommandTextField {
    pub(crate) fn new(
        text: impl Into<String>,
        placeholder: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = text.into();
        let end = text.encode_utf16().count();
        Self {
            focus_handle: cx.focus_handle(),
            text,
            selection: end..end,
            reversed: false,
            marked_range: None,
            placeholder: placeholder.into(),
            masked: false,
            bounds: Rc::new(Cell::new(None)),
            context_menu: None,
            focused: false,
            caret_on: true,
            blink_scheduled: false,
        }
    }

    pub(crate) fn value(&self) -> &str {
        &self.text
    }

    pub(crate) fn has_context_menu(&self) -> bool {
        self.context_menu.is_some()
    }

    pub(crate) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    /// 把焦点交给这个输入框（打开弹窗后调用，光标随即闪烁）。
    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 密码/密钥输入：内容以圆点显示。
    pub(crate) fn masked(mut self) -> Self {
        self.masked = true;
        self
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
        if !self.masked
            && let Some(text) = self.selection_text()
        {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    fn cut_selection(&mut self, cx: &mut Context<Self>) {
        if !self.masked
            && let Some(text) = self.selection_text()
        {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            self.delete_backward();
            self.caret_on = true;
            cx.emit(CommandTextChanged);
            cx.notify();
        }
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        // 单行输入框：换行折叠为空格，避免静默吞字
        let text = text.replace("\r\n", "\n").replace(['\r', '\n'], " ");
        self.replace_selection(&text);
        cx.emit(CommandTextChanged);
        cx.notify();
    }

    /// 根据屏幕坐标计算最近的 UTF-16 字符边界。
    fn caret_offset_at(&self, position: gpui::Point<Pixels>, window: &mut Window) -> Option<usize> {
        let bounds = self.bounds.get()?;
        let style = window.text_style();
        let font_size = match style.font_size {
            gpui::AbsoluteLength::Pixels(size) => size,
            gpui::AbsoluteLength::Rems(rems) => window.rem_size() * rems.0,
        };
        let display = self.display_text();
        let run = gpui::TextRun {
            len: display.len(),
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped =
            window
                .text_system()
                .shape_line(display.clone().into(), font_size, &[run], None);
        // 单行输入框只需按 x 求最近字符边界（utf8 字节下标）
        let byte_index = shaped.closest_index_for_x(position.x - bounds.origin.x - px(8.0));
        // 掩码后字符数与原文一致，按字符数换算回真实 UTF-16 偏移
        let char_count = display[..byte_index.min(display.len())].chars().count();
        let offset = self
            .text
            .chars()
            .take(char_count)
            .map(char::len_utf16)
            .sum::<usize>();
        Some(offset)
    }

    fn place_caret_at(&mut self, position: gpui::Point<Pixels>, window: &mut Window) {
        if let Some(offset) = self.caret_offset_at(position, window) {
            self.selection = offset..offset;
            self.reversed = false;
            self.caret_on = true;
        }
    }

    /// 渲染用文本（掩码模式返回等量圆点；字符数与原文一致）。
    fn display_text(&self) -> String {
        if self.masked {
            "\u{2022}".repeat(self.text.chars().count())
        } else {
            self.text.clone()
        }
    }

    fn on_field_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.focus_handle.focus(window);
        self.caret_on = true;
        let had_menu = self.context_menu.take().is_some();
        if !had_menu {
            self.place_caret_at(event.position, window);
        }
        cx.notify();
    }

    fn on_field_right_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.focus_handle.focus(window);
        if self.selection.start == self.selection.end {
            self.place_caret_at(event.position, window);
        }
        self.context_menu = Some(event.position);
        cx.notify();
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
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let k = event.keystroke.key.as_str();
        let ctrl = event.keystroke.modifiers.control
            && !event.keystroke.modifiers.alt
            && !event.keystroke.modifiers.shift;
        if ctrl && matches!(k.to_ascii_lowercase().as_str(), "a" | "c" | "x" | "v") {
            cx.stop_propagation();
        }
        if ctrl && k.eq_ignore_ascii_case("a") {
            self.selection = 0..self.utf16_len();
            self.reversed = false;
            self.caret_on = true;
            cx.notify();
            return;
        }
        // 剪贴板：Ctrl+C 复制选区 / Ctrl+X 剪切 / Ctrl+V 粘贴（多行折叠为空格）
        if ctrl && k.eq_ignore_ascii_case("c") {
            self.copy_selection(cx);
            return;
        }
        if ctrl && k.eq_ignore_ascii_case("x") {
            self.cut_selection(cx);
            return;
        }
        if ctrl && k.eq_ignore_ascii_case("v") {
            self.paste_clipboard(cx);
            return;
        }
        if k == "enter" {
            cx.emit(FieldSubmit);
            return;
        }
        if k == "escape" && self.context_menu.take().is_some() {
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
            "home" => self.selection = 0..0,
            "end" => {
                let p = self.utf16_len();
                self.selection = p..p;
            }
            _ => return,
        }
        self.caret_on = true;
        if self.text != previous_text {
            cx.emit(CommandTextChanged);
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
                let Ok(alive) = this.update(cx, |field, cx| {
                    if !field.focused {
                        field.blink_scheduled = false;
                        return false;
                    }
                    field.caret_on = !field.caret_on;
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

impl Render for CommandTextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity: Entity<Self> = cx.entity();
        let focus = self.focus_handle.clone();
        let focused = focus.is_focused(window);
        if focused != self.focused {
            self.focused = focused;
            // 重新获得焦点时光标立即可见；失焦时关闭右键菜单。
            self.caret_on = true;
            if !focused {
                self.context_menu = None;
            }
        }
        if focused {
            self.schedule_caret_blink(cx);
        }

        let composing = self.marked_range.is_some();
        let show_caret = focused && self.caret_on && !composing;
        let sel_start = self.selection.start.min(self.selection.end);
        let sel_end = self.selection.start.max(self.selection.end);

        let bounds_cell = self.bounds.clone();
        let mut field = div()
            .track_focus(&focus)
            .relative()
            .h(px(30.0))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .border_1()
            .rounded_sm()
            .bg(if focused {
                rgb(0x2F3147)
            } else {
                rgb(0x292B3D)
            })
            .border_color(if focused {
                rgb(0x7AA2F7)
            } else {
                rgb(0x343750)
            })
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
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_field_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_field_right_click));

        if let Some(pos) = self.context_menu {
            let item = |cx: &mut Context<Self>,
                        label: &'static str,
                        action: fn(&mut Self, &mut Context<Self>)| {
                div()
                    .h(px(26.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .text_size(px(12.0))
                    .text_color(rgb(0xC0CAF5))
                    .hover(|style| style.bg(rgb(0x2F3147)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |field, _event, _window, cx| {
                            cx.stop_propagation();
                            action(field, cx);
                            field.context_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(label)
            };
            field = field.child(
                deferred(
                    anchored().position(pos).snap_to_window().child(
                        div()
                            .w(px(136.0))
                            .flex()
                            .flex_col()
                            .py_1()
                            .bg(rgb(0x252837))
                            .border_1()
                            .border_color(rgb(0x3B3F5C))
                            .rounded_md()
                            .on_mouse_down_out(cx.listener(|field, _, _, cx| {
                                field.close_context_menu(cx);
                            }))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                            .child(item(cx, "粘贴", |field, cx| field.paste_clipboard(cx)))
                            .when(!self.masked, |menu| {
                                menu.child(item(cx, "复制", |field, cx| field.copy_selection(cx)))
                                    .child(item(cx, "剪切", |field, cx| field.cut_selection(cx)))
                            })
                            .child(item(cx, "全选", |field, cx| {
                                field.selection = 0..field.utf16_len();
                                field.reversed = false;
                                field.caret_on = true;
                                cx.notify();
                            })),
                    ),
                )
                .with_priority(1),
            );
        }

        if self.text.is_empty() {
            if show_caret {
                field = field.child(caret_bar());
            }
            field = field
                .text_color(rgb(0x565A78))
                .child(self.placeholder.clone());
            return field;
        }

        // 三段式：选区前 / 选区（高亮底色）/ 选区后；光标位置由选区方向决定。
        let collapsed = sel_start == sel_end;
        let start_byte = self.byte_offset(sel_start);
        let end_byte = self.byte_offset(sel_end);
        let mask = |segment: &str| "\u{2022}".repeat(segment.chars().count());
        let head = if self.masked {
            mask(&self.text[..start_byte])
        } else {
            self.text[..start_byte].to_string()
        };
        let mid = if self.masked {
            mask(&self.text[start_byte..end_byte])
        } else {
            self.text[start_byte..end_byte].to_string()
        };
        let tail = if self.masked {
            mask(&self.text[end_byte..])
        } else {
            self.text[end_byte..].to_string()
        };
        let caret_leading = self.reversed || collapsed;
        field = field.text_color(rgb(0xC0CAF5)).child(head);
        if show_caret && caret_leading {
            field = field.child(caret_bar());
        }
        if !collapsed {
            field = field.child(div().bg(rgb(0x3B4661)).text_color(rgb(0xE0E6F5)).child(mid));
        }
        if show_caret && !caret_leading {
            field = field.child(caret_bar());
        }
        field = field.child(tail);
        field
    }
}

/// 光标条：细竖线，闪烁由外层控制是否渲染。
fn caret_bar() -> gpui::Div {
    div().w(px(2.0)).h(px(16.0)).bg(rgb(0x7DCFFF))
}

impl EntityInputHandler for CommandTextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        if self.masked {
            return None;
        }
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
        cx.emit(CommandTextChanged);
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
            bounds.origin.x += px(8.0);
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

/// 返回光标之前一个 Unicode 标量的 UTF-8 起点。
fn previous_char_start(text: &str, end: usize) -> usize {
    text[..end]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::{CommandTextField, previous_char_start};
    use gpui::{ClipboardItem, TestAppContext};

    #[gpui::test]
    fn clipboard_shortcuts_replace_selection_and_respect_mask(cx: &mut TestAppContext) {
        let (field, cx) = cx.add_window_view(|_, cx| CommandTextField::new("旧文本", "", cx));
        cx.update(|window, cx| {
            field.read(cx).focus_handle().focus(window);
            window.activate_window();
        });
        cx.write_to_clipboard(ClipboardItem::new_string("新😀\n内容".into()));
        cx.simulate_keystrokes("ctrl-a ctrl-v");
        field.read_with(cx, |field, _| assert_eq!(field.value(), "新😀 内容"));

        cx.simulate_keystrokes("ctrl-a ctrl-c");
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("新😀 内容")
        );
        cx.simulate_keystrokes("ctrl-x");
        field.read_with(cx, |field, _| assert!(field.value().is_empty()));

        field.update(cx, |field, _| {
            field.text = "私密😀".into();
            field.selection = 0..field.utf16_len();
            field.masked = true;
        });
        cx.write_to_clipboard(ClipboardItem::new_string("保留剪贴板".into()));
        cx.simulate_keystrokes("ctrl-c ctrl-x");
        field.read_with(cx, |field, _| assert_eq!(field.value(), "私密😀"));
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("保留剪贴板")
        );
    }

    #[gpui::test]
    fn right_click_menu_pastes_into_field(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseButton, point, px};

        let (field, cx) = cx.add_window_view(|_, cx| CommandTextField::new("", "", cx));
        cx.update(|window, cx| {
            field.read(cx).focus_handle().focus(window);
            window.activate_window();
        });
        cx.write_to_clipboard(ClipboardItem::new_string("粘贴测试😀".into()));
        cx.simulate_mouse_down(
            point(px(20.0), px(15.0)),
            MouseButton::Right,
            Modifiers::none(),
        );
        field.read_with(cx, |field, _| assert!(field.has_context_menu()));
        cx.simulate_click(point(px(30.0), px(25.0)), Modifiers::none());
        field.read_with(cx, |field, _| {
            assert_eq!(field.value(), "粘贴测试😀");
            assert!(!field.has_context_menu());
        });
    }

    #[gpui::test]
    fn ime_composition_replaces_marked_range_without_losing_text(cx: &mut TestAppContext) {
        use gpui::EntityInputHandler;

        let (field, cx) = cx.add_window_view(|_, cx| CommandTextField::new("你好AB", "", cx));
        field.update_in(cx, |field, window, cx| {
            field.selection = 2..4;
            field.replace_and_mark_text_in_range(None, "zhong", None, window, cx);
            assert_eq!(field.value(), "你好zhong");
            assert_eq!(field.marked_text_range(window, cx), Some(2..7));
            field.replace_and_mark_text_in_range(None, "中", None, window, cx);
            assert_eq!(field.value(), "你好中");
            assert_eq!(field.marked_text_range(window, cx), Some(2..3));
            field.replace_text_in_range(None, "中", window, cx);
            assert_eq!(field.value(), "你好中");
            assert!(field.marked_text_range(window, cx).is_none());
        });
    }

    #[test]
    fn backward_deletion_locates_one_unicode_scalar() {
        let text = "中😀a";
        assert_eq!(&text[previous_char_start(text, text.len())..], "a");
        let end = "中😀".len();
        assert_eq!(&text[previous_char_start(text, end)..end], "😀");
        assert_eq!(previous_char_start(text, "中".len()), 0);
        assert_eq!(previous_char_start(text, 0), 0);
    }
}
