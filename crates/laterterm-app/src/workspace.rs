use gpui::{
    App, AppContext, Context, Edges, Entity, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Render, Styled, Window, div, px, rgb,
};
use laterterm_termview::{ColorPalette, TerminalConfig, TerminalView};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::sync::Arc;

const TITLEBAR_HEIGHT: f32 = 34.0;
const PANEL_WIDTH: f32 = 220.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TabId(u64);

struct TerminalTab {
    id: TabId,
    title: String,
    terminal: Entity<TerminalView>,
    exited: bool,
}

pub struct WorkspaceView {
    tabs: Vec<TerminalTab>,
    active_tab: usize,
    next_tab_id: u64,
    left_panel: bool,
    right_panel: bool,
    settings_open: bool,
}

impl WorkspaceView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let weak_workspace = cx.weak_entity();
        let first_id = TabId(1);
        let terminal = create_local_terminal(first_id, weak_workspace, cx);
        Self {
            tabs: vec![TerminalTab {
                id: first_id,
                title: "本地终端".into(),
                terminal,
                exited: false,
            }],
            active_tab: 0,
            next_tab_id: 2,
            left_panel: false,
            right_panel: false,
            settings_open: false,
        }
    }

    fn active_tab_id(&self) -> TabId {
        self.tabs[self.active_tab].id
    }

    fn active_terminal(&self) -> Entity<TerminalView> {
        self.tabs[self.active_tab].terminal.clone()
    }

    fn new_tab(&mut self, _event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let terminal = create_local_terminal(id, cx.weak_entity(), cx);
        self.tabs.push(TerminalTab {
            id,
            title: format!("终端 {}", id.0),
            terminal,
            exited: false,
        });
        self.active_tab = self.tabs.len() - 1;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn activate_tab(
        &mut self,
        id: TabId,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.active_tab = index;
            window.set_window_title(&self.tabs[index].title);
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn close_tab(
        &mut self,
        id: TabId,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.tabs.len() == 1 {
            return;
        }
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.tabs.remove(index);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            } else if index < self.active_tab {
                self.active_tab -= 1;
            }
            window.set_window_title(&self.tabs[self.active_tab].title);
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn toggle_left_panel(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.left_panel = !self.left_panel;
        cx.notify();
    }

    fn toggle_right_panel(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.right_panel = !self.right_panel;
        cx.notify();
    }

    fn toggle_settings(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.settings_open = !self.settings_open;
        self.right_panel = self.settings_open;
        cx.notify();
    }

    fn start_move(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        window.start_window_move();
    }

    fn minimize(&mut self, _event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.minimize_window();
    }

    fn zoom(&mut self, _event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.zoom_window();
    }

    fn close_window(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        cx.quit();
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.active_terminal().read(cx).focus_handle().focus(window);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.control && key == "t" {
            self.new_tab(
                &MouseDownEvent {
                    position: gpui::point(px(0.0), px(0.0)),
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: true,
                },
                window,
                cx,
            );
            cx.stop_propagation();
        } else if event.keystroke.modifiers.control && key == "w" {
            let id = self.active_tab_id();
            self.close_tab(
                id,
                &MouseDownEvent {
                    position: gpui::point(px(0.0), px(0.0)),
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: true,
                },
                window,
                cx,
            );
            cx.stop_propagation();
        }
    }

    fn set_tab_title(&mut self, id: TabId, title: &str) -> bool {
        let active_id = self.active_tab_id();
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            tab.title = if title.is_empty() {
                "本地终端".into()
            } else {
                title.into()
            };
            return tab.id == active_id;
        }
        false
    }

    fn mark_exited(&mut self, id: TabId, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            if !tab.exited {
                tab.exited = true;
                tab.title = format!("{}（已退出）", tab.title);
                cx.notify();
            }
        }
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_terminal = self.active_terminal();
        let active_id = self.active_tab_id();
        let tabs = self
            .tabs
            .iter()
            .map(|tab| (tab.id, tab.title.clone(), tab.exited))
            .collect::<Vec<_>>();
        let left_panel = self.left_panel;
        let right_panel = self.right_panel;
        let settings_open = self.settings_open;

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1A1B26))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(
                div()
                    .h(px(TITLEBAR_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .bg(rgb(0x161722))
                    .child(
                        div()
                            .h_full()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .child(button("☰", cx.listener(Self::toggle_left_panel)))
                            .child(div().text_color(rgb(0xC0CAF5)).child("LaterTerm")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .overflow_x_hidden()
                            .children(tabs.into_iter().map(|(id, title, exited)| {
                                let is_active = id == active_id;
                                div()
                                    .h_full()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .px_3()
                                    .bg(if is_active {
                                        rgb(0x292B3D)
                                    } else {
                                        rgb(0x161722)
                                    })
                                    .text_color(if exited { rgb(0x565A78) } else { rgb(0xC0CAF5) })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, event, window, cx| {
                                            this.activate_tab(id, event, window, cx)
                                        }),
                                    )
                                    .child(title)
                                    .child(
                                        div()
                                            .px_1()
                                            .text_color(rgb(0x7D829F))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, event, window, cx| {
                                                    this.close_tab(id, event, window, cx)
                                                }),
                                            )
                                            .child("×"),
                                    )
                            }))
                            .child(button("+", cx.listener(Self::new_tab)))
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::start_move)),
                    )
                    .child(button("▣", cx.listener(Self::toggle_right_panel)))
                    .child(button("⚙", cx.listener(Self::toggle_settings)))
                    .child(button("—", cx.listener(Self::minimize)))
                    .child(button("□", cx.listener(Self::zoom)))
                    .child(button_with_color(
                        "×",
                        0xF7768E,
                        cx.listener(Self::close_window),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .child(if left_panel {
                        div()
                            .w(px(PANEL_WIDTH))
                            .h_full()
                            .bg(rgb(0x202132))
                            .p_3()
                            .child(div().text_color(rgb(0xC0CAF5)).child("会话"))
                            .children(self.tabs.iter().map(|tab| {
                                div()
                                    .py_1()
                                    .text_color(if tab.id == active_id {
                                        rgb(0x7DCEFF)
                                    } else {
                                        rgb(0xA9B1D6)
                                    })
                                    .child(tab.title.clone())
                            }))
                    } else {
                        div()
                    })
                    .child(div().flex_1().min_w(px(0.0)).child(active_terminal))
                    .child(if right_panel {
                        div()
                            .w(px(PANEL_WIDTH))
                            .h_full()
                            .bg(rgb(0x202132))
                            .p_3()
                            .child(div().text_color(rgb(0xC0CAF5)).child(if settings_open {
                                "设置"
                            } else {
                                "当前会话"
                            }))
                            .child(
                                div()
                                    .pt_3()
                                    .text_color(rgb(0xA9B1D6))
                                    .child(if settings_open {
                                        "外观设置将在这里生效：字体、字号、主题和终端间距。"
                                    } else {
                                        "终端会话信息、快捷命令和远程连接入口。"
                                    }),
                            )
                    } else {
                        div()
                    }),
            )
    }
}

