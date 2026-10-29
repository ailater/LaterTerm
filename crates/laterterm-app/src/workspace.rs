use gpui::{
    App, AppContext, AsyncApp, Context, Edges, Entity, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, StatefulInteractiveElement,
    Styled, WeakEntity, Window, div, prelude::FluentBuilder, px, rgb,
};
use laterterm_config::{Appearance, Settings, Theme, ThemeRegistry, app_config_dir};
use laterterm_core::TerminalSession;
use laterterm_local::{LocalOptions, LocalSession};
use laterterm_termview::{ColorPalette, TerminalConfig, TerminalView};
use notify::Watcher;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const TITLEBAR_HEIGHT: f32 = 34.0;
const PANEL_WIDTH: f32 = 220.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsPage {
    Appearance,
    Shortcuts,
    Terminal,
    Servers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TabId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabStatus {
    Running,
    Closing,
    Exited,
}

impl TabStatus {
    pub(crate) fn is_exited(self) -> bool {
        matches!(self, Self::Exited)
    }

    pub(crate) fn mark_closing(&mut self) -> bool {
        if matches!(self, Self::Running) {
            *self = Self::Closing;
            true
        } else {
            false
        }
    }

    pub(crate) fn mark_exited(&mut self) -> bool {
        if !matches!(self, Self::Exited) {
            *self = Self::Exited;
            true
        } else {
            false
        }
    }
}

/// 标签的会话关闭控制：持有会话契约，close 幂等。
struct SessionControl {
    session: Arc<dyn TerminalSession>,
    closing: AtomicBool,
}

impl SessionControl {
    fn new(session: Arc<dyn TerminalSession>) -> Self {
        Self {
            session,
            closing: AtomicBool::new(false),
        }
    }

    fn close(&self) -> bool {
        if self.closing.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.session.kill();
        true
    }
}

struct TerminalTab {
    id: TabId,
    title: String,
    terminal: Entity<TerminalView>,
    session: Arc<SessionControl>,
    status: TabStatus,
}

pub struct WorkspaceView {
    tabs: Vec<TerminalTab>,
    active_tab: usize,
    next_tab_id: u64,
    left_panel: bool,
    right_panel: bool,
    settings_open: bool,
    settings_page: SettingsPage,
    settings: Settings,
    themes: ThemeRegistry,
}

impl WorkspaceView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let weak_workspace = cx.weak_entity();
        let settings = Settings::load_or_default();
        settings.ensure_settings_file();
        let themes = ThemeRegistry::load();
        let first_id = TabId(1);
        let theme = themes.resolve(&settings.appearance.theme).clone();
        let (terminal, session) =
            create_local_terminal(first_id, weak_workspace, cx, &settings.appearance, &theme);
        let view = Self {
            settings,
            themes,
            settings_page: SettingsPage::Appearance,
            tabs: vec![TerminalTab {
                id: first_id,
                title: "本地终端".into(),
                terminal,
                session,
                status: TabStatus::Running,
            }],
            active_tab: 0,
            next_tab_id: 2,
            left_panel: false,
            right_panel: false,
            settings_open: false,
        };
        Self::watch_config_files(cx);
        view
    }

    fn active_tab_id(&self) -> TabId {
        self.tabs[self.active_tab].id
    }

    fn active_terminal(&self) -> Entity<TerminalView> {
        self.tabs[self.active_tab].terminal.clone()
    }

    fn new_tab(&mut self, _event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.spawn_tab(window, cx);
    }

    fn spawn_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let theme = self.themes.resolve(&self.settings.appearance.theme).clone();
        let (terminal, session) =
            create_local_terminal(id, cx.weak_entity(), cx, &self.settings.appearance, &theme);
        self.tabs.push(TerminalTab {
            id,
            title: format!("终端 {}", id.0),
            terminal,
            session,
            status: TabStatus::Running,
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
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            if self.tabs[index].status.is_exited() {
                // 已退出的标签直接移除；移除最后一个后会自动补一个新终端
                self.remove_tab_at(index, window, cx);
                return;
            }
            if self.tabs.len() == 1 {
                // 仅剩一个运行中的标签时不从标签 × 关闭，退出窗口走标题栏 ×
                return;
            }
            if !self.tabs[index].status.mark_closing() {
                return;
            }
            self.tabs[index].session.close();
            cx.notify();
        }
    }

    fn remove_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            // 空工作区没有可聚焦的终端，自动补一个本地标签
            self.spawn_tab(window, cx);
            return;
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if index < self.active_tab {
            self.active_tab -= 1;
        }
        window.set_window_title(&self.tabs[self.active_tab].title);
        self.focus_active(window, cx);
        cx.notify();
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
        cx.notify();
    }

    fn change_setting(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Appearance),
    ) {
        cx.stop_propagation();
        change(&mut self.settings.appearance);
        self.settings.save();
        self.apply_appearance(cx);
    }

    /// 把当前设置（外观五项 + 主题）应用到所有标签，不重建终端。
    fn apply_appearance(&mut self, cx: &mut Context<Self>) {
        let appearance = self.settings.appearance.clone();
        let palette = palette_for(self.themes.resolve(&appearance.theme));
        for tab in &self.tabs {
            tab.terminal.update(cx, |terminal, cx| {
                let mut config = terminal.config().clone();
                config.font_family = appearance.font_family.clone();
                config.font_size = px(appearance.font_size);
                config.line_height_multiplier = appearance.line_height;
                config.padding = Edges::all(px(appearance.padding));
                config.scrollback = appearance.scrollback;
                config.colors = palette.clone();
                terminal.update_config(config, cx);
            });
        }
        cx.notify();
    }

    /// 配置目录被外部修改后的热加载入口：重读设置与主题注册表，
    /// 有实际变化才重应用（自身保存触发的回声事件在这里被等值比较吸收）。
    fn reload_from_disk(&mut self, cx: &mut Context<Self>) {
        // 编辑中途的半截文件解析失败时保持现状，等下一次事件重试
        let settings = Settings::default_path()
            .and_then(|path| Settings::load(&path).ok())
            .unwrap_or_else(|| self.settings.clone());
        let themes = ThemeRegistry::load();
        if settings != self.settings || themes != self.themes {
            self.settings = settings;
            self.themes = themes;
            self.apply_appearance(cx);
        }
    }

    /// 监听配置目录（settings.toml 与 themes/）实现主题/设置热加载。
    fn watch_config_files(cx: &mut Context<Self>) {
        let Some(dir) = app_config_dir() else {
            return;
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let (event_tx, event_rx) = flume::unbounded::<()>();
        let Ok(mut watcher) =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                if event.is_ok() {
                    let _ = event_tx.send(());
                }
            })
        else {
            return;
        };
        if watcher
            .watch(&dir, notify::RecursiveMode::Recursive)
            .is_err()
        {
            return;
        }
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let _watcher = watcher; // 监听器随任务存活，避免被提前丢弃
            while event_rx.recv_async().await.is_ok() {
                // 非阻塞排空已排队事件（编辑器保存的连发）。
                // 本任务跑在前台执行器（主线程）上，任何阻塞等待都会卡 UI；
                // reload 幂等，多触发一次只是多读一次小文件后等值比较。
                while event_rx.try_recv().is_ok() {}
                let _ = this.update(cx, |workspace, cx| workspace.reload_from_disk(cx));
            }
        })
        .detach();
    }

    fn reset_settings(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.change_setting(event, window, cx, |appearance| {
            *appearance = Appearance::default()
        });
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

    fn select_settings_page(
        &mut self,
        page: SettingsPage,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.settings_page = page;
        cx.notify();
    }

    fn close_settings(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.settings_open = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn run_quick_command(
        &mut self,
        command: &'static str,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if matches!(self.tabs[self.active_tab].status, TabStatus::Running) {
            let terminal = self.active_terminal();
            terminal.update(cx, |terminal, _cx| {
                terminal.send_text(&format!("{command}\n"))
            });
            self.focus_active(window, cx);
        }
    }

    fn close_window(
        &mut self,
        _event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        // 退出前向所有会话发送关闭请求；SessionControl.close 幂等，wait 线程负责回收子进程
        for tab in &self.tabs {
            tab.session.close();
        }
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

    fn set_tab_title(&mut self, id: TabId, title: &str, cx: &mut Context<Self>) -> bool {
        let active_id = self.active_tab_id();
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            if tab.status.is_exited() {
                // 已退出的标签不接受迟到的 OSC 标题，避免覆盖退出后缀
                return false;
            }
            tab.title = if title.is_empty() {
                "本地终端".into()
            } else {
                title.into()
            };
            cx.notify();
            return tab.id == active_id;
        }
        false
    }

    fn mark_exited(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let closing = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .is_some_and(|tab| matches!(tab.status, TabStatus::Closing));
        if closing {
            if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                self.remove_tab_at(index, window, cx);
            }
            return;
        }
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id)
            && tab.status.mark_exited()
        {
            if !tab.title.ends_with("（已退出）") {
                tab.title.push_str("（已退出）");
            }
            cx.notify();
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
            .map(|tab| (tab.id, tab.title.clone(), tab.status.is_exited()))
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
                    .child(if right_panel { quick_panel(cx) } else { div() }),
            )
            .when(settings_open, |root| {
                root.child(settings_overlay(
                    cx.listener(Self::close_settings),
                    cx.listener(Self::close_settings),
                    settings_navigation(self.settings_page, cx),
                    settings_content(self.settings_page, self, cx),
                ))
            })
    }
}

