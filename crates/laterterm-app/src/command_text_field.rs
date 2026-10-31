use gpui::{
    Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Pixels, Render, Styled,
    UTF16Selection, Window, canvas, div, px,
};
use std::ops::Range;

pub(crate) struct CommandTextChanged;

/// 可复用的单行文本输入框。文本编辑使用 UTF-16 偏移，兼容 GPUI 输入法协议。
pub(crate) struct CommandTextField {
    focus_handle: FocusHandle,
    text: String,
    selection: Range<usize>,
    reversed: bool,
    marked_text: String,
    placeholder: String,
}

impl EventEmitter<CommandTextChanged> for CommandTextField {}

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
            marked_text: String::new(),
            placeholder: placeholder.into(),
        }
    }

    pub(crate) fn value(&self) -> &str {
        &self.text
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
    }
    fn delete_backward(&mut self) {
        if self.selection.start != self.selection.end {
            self.replace_selection("");
            return;
        }
        if self.selection.start == 0 {
            return;
        }
        let mut chars = self.text[..self.byte_offset(self.selection.start)].char_indices();
        chars.next_back();
        let start = chars.next_back().map(|(i, _)| i).unwrap_or(0);
        let end = self.byte_offset(self.selection.start);
        self.text.replace_range(start..end, "");
        self.selection =
            self.selection.start.saturating_sub(1)..self.selection.start.saturating_sub(1);
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
        if event.keystroke.modifiers.control && k.eq_ignore_ascii_case("a") {
            self.selection = 0..self.utf16_len();
            self.reversed = false;
            cx.notify();
            return;
        }
        match k {
            "backspace" => self.delete_backward(),
            "delete" => self.delete_forward(),
            "left" => {
                let p = self.selection.start.min(self.selection.end);
                self.selection = p.saturating_sub(1)..p.saturating_sub(1);
            }
            "right" => {
                let p = self
                    .selection
                    .end
                    .max(self.selection.start)
                    .min(self.utf16_len());
                self.selection = (p + 1).min(self.utf16_len())..(p + 1).min(self.utf16_len());
            }
            "home" => self.selection = 0..0,
            "end" => {
                let p = self.utf16_len();
                self.selection = p..p;
            }
            _ => return,
        }
        cx.emit(CommandTextChanged);
        cx.notify();
    }
}

impl Render for CommandTextField {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity: Entity<Self> = cx.entity();
        let focus = self.focus_handle.clone();
        let text = if self.text.is_empty() {
            self.placeholder.clone()
        } else {
            self.text.clone()
        };
        let marked = self.marked_text.clone();
        div()
            .track_focus(&focus)
            .h(px(30.0))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .bg(gpui::rgb(0x292B3D))
            .text_color(if self.text.is_empty() {
                gpui::rgb(0x565A78)
            } else {
                gpui::rgb(0xC0CAF5)
            })
            .child(format!("{}{}", text, marked))
            .child(canvas(
                move |bounds, _window, _cx| bounds,
                move |bounds, _, window, cx| {
                    window.handle_input(
                        &focus,
                        ElementInputHandler::new(bounds, entity.clone()),
                        cx,
                    );
                },
            ))
            .on_key_down(cx.listener(Self::on_key_down))
    }
}

impl EntityInputHandler for CommandTextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let utf16: Vec<u16> = self.text.encode_utf16().collect();
        if range.end <= utf16.len() {
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
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(range) = range {
            self.selection = range;
        }
        self.marked_text.clear();
        self.replace_selection(text);
        cx.emit(CommandTextChanged);
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _selected: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(range) = range {
            self.selection = range;
        }
        self.marked_text = text.to_string();
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
    }
    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.selection.start)
    }
}
