//! LaterTerm — 跨平台 GPU 终端。

mod branding;
mod command_text_field;
mod import;
mod onboarding;
mod pane_navigation;
mod quake;
mod server_connection;
mod sftp_panel;
mod shortcut_recorder;
mod update_check;
mod workspace;
#[cfg(test)]
mod workspace_tests;

use anyhow::Result;
use gpui::AppContext;
use laterterm_config::Settings;
use workspace::WorkspaceView;

/// 启动期共享设置：main 只读一次 settings.toml，窗口几何判断与
/// WorkspaceView 共用，消掉启动路径的双读双解析。WorkspaceView::new
/// 取走（move）；测试等直连构造路径无此全局，自行回退加载。
struct StartupSettings(Settings);

impl gpui::Global for StartupSettings {}

fn window_options(cx: &mut gpui::App, settings: &Settings) -> gpui::WindowOptions {
    let mut options = gpui::WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("LaterTerm".into()),
            appears_transparent: true,
            ..Default::default()
        }),
        app_id: Some(branding::APP_ID.into()),
        window_decorations: Some(gpui::WindowDecorations::Client),
        ..Default::default()
    };
    // Quake 几何仅 Linux + 已启用时：全屏宽 × 50% 高、顶部水平居中。
    // 窗口位置只能在创建时经 WindowOptions 给出（运行期无 set_position 公开口），
    // 故开关切换后几何在下一次启动生效；X11 创建时吃 origin（定位可靠），
    // Wayland xdg-shell 忽略 origin，由合成器摆位。
    #[cfg(target_os = "linux")]
    {
        if settings.quake.enabled
            && let Some(display) = cx.primary_display()
        {
            let screen = display.bounds();
            let height = (screen.size.height / 2.0).max(gpui::px(200.0));
            options.window_bounds = Some(gpui::WindowBounds::Windowed(gpui::Bounds {
                origin: screen.origin,
                size: gpui::size(screen.size.width, height),
            }));
        }
    }
    // 非 Linux 平台不读 quake 设置，参数仅为签名统一。
    #[cfg(not(target_os = "linux"))]
    let _ = cx;
    #[cfg(not(target_os = "linux"))]
    let _ = settings;
    #[cfg(target_os = "macos")]
    let options = {
        let mut options = options;
        if let Some(titlebar) = &mut options.titlebar {
            titlebar.traffic_light_position = Some(gpui::point(gpui::px(12.0), gpui::px(10.0)));
        }
        options
    };
    options
}

fn main() -> Result<()> {
    let app = gpui::Application::new();
    app.run(|cx| {
        // 启动路径仅此一次读盘+解析 settings.toml：先算 Quake 窗口几何，
        // 再把同一份设置共享给 WorkspaceView（new 里取走，避免二次加载）。
        let settings = Settings::load_or_default();
        let options = window_options(cx, &settings);
        cx.set_global(StartupSettings(settings));
        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                let workspace = cx.new(|cx| WorkspaceView::new(window, cx));
                workspace.update(cx, |workspace, cx| workspace.focus_active(window, cx));
                let weak = workspace.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    let _ = weak.update(cx, |workspace, _| workspace.shutdown());
                    true
                });
                workspace
            })?;
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
    Ok(())
}