fn quick_panel(cx: &mut Context<WorkspaceView>) -> gpui::Div {
    div()
        .w(px(PANEL_WIDTH))
        .h_full()
        .bg(rgb(0x202132))
        .p_3()
        .text_color(rgb(0xC0CAF5))
        .child(div().child("快捷命令"))
        .children(["htop", "tmux", "vim", "clear"].into_iter().map(|command| {
            div()
                .py_2()
                .text_color(rgb(0xA9B1D6))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event, window, cx| {
                        this.run_quick_command(command, event, window, cx)
                    }),
                )
                .child(command)
        }))
        .child(div().pt_4().child("快捷服务器"))
        .child(div().pt_2().text_color(rgb(0x7D829F)).child("本地 Shell"))
        .child(
            div()
                .pt_2()
                .text_color(rgb(0x565A78))
                .child("SSH 服务器将在 SSH 模块接入后可用"),
        )
}

fn settings_overlay(
    scrim_listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    close_listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    navigation: gpui::Div,
    content: gpui::Stateful<gpui::Div>,
) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .left_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::black().opacity(0.6))
        .on_mouse_down(MouseButton::Left, scrim_listener)
        .child(
            div()
                .w(px(640.0))
                .max_h(px(520.0))
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(rgb(0x202132))
                .border_1()
                .border_color(rgb(0x3B3F5C))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .h(px(52.0))
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_5()
                        .border_b_1()
                        .border_color(rgb(0x343750))
                        .child(div().text_color(rgb(0xC0CAF5)).child("设置"))
                        .child(
                            div()
                                .h(px(30.0))
                                .w(px(30.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(0xF7768E))
                                .on_mouse_down(MouseButton::Left, close_listener)
                                .child("×"),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .flex()
                        .child(navigation)
                        .child(content),
                )
                .child(
                    div()
                        .h(px(44.0))
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_5()
                        .border_t_1()
                        .border_color(rgb(0x343750))
                        .text_color(rgb(0x7D829F))
                        .child("修改即时生效")
                        .child("LaterTerm"),
                ),
        )
}

