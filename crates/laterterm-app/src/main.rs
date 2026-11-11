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
        // 创建期即按设置落窗口背景三态（映射与运行期共用
        // workspace::window_background_appearance，唯一口径）：透明配置下
        // 首帧直接以透明合成，避免「先不透明、apply_appearance 再切换」
        // 的首帧闪变。
        window_background: workspace::window_background_appearance(&settings.appearance),
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
    // 非 Linux 平台不读 quake 设置，cx 仅为签名统一（settings 已被
    // window_background 全平台消费）。
    #[cfg(not(target_os = "linux"))]
    let _ = cx;
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
    let app = gpui::Application::new().with_assets(branding::Assets);
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
                // 原生窗口关闭（X11 WM_DELETE_WINDOW / Wayland Close / Alt+F4）：
                // 经工作区关闭确认门卫——探测到运行中终端时返回 false 挂起窗口
                // 销毁（弹窗确认回调里再 shutdown+quit），无需确认时 shutdown 后
                // 放行（true）。实体已亡的极端情形按放行处理。must 同步返回：
                // 异步探测会让空闲窗口的原生关闭出现闪延迟（见 vendor/gpui
                // window.rs on_window_should_close 注释）。
                window.on_window_should_close(cx, move |_, cx| {
                    weak.update(cx, |workspace, cx| workspace.request_window_close(cx))
                        .unwrap_or(true)
                });
                workspace
            })?;
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
    Ok(())
}