fn button(
    label: &'static str,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    button_with_color(label, 0xA9B1D6, listener)
}

fn button_with_color(
    label: &'static str,
    color: u32,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .h_full()
        .min_w(px(32.0))
        .flex()
        .items_center()
        .justify_center()
        .px_2()
        .text_color(rgb(color))
        .on_mouse_down(MouseButton::Left, listener)
        .child(label)
}

fn create_local_terminal(
    id: TabId,
    workspace: gpui::WeakEntity<WorkspaceView>,
    cx: &mut Context<WorkspaceView>,
) -> Entity<TerminalView> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 110,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("打开 PTY 失败");
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut command = CommandBuilder::new(shell);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    let mut child = pair.slave.spawn_command(command).expect("启动 shell 失败");
    std::thread::spawn(move || {
        if let Err(error) = child.wait() {
            eprintln!("回收 shell 子进程失败: {error}");
        }
    });

    let writer = pair.master.take_writer().expect("获取 PTY 写端失败");
    let reader = pair.master.try_clone_reader().expect("获取 PTY 读端失败");
    let pty_master = Arc::new(parking_lot::Mutex::new(pair.master));
    drop(pair.slave);
    let pty_for_resize = pty_master;
    let config = TerminalConfig {
        font_family: "JetBrainsMono Nerd Font".into(),
        font_size: px(13.0),
        cols: 110,
        rows: 30,
        scrollback: 100_000,
        line_height_multiplier: 1.0,
        padding: Edges::all(px(8.0)),
        colors: tokyo_night(),
    };

    cx.new(|cx| {
        TerminalView::new(writer, reader, config, cx)
            .with_resize_callback(move |cols, rows| {
                if let Err(error) = pty_for_resize.lock().resize(PtySize {
                    cols: cols as u16,
                    rows: rows as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    eprintln!("PTY resize 失败: {error}");
                }
            })
            .with_title_callback({
                let workspace = workspace.clone();
                move |window, cx, title| {
                    let active = workspace
                        .update(cx, |workspace, _cx| workspace.set_tab_title(id, title))
                        .unwrap_or(false);
                    if active {
                        window.set_window_title(if title.is_empty() { "LaterTerm" } else { title });
                    }
                }
            })
            .with_exit_callback(move |_window, cx| {
                let _ = workspace.update(cx, |workspace, cx| workspace.mark_exited(id, cx));
            })
    })
}

fn tokyo_night() -> ColorPalette {
    ColorPalette::builder()
        .background(0x1A, 0x1B, 0x26)
        .foreground(0xC0, 0xCA, 0xF5)
        .cursor(0xC0, 0xCA, 0xF5)
        .black(0x15, 0x16, 0x1E)
        .red(0xF7, 0x76, 0x8E)
        .green(0x9E, 0xCE, 0x6A)
        .yellow(0xE0, 0xAF, 0x68)
        .blue(0x7A, 0xA2, 0xF7)
        .magenta(0xBB, 0x9A, 0xF7)
        .cyan(0x7D, 0xCF, 0xFF)
        .white(0xA9, 0xB1, 0xD6)
        .bright_black(0x41, 0x48, 0x68)
        .bright_red(0xF7, 0x76, 0x8E)
        .bright_green(0x9E, 0xCE, 0x6A)
        .bright_yellow(0xE0, 0xAF, 0x68)
        .bright_blue(0x7A, 0xA2, 0xF7)
        .bright_magenta(0xBB, 0x9A, 0xF7)
        .bright_cyan(0x7D, 0xCF, 0xFF)
        .bright_white(0xC0, 0xCA, 0xF5)
        .build()
}
