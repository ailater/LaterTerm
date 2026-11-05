use gpui::{
    Context, EventEmitter, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Render, Styled, Window, div, prelude::FluentBuilder, px,
};
use laterterm_config::normalize_shortcut;

pub(crate) struct ShortcutChanged;

pub(crate) struct ShortcutRecorder {
    focus_handle: FocusHandle,
    value: Option<String>,
    recording: bool,
    error: Option<String>,
    subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<ShortcutChanged> for ShortcutRecorder {}

impl ShortcutRecorder {
    pub(crate) fn new(value: Option<String>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            value,
            recording: false,
            error: None,
            subscriptions: Vec::new(),
        }
    }

    pub(crate) fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    pub(crate) fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub(crate) fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.observe_focus(window, cx);
        self.recording = true;
        self.error = None;
        self.focus_handle.focus(window);
        cx.notify();
    }

    // new 尚无 Window，首次 render/start 才注册；订阅随控件销毁释放。
    fn observe_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.subscriptions.is_empty() {
            return;
        }
        self.subscriptions
            .push(cx.on_blur(&self.focus_handle, window, |this, _, cx| {
                this.cancel(cx);
            }));
        self.subscriptions
            .push(cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.cancel(cx);
                }
            }));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.recording {
            self.recording = false;
            self.error = None;
            cx.notify();
        }
    }

    pub(crate) fn is_recording(&self) -> bool {
        self.recording
    }

    pub(crate) fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.error = error;
        cx.notify();
    }

    pub(crate) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus_handle.is_focused(window) {
            self.cancel(cx);
            return;
        }
        let key = event.keystroke.key.to_ascii_lowercase();
        let bare = !event.keystroke.modifiers.modified();
        // 非录制的裸 Esc 留给外层关闭弹窗；其他键（含重复事件）绝不穿透。
        if !self.recording && bare && key == "escape" && !event.is_held {
            return;
        }
        cx.stop_propagation();
        if event.is_held {
            return;
        }
        if !self.recording {
            if bare && matches!(key.as_str(), "enter" | "space") {
                self.start(window, cx);
            }
            return;
        }
        if bare && key == "escape" {
            self.cancel(cx);
            return;
        }
        // 等待主按键，不在用户刚按下 Ctrl/Alt 等时显示错误。
        if is_modifier_key(&key) {
            return;
        }
        match shortcut_for_keystroke(&event.keystroke) {
            Ok(shortcut) => {
                self.value = Some(shortcut);
                self.recording = false;
                self.error = None;
                cx.emit(ShortcutChanged);
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.value = None;
        self.error = None;
        self.recording = false;
        cx.emit(ShortcutChanged);
        cx.notify();
    }
}

impl Render for ShortcutRecorder {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.observe_focus(window, cx);
        let focus = self.focus_handle.clone();
        let label = if self.recording {
            "请按下快捷键（Esc 取消）".to_string()
        } else {
            self.value.clone().unwrap_or_else(|| "未设置".to_string())
        };
        let error = self.error.clone();
        div()
            .track_focus(&focus)
            .flex()
            .flex_col()
            .gap(px(6.0))
            .w_full()
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(gpui::rgb(if self.recording {
                                0x7aa2f7
                            } else {
                                0x343750
                            }))
                            .bg(gpui::rgb(0x292b3d))
                            .text_color(gpui::rgb(0xc0caf5))
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(gpui::rgb(0x343750))
                            .child("录制")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.start(window, cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(gpui::rgb(0x343750))
                            .child("清空")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.focus_handle.focus(window);
                                    this.clear(cx);
                                }),
                            ),
                    ),
            )
            .when_some(error, |this, error| {
                this.child(div().text_color(gpui::rgb(0xf7768e)).child(error))
            })
    }
}

