//! LaterTerm — 跨平台 GPU 终端。

mod audit;
mod branding;
mod command_text_field;
mod flow;
mod import;
#[cfg(target_os = "macos")]
mod macos_permissions;
mod onboarding;
mod pane_navigation;
mod paste_text_area;
mod quake;
mod server_connection;
mod sftp_panel;
mod shortcut_recorder;
mod ssh_command;
mod update_check;
mod webdav_sync;
mod workspace;
#[cfg(test)]
mod workspace_tests;

use anyhow::Result;
use gpui::AppContext;
use laterterm_config::Settings;
use std::path::PathBuf;
use workspace::WorkspaceView;

/// 启动期共享设置：main 只读一次 settings.toml，窗口几何判断与
/// WorkspaceView 共用，消掉启动路径的双读双解析。WorkspaceView::new
/// 取走（move）；测试等直连构造路径无此全局，自行回退加载。
struct StartupSettings(Settings);

impl gpui::Global for StartupSettings {}

/// 外部指定的启动目录（如访达里的「在此处打开终端」）：main 从 argv/环境变量
/// 解析一次，WorkspaceView::new 首窗格取走（move）。没挂这个全局 = 默认主目录。
struct StartupDir(Option<PathBuf>);

impl gpui::Global for StartupDir {}

/// 解析外部指定的启动目录。两处来源（优先级从高到低）：
/// - argv：`--working-directory <dir>` / `--working-directory=<dir>`，或第一个
///   真实存在的目录位置参数（`open -na LaterTerm --args <dir>` 传入的就是它）；
/// - 环境变量 `LATERTERM_OPEN_DIR`。
///
/// 目标必须是存在的目录，否则忽略——照常默认启动，不因参数坏掉拒启。
fn startup_dir_from_launch(
    args: &[std::ffi::OsString],
    env_dir: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let mut rest = args.iter().skip(1); // argv[0] 是程序自身路径
    let mut specified: Option<std::ffi::OsString> = None;
    while let Some(arg) = rest.next() {
        let text = arg.to_string_lossy();
        if text == "--working-directory" {
            specified = rest.next().cloned();
            break;
        }
        if let Some(value) = text.strip_prefix("--working-directory=") {
            specified = Some(std::ffi::OsString::from(value));
            break;
        }
        if !text.starts_with('-') && std::path::Path::new(arg).is_dir() {
            specified = Some(arg.clone());
            break;
        }
    }
    let dir = std::path::PathBuf::from(specified.or(env_dir.map(Into::into))?);
    let dir = dir.canonicalize().ok()?;
    dir.is_dir().then_some(dir)
}

fn window_options(cx: &mut gpui::App, settings: &Settings) -> gpui::WindowOptions {
    // 仅 Linux 的 Quake 几何需要可变；macOS 下方会另取阴影绑定。
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
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
        // 外部指定的启动目录：有就挂成全局，WorkspaceView::new 首窗格取走
        if let Some(dir) = startup_dir_from_launch(
            &std::env::args_os().collect::<Vec<_>>(),
            std::env::var_os("LATERTERM_OPEN_DIR").as_deref(),
        ) {
            cx.set_global(StartupDir(Some(dir)));
        }
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

#[cfg(test)]
mod startup_dir_tests {
    use super::*;
    use std::ffi::OsString;

    /// 测试专用临时目录（进程级唯一，结束清理）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("laterterm-startdir-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        let mut out = vec![OsString::from(
            "/Applications/LaterTerm.app/Contents/MacOS/laterterm",
        )];
        out.extend(list.iter().map(OsString::from));
        out
    }

    #[test]
    fn no_source_gives_none() {
        assert_eq!(startup_dir_from_launch(&args(&[]), None), None);
        assert_eq!(startup_dir_from_launch(&args(&[]), Some("".as_ref())), None);
    }

    #[test]
    fn positional_dir_wins_when_it_exists() {
        let dir = temp_dir("positional");
        // 位置参数必须是真实存在的目录：不存在的路径不当启动目录
        assert_eq!(
            startup_dir_from_launch(&args(&["/no/such/dir", &dir.to_string_lossy()]), None)
                .unwrap(),
            dir.canonicalize().unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn working_directory_flag_both_forms() {
        let dir = temp_dir("flag");
        let path = dir.to_string_lossy().into_owned();
        assert_eq!(
            startup_dir_from_launch(&args(&["--working-directory", &path]), None).unwrap(),
            dir.canonicalize().unwrap()
        );
        assert_eq!(
            startup_dir_from_launch(&args(&[&format!("--working-directory={path}")]), None)
                .unwrap(),
            dir.canonicalize().unwrap()
        );
        // 目录不存在：忽略而不是拒启
        assert_eq!(
            startup_dir_from_launch(&args(&["--working-directory", "/no/such/dir"]), None),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_is_fallback_and_argv_wins() {
        let dir = temp_dir("env");
        let path = dir.to_string_lossy().into_owned();
        // 无 argv → 环境变量兜底
        assert_eq!(
            startup_dir_from_launch(&args(&[]), Some(path.as_ref())).unwrap(),
            dir.canonicalize().unwrap()
        );
        // argv 优先于环境变量
        assert_eq!(
            startup_dir_from_launch(
                &args(&["--working-directory", &path]),
                Some("/no/such/dir".as_ref())
            )
            .unwrap(),
            dir.canonicalize().unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
