//! LaterTerm — 跨平台 GPU 终端。
//!
//! M0 里程碑：单窗口 + 本地 shell 会话，验证 GPUI 渲染与 PTY 链路。

use anyhow::Result;
use gpui::{
    AppContext, Context, Edges, Entity, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, Styled, Window, div, px,
};
use laterterm_termview::{ColorPalette, TerminalConfig, TerminalView};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::sync::Arc;

/// 外壳视图：持有终端实体，处理全局快捷键（字号增减）。
struct LaterTermApp {
    terminal: Entity<TerminalView>,
}

impl LaterTermApp {
    fn new(terminal: Entity<TerminalView>) -> Self {
        Self { terminal }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;

        // Ctrl+= / Ctrl++ 放大，Ctrl+- 缩小
        if keystroke.modifiers.control && (keystroke.key == "+" || keystroke.key == "=") {
            self.terminal.update(cx, |terminal, cx| {
                let mut config = terminal.config().clone();
                config.font_size += px(1.0);
                terminal.update_config(config, cx);
            });
            cx.stop_propagation();
        } else if keystroke.modifiers.control && keystroke.key == "-" {
            self.terminal.update(cx, |terminal, cx| {
                let mut config = terminal.config().clone();
                if config.font_size > px(6.0) {
                    config.font_size -= px(1.0);
                    terminal.update_config(config, cx);
                }
            });
            cx.stop_propagation();
        }
    }
}

impl Render for LaterTermApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .on_key_down(cx.listener(Self::on_key_down))
            .child(self.terminal.clone())
    }
}

/// Tokyo Night 配色（与 LaterScreen/LaterMD 家族同一审美）。
fn tokyo_night() -> ColorPalette {
    ColorPalette::builder()
        .background(0x1A, 0x1B, 0x26)
        .foreground(0xC0, 0xCA, 0xF5)
        .cursor(0xC0, 0xCA, 0xF5)
        // 普通色
        .black(0x15, 0x16, 0x1E)
        .red(0xF7, 0x76, 0x8E)
        .green(0x9E, 0xCE, 0x6A)
        .yellow(0xE0, 0xAF, 0x68)
        .blue(0x7A, 0xA2, 0xF7)
        .magenta(0xBB, 0x9A, 0xF7)
        .cyan(0x7D, 0xCF, 0xFF)
        .white(0xA9, 0xB1, 0xD6)
        // 亮色
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

fn main() -> Result<()> {
    let app = gpui::Application::new();

    app.run(move |cx| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 30,
                cols: 110,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("打开 PTY 失败");

        let mut cmd = CommandBuilder::new(&shell);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let _child = pair.slave.spawn_command(cmd).expect("启动 shell 失败");

        let writer = pair.master.take_writer().expect("获取 PTY 写端失败");
        let reader = pair.master.try_clone_reader().expect("获取 PTY 读端失败");
        let pty_master = Arc::new(parking_lot::Mutex::new(pair.master));
        drop(pair.slave);

        let pty_for_resize = pty_master.clone();
        cx.spawn(async move |cx| {
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

            let resize_callback = move |cols: usize, rows: usize| {
                if let Err(e) = pty_for_resize.lock().resize(PtySize {
                    cols: cols as u16,
                    rows: rows as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    eprintln!("PTY resize 失败: {}", e);
                }
            };

            cx.open_window(
                gpui::WindowOptions {
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("LaterTerm".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let terminal = cx.new(|cx| {
                        TerminalView::new(writer, reader, config, cx)
                            .with_resize_callback(resize_callback)
                            .with_exit_callback(|_window, cx| {
                                cx.quit();
                            })
                    });

                    terminal.read(cx).focus_handle().focus(window);
                    cx.new(|_cx| LaterTermApp::new(terminal))
                },
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });

    Ok(())
}
