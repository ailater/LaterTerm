//! LaterTerm — 跨平台 GPU 终端。

mod branding;
mod command_text_field;
mod pane_navigation;
mod server_connection;
mod shortcut_recorder;
mod workspace;
#[cfg(test)]
mod workspace_tests;

use anyhow::Result;
use gpui::AppContext;
use workspace::WorkspaceView;

fn window_options() -> gpui::WindowOptions {
    let options = gpui::WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("LaterTerm".into()),
            appears_transparent: true,
            ..Default::default()
        }),
        app_id: Some(branding::APP_ID.into()),
        window_decorations: Some(gpui::WindowDecorations::Client),
        ..Default::default()
    };
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
        cx.spawn(async move |cx| {
            cx.open_window(window_options(), |window, cx| {
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