fn settings_navigation(page: SettingsPage, cx: &mut Context<WorkspaceView>) -> gpui::Div {
    let entries = [
        (SettingsPage::Appearance, "外观"),
        (SettingsPage::Shortcuts, "快捷键"),
        (SettingsPage::Terminal, "终端"),
        (SettingsPage::Servers, "服务器"),
    ];
    div()
        .w(px(150.0))
        .h_full()
        .p_3()
        .bg(rgb(0x1B1C2A))
        .flex()
        .flex_col()
        .gap_1()
        .children(entries.into_iter().map(|(entry, label)| {
            let selected = entry == page;
            div()
                .w_full()
                .px_3()
                .py_2()
                .bg(if selected {
                    rgb(0x303550)
                } else {
                    rgb(0x1B1C2A)
                })
                .text_color(if selected {
                    rgb(0x7DCEFF)
                } else {
                    rgb(0xA9B1D6)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event, window, cx| {
                        this.select_settings_page(entry, event, window, cx)
                    }),
                )
                .child(label)
        }))
}

fn settings_content(
    page: SettingsPage,
    workspace: &mut WorkspaceView,
    cx: &mut Context<WorkspaceView>,
) -> gpui::Stateful<gpui::Div> {
    let title = match page {
        SettingsPage::Appearance => "外观",
        SettingsPage::Shortcuts => "快捷键",
        SettingsPage::Terminal => "终端",
        SettingsPage::Servers => "服务器",
    };
    let body = match page {
        SettingsPage::Appearance => settings_panel(workspace, cx),
        SettingsPage::Shortcuts => div()
            .pt_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("标签"))
            .child(
                setting_card()
                    .child(shortcut_row("新建标签", "Ctrl+T"))
                    .child(shortcut_row("关闭标签", "Ctrl+W")),
            )
            .child(section_title("终端"))
            .child(
                setting_card()
                    .child(shortcut_row("搜索回滚", "Ctrl+Shift+F"))
                    .child(shortcut_row("下一个 / 上一个", "Enter / Shift+Enter")),
            ),
        SettingsPage::Terminal => div()
            .pt_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("本地会话"))
            .child(
                setting_card()
                    .child(info_row("Shell", "$SHELL / bash"))
                    .child(info_row("TERM", "xterm-256color"))
                    .child(info_row("COLORTERM", "truecolor"))
                    .child(info_row("滚动", "自然滚动，支持历史回看")),
            ),
        SettingsPage::Servers => div()
            .pt_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("连接"))
            .child(
                setting_card()
                    .child(info_row("本地 Shell", "已内置"))
                    .child(info_row("SSH", "远程模块接入后可用")),
            ),
    };
    div()
        .id(gpui::SharedString::from(format!(
            "settings-content-{page:?}"
        )))
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .h_full()
        .overflow_y_scroll()
        .p_5()
        .text_color(rgb(0xC0CAF5))
        .child(div().text_color(rgb(0xC0CAF5)).child(title))
        .child(body)
}