pub(crate) fn shortcut_for_keystroke(key: &gpui::Keystroke) -> Result<String, String> {
    let modifiers = key.modifiers;
    if modifiers.function {
        return Err("不支持 Fn 修饰键".into());
    }
    if !modifiers.control && !modifiers.alt && !modifiers.shift && !modifiers.platform {
        return Err("快捷键至少需要一个修饰键".into());
    }
    let name = key.key.trim().to_ascii_lowercase();
    if name.is_empty() || is_modifier_key(&name) {
        return Err("请选择一个非修饰主按键".into());
    }
    let mut raw = String::new();
    if modifiers.control {
        raw.push_str("control+");
    }
    if modifiers.alt {
        raw.push_str("alt+");
    }
    if modifiers.shift {
        raw.push_str("shift+");
    }
    if modifiers.platform {
        raw.push_str("cmd+");
    }
    raw.push_str(&name);
    normalize_shortcut(&raw).map_err(|error| error.to_string())
}

pub(crate) fn matches_shortcut(key: &gpui::Keystroke, shortcut: &str) -> bool {
    let Ok(normalized) = normalize_shortcut(shortcut) else {
        return false;
    };
    let mut parts = normalized.split('+').collect::<Vec<_>>();
    let wanted = parts.pop().unwrap_or_default();
    let m = key.modifiers;
    let has = |name: &str| parts.contains(&name);
    !m.function
        && m.control == has("ctrl")
        && m.alt == has("alt")
        && m.shift == has("shift")
        && m.platform == has("cmd")
        && key.key.eq_ignore_ascii_case(wanted)
}

fn is_modifier_key(key: &str) -> bool {
    matches!(
        key,
        "ctrl"
            | "control"
            | "alt"
            | "shift"
            | "cmd"
            | "command"
            | "super"
            | "meta"
            | "platform"
            | "fn"
    )
}

#[cfg(test)]
mod tests {
    use super::shortcut_for_keystroke;
    use gpui::Keystroke;

    #[test]
    fn records_and_matches_grave_and_tilde() {
        for (name, shift, expected) in [
            ("`", false, "ctrl+`"),
            ("~", false, "ctrl+~"),
            ("~", true, "ctrl+shift+~"),
        ] {
            let key = Keystroke {
                modifiers: gpui::Modifiers { control: true, shift, ..Default::default() },
                key: name.into(),
                key_char: None,
            };
            let recorded = shortcut_for_keystroke(&key).unwrap();
            assert_eq!(recorded, expected);
            assert!(super::matches_shortcut(&key, &recorded));
        }
    }

    #[test]
    fn formats_supported_modifiers_in_config_order() {
        let key = Keystroke::parse("shift-ctrl-k").expect("keystroke");
        assert_eq!(shortcut_for_keystroke(&key).unwrap(), "ctrl+shift+k");
    }

    #[test]
    fn rejects_pure_modifier_keystrokes() {
        let key = Keystroke {
            modifiers: gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            key: "control".into(),
            key_char: None,
        };
        assert!(shortcut_for_keystroke(&key).is_err());
    }

    #[test]
    fn accepts_f1_through_f24_but_rejects_fn_modifier() {
        for number in 1..=24 {
            let key = Keystroke::parse(&format!("ctrl-f{number}")).unwrap();
            assert_eq!(
                shortcut_for_keystroke(&key).unwrap(),
                format!("ctrl+f{number}")
            );
        }
        let key = Keystroke::parse("fn-ctrl-k").unwrap();
        assert!(shortcut_for_keystroke(&key).is_err());
    }

    #[gpui::test]
    fn escape_only_cancels_without_modifiers(cx: &mut gpui::TestAppContext) {
        let (recorder, cx) =
            cx.add_window_view(|_, cx| super::ShortcutRecorder::new(Some(" Ctrl+J ".into()), cx));
        recorder.update_in(cx, |view, window, cx| {
            window.activate_window();
            view.start(window, cx);
        });
        cx.simulate_keystrokes("escape");
        recorder.read_with(cx, |view, _| {
            assert!(!view.is_recording());
            assert_eq!(view.value(), Some(" Ctrl+J "));
        });
        recorder.update_in(cx, |view, window, cx| view.start(window, cx));
        cx.simulate_keystrokes("ctrl-escape");
        recorder.read_with(cx, |view, _| {
            assert!(!view.is_recording());
            assert_eq!(view.value(), Some("ctrl+escape"));
        });
    }

