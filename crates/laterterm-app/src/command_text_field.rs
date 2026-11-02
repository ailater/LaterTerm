use gpui::{
    Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Pixels, Render, Styled,
    UTF16Selection, Window, canvas, div, px, rgb,
};

use std::ops::Range;
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
    marked_text: String,
    placeholder: String,
    /// 掩码模式（密码/密钥输入）：显示圆点，占位提示保持明文。
    masked: bool,
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
            marked_text: String::new(),
            placeholder: placeholder.into(),
            masked: false,
            focused: false,
            caret_on: true,
            blink_scheduled: false,
        }
    }

    pub(crate) fn value(&self) -> &str {
        &self.text
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
            self.caret_on = true;
            cx.notify();
            return;
        }
        if k == "enter" {
            cx.emit(FieldSubmit);
            return;
        }
        match k {
            "backspace" => self.delete_backward(),
            "delete" => self.delete_forward(),
            "left" => {
                let p = self.selection.start.min(self.selection.end);
                self.selection = p.saturating_sub(1)..p.saturating_sub(1);
                self.reversed = false;
            }
            "right" => {
                let p = self
                    .selection
                    .end
                    .max(self.selection.start)
                    .min(self.utf16_len());
                self.selection = (p + 1).min(self.utf16_len())..(p + 1).min(self.utf16_len());
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
        cx.emit(CommandTextChanged);
        cx.notify();
    }

    /// 焦点期间每半个周期翻转一次光标相位；失焦后自动停。
    fn schedule_caret_blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_scheduled {
            return;
        }
        self.blink_scheduled = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(CARET_BLINK_INTERVAL)
                .await;
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
            // 重新获得焦点时光标立即可见，从头开始闪。
            self.caret_on = true;
        }
        if focused {
            self.schedule_caret_blink(cx);
        }

        let marked = self.marked_text.clone();
        let composing = !marked.is_empty();
        let show_caret = focused && self.caret_on && !composing;
        let sel_start = self.selection.start.min(self.selection.end);
        let sel_end = self.selection.start.max(self.selection.end);

        let mut field = div()
            .track_focus(&focus)
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
            .on_key_down(cx.listener(Self::on_key_down));

        if composing {
            // 输入法组词期间整体展示 原文+预编辑，不画光标（与终端 IME 行为一致）。
            let shown = if self.masked {
                "\u{2022}".repeat(self.text.chars().count() + marked.chars().count())
            } else {
                format!("{}{}", self.text, marked)
            };
            field = field.text_color(rgb(0xC0CAF5)).child(shown);
            return field;
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
            field = field.child(
                div()
                    .bg(rgb(0x3B4661))
                    .text_color(rgb(0xE0E6F5))
                    .child(mid),
            );
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