fn settings_panel(workspace: &mut WorkspaceView, cx: &mut Context<WorkspaceView>) -> gpui::Div {
    let settings = workspace.settings.appearance.clone();
    div()
        .pt_4()
        .flex()
        .flex_col()
        .gap_3()
        .child(section_title("终端外观"))
        .child(
            setting_card()
                .child(setting_row(
                    "字号",
                    value_chip(&format!("{:.0} px", settings.font_size)),
                    stepper_controls(
                        cx,
                        |s: &mut Appearance| s.font_size = (s.font_size - 1.0).max(8.0),
                        |s: &mut Appearance| s.font_size = (s.font_size + 1.0).min(32.0),
                    ),
                ))
                .child(setting_row(
                    "行高",
                    value_chip(&format!("{:.1}", settings.line_height)),
                    stepper_controls(
                        cx,
                        |s: &mut Appearance| {
                            s.line_height = ((s.line_height - 0.1) * 10.0).round() / 10.0;
                            s.line_height = s.line_height.clamp(0.8, 2.0);
                        },
                        |s: &mut Appearance| {
                            s.line_height = ((s.line_height + 0.1) * 10.0).round() / 10.0;
                            s.line_height = s.line_height.clamp(0.8, 2.0);
                        },
                    ),
                ))
                .child(setting_row(
                    "内边距",
                    value_chip(&format!("{:.0} px", settings.padding)),
                    stepper_controls(
                        cx,
                        |s: &mut Appearance| s.padding = (s.padding - 1.0).max(0.0),
                        |s: &mut Appearance| s.padding = (s.padding + 1.0).min(24.0),
                    ),
                )),
        )
        .child(section_title("滚动与主题"))
        .child(
            setting_card()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_2()
                        .child(div().w(px(64.0)).text_color(rgb(0xA9B1D6)).child("回滚"))
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .child(scrollback_controls(cx, &settings)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_2()
                        .child(div().w(px(64.0)).text_color(rgb(0xA9B1D6)).child("主题"))
                        .child(theme_controls(cx, &workspace.themes, &settings.theme)),
                ),
        )
        .child(setting_button(
            "恢复默认",
            false,
            cx.listener(WorkspaceView::reset_settings),
        ))
}