    #[gpui::test]
    fn idle_modified_enter_does_not_start_and_modifiers_wait(cx: &mut gpui::TestAppContext) {
        let (recorder, cx) = cx.add_window_view(|_, cx| super::ShortcutRecorder::new(None, cx));
        recorder.update_in(cx, |view, window, _| {
            window.activate_window();
            view.focus_handle().focus(window);
        });
        cx.simulate_keystrokes("ctrl-enter");
        recorder.read_with(cx, |view, _| assert!(!view.is_recording()));
        cx.simulate_keystrokes("space");
        recorder.update_in(cx, |view, window, cx| {
            let event = gpui::KeyDownEvent {
                keystroke: Keystroke {
                    key: "control".into(),
                    modifiers: gpui::Modifiers {
                        control: true,
                        ..Default::default()
                    },
                    key_char: None,
                },
                is_held: false,
            };
            view.handle_key_down(&event, window, cx);
            assert!(view.is_recording());
            assert!(view.error.is_none());
        });
    }

    #[gpui::test]
    fn held_keys_do_not_record_or_restart_and_clear_emits(cx: &mut gpui::TestAppContext) {
        use std::{cell::Cell, rc::Rc};
        let (recorder, cx) =
            cx.add_window_view(|_, cx| super::ShortcutRecorder::new(Some("ctrl+j".into()), cx));
        let changes = Rc::new(Cell::new(0));
        let count = changes.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&recorder, move |_, _: &super::ShortcutChanged, _| {
                count.set(count.get() + 1)
            })
        });
        recorder.update_in(cx, |view, window, cx| {
            window.activate_window();
            view.start(window, cx);
            view.handle_key_down(
                &gpui::KeyDownEvent {
                    keystroke: Keystroke::parse("ctrl-k").unwrap(),
                    is_held: true,
                },
                window,
                cx,
            );
            assert!(view.is_recording());
            assert_eq!(view.value(), Some("ctrl+j"));
        });
        cx.simulate_keystrokes("ctrl-k");
        recorder.update_in(cx, |view, window, cx| {
            assert!(view.focus_handle().is_focused(window));
            view.handle_key_down(
                &gpui::KeyDownEvent {
                    keystroke: Keystroke::parse("enter").unwrap(),
                    is_held: true,
                },
                window,
                cx,
            );
            assert!(!view.is_recording());
            view.clear(cx);
            assert_eq!(view.value(), None);
            assert!(!view.is_recording());
        });
        assert_eq!(changes.get(), 2);
    }

    #[gpui::test]
    fn blur_cancels_without_render(cx: &mut gpui::TestAppContext) {
        let (recorder, cx) = cx.add_window_view(|_, cx| super::ShortcutRecorder::new(None, cx));
        recorder.update_in(cx, |view, window, cx| {
            window.activate_window();
            view.start(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, _| window.blur());
        cx.run_until_parked();
        recorder.read_with(cx, |view, _| assert!(!view.is_recording()));
    }

    #[gpui::test]
    fn deactivation_cancels_even_if_focus_is_retained(cx: &mut gpui::TestAppContext) {
        let (recorder, cx) = cx.add_window_view(|_, cx| super::ShortcutRecorder::new(None, cx));
        recorder.update_in(cx, |view, window, cx| {
            window.activate_window();
            view.start(window, cx);
        });
        cx.run_until_parked();
        cx.deactivate_window();
        cx.run_until_parked();
        recorder.read_with(cx, |view, _| assert!(!view.is_recording()));
    }

    #[test]
    fn uses_key_not_key_char_for_non_text_keys() {
        let key = Keystroke::parse("ctrl-enter").expect("keystroke");
        assert_eq!(shortcut_for_keystroke(&key).unwrap(), "ctrl+enter");
    }
}
