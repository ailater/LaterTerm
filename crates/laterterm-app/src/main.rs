//! LaterTerm — 跨平台 GPU 终端。

mod command_text_field;
mod workspace;
#[cfg(test)]
mod workspace_tests;

use anyhow::Result;
use gpui::AppContext;
use workspace::WorkspaceView;

fn main() -> Result<()> {
    let app = gpui::Application::new();
    app.run(|cx| {
        cx.spawn(async move |cx| {
            cx.open_window(
                gpui::WindowOptions {
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("LaterTerm".into()),
                        ..Default::default()
                    }),
                    window_decorations: Some(gpui::WindowDecorations::Client),
                    ..Default::default()
                },
                |window, cx| {
                    let workspace = cx.new(|cx| WorkspaceView::new(window, cx));
                    workspace.update(cx, |workspace, cx| workspace.focus_active(window, cx));
                    workspace
                },
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
    Ok(())
}