fn stepper_controls(
    cx: &mut Context<WorkspaceView>,
    decrease: impl Fn(&mut Appearance) + Clone + 'static,
    increase: impl Fn(&mut Appearance) + Clone + 'static,
) -> gpui::Div {
    div()
        .flex()
        .gap_1()
        .child(setting_button(
            "−",
            false,
            cx.listener(move |this, event, window, cx| {
                this.change_setting(event, window, cx, decrease.clone());
            }),
        ))
        .child(setting_button(
            "+",
            false,
            cx.listener(move |this, event, window, cx| {
                this.change_setting(event, window, cx, increase.clone());
            }),
        ))
}

fn scrollback_controls(cx: &mut Context<WorkspaceView>, settings: &Appearance) -> gpui::Div {
    div().flex().gap_1().children(
        [(10_000usize, "10k"), (50_000, "50k"), (100_000, "100k")]
            .into_iter()
            .map(|(value, label)| {
                let selected = settings.scrollback == value;
                setting_button(
                    label,
                    selected,
                    cx.listener(move |this, event, window, cx| {
                        this.change_setting(event, window, cx, move |s| s.scrollback = value);
                    }),
                )
            }),
    )
}

fn theme_controls(
    cx: &mut Context<WorkspaceView>,
    themes: &ThemeRegistry,
    current: &str,
) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .gap_1()
        .children(themes.names().into_iter().map(|name| {
            let selected = name == current;
            setting_button(
                name.clone(),
                selected,
                cx.listener(move |this, event, window, cx| {
                    this.change_setting(event, window, cx, |s| s.theme = name.clone());
                }),
            )
        }))
}

fn shortcut_row(label: &str, keys: &str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .py_2()
        .text_color(rgb(0xA9B1D6))
        .child(label.to_string())
        .child(value_chip(keys))
}

fn info_row(label: &str, value: &str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .py_2()
        .text_color(rgb(0xA9B1D6))
        .child(label.to_string())
        .child(value.to_string())
}

fn section_title(text: &str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().w(px(3.0)).h(px(14.0)).bg(rgb(0x7AA2F7)).rounded_sm())
        .child(div().text_color(rgb(0xC0CAF5)).child(text.to_string()))
}

fn setting_card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .p_3()
        .gap_1()
        .bg(rgb(0x252637))
        .border_1()
        .border_color(rgb(0x343750))
        .rounded_md()
}

fn setting_row(label: &str, chip: gpui::Div, controls: gpui::Div) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_2()
        .child(
            div()
                .w(px(64.0))
                .text_color(rgb(0xA9B1D6))
                .child(label.to_string()),
        )
        .child(chip)
        .child(controls)
}

fn value_chip(text: &str) -> gpui::Div {
    div()
        .min_w(px(64.0))
        .h(px(30.0))
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .bg(rgb(0x1A1B26))
        .border_1()
        .border_color(rgb(0x343750))
        .rounded_sm()
        .text_color(rgb(0xC0CAF5))
        .child(text.to_string())
}

fn setting_button(
    label: impl Into<gpui::SharedString>,
    selected: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let bg = if selected {
        rgb(0x2E4A6B)
    } else {
        rgb(0x252637)
    };
    let text = if selected {
        rgb(0x7DCEFF)
    } else {
        rgb(0xA9B1D6)
    };
    div()
        .h(px(30.0))
        .min_w(px(36.0))
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .bg(bg)
        .border_1()
        .border_color(if selected {
            rgb(0x7AA2F7)
        } else {
            rgb(0x343750)
        })
        .rounded_sm()
        .text_color(text)
        .hover(|this| this.bg(rgb(0x303150)))
        .on_mouse_down(MouseButton::Left, listener)
        .child(label.into())
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
    settings: &Appearance,
    theme: &Theme,
) -> (Entity<TerminalView>, Arc<SessionControl>) {
    // 会话的开启/spawn/回收线程全部内聚在 LocalSession（laterterm-local）
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let session: Arc<dyn TerminalSession> = Arc::new(
        LocalSession::spawn(LocalOptions {
            shell,
            args: vec![],
            env: vec![],
            cols: 110,
            rows: 30,
        })
        .expect("启动本地会话失败"),
    );
    let session_control = Arc::new(SessionControl::new(Arc::clone(&session)));

    let config = TerminalConfig {
        font_family: settings.font_family.clone(),
        font_size: px(settings.font_size),
        cols: 110,
        rows: 30,
        scrollback: settings.scrollback,
        line_height_multiplier: settings.line_height,
        padding: Edges::all(px(settings.padding)),
        colors: palette_for(theme),
    };

    let terminal = cx.new(|cx| {
        // resize 由视图默认回调转发给会话，无需在此另接
        TerminalView::new(Arc::clone(&session), config, cx)
            .with_title_callback({
                let workspace = workspace.clone();
                move |window, cx, title| {
                    let active = workspace
                        .update(cx, |workspace, cx| workspace.set_tab_title(id, title, cx))
                        .unwrap_or(false);
                    if active {
                        window.set_window_title(if title.is_empty() { "LaterTerm" } else { title });
                    }
                }
            })
            .with_exit_callback(move |window, cx| {
                let _ = workspace.update(cx, |workspace, cx| workspace.mark_exited(id, window, cx));
            })
    });
    (terminal, session_control)
}

/// laterterm_config 主题 → termview 调色板。
///
/// builder 的颜色方法都是 (r, g, b) 三参形式，这里用宏把 0xRRGGBB 打包值展开进去。
macro_rules! set_color {
    ($builder:expr, $method:ident, $value:expr) => {
        $builder = $builder.$method(
            ($value >> 16) as u8,
            (($value >> 8) & 0xFF) as u8,
            ($value & 0xFF) as u8,
        )
    };
}

fn palette_for(theme: &Theme) -> ColorPalette {
    let colors = &theme.colors;
    let mut builder = ColorPalette::builder();
    set_color!(builder, background, colors.background);
    set_color!(builder, foreground, colors.foreground);
    set_color!(builder, cursor, colors.cursor);
    set_color!(builder, black, colors.black);
    set_color!(builder, red, colors.red);
    set_color!(builder, green, colors.green);
    set_color!(builder, yellow, colors.yellow);
    set_color!(builder, blue, colors.blue);
    set_color!(builder, magenta, colors.magenta);
    set_color!(builder, cyan, colors.cyan);
    set_color!(builder, white, colors.white);
    set_color!(builder, bright_black, colors.bright_black);
    set_color!(builder, bright_red, colors.bright_red);
    set_color!(builder, bright_green, colors.bright_green);
    set_color!(builder, bright_yellow, colors.bright_yellow);
    set_color!(builder, bright_blue, colors.bright_blue);
    set_color!(builder, bright_magenta, colors.bright_magenta);
    set_color!(builder, bright_cyan, colors.bright_cyan);
    set_color!(builder, bright_white, colors.bright_white);
    builder.build()
}
