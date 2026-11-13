use super::workspace::{LayoutNode, PaneId, SplitDirection, TabStatus};

#[test]
fn tab_status_transitions_are_idempotent() {
    let mut status = TabStatus::Running;
    assert!(status.mark_closing());
    assert_eq!(status, TabStatus::Closing);
    assert!(!status.mark_closing());
    assert!(status.mark_exited());
    assert_eq!(status, TabStatus::Exited);
    assert!(!status.mark_exited());
    assert!(!status.mark_closing());
}

#[test]
fn exited_status_reports_exited_only() {
    assert!(!TabStatus::Running.is_exited());
    assert!(!TabStatus::Closing.is_exited());
    assert!(TabStatus::Exited.is_exited());
}

fn pane(id: u64) -> PaneId {
    PaneId(id)
}

fn leaves(layout: &LayoutNode) -> Vec<PaneId> {
    let mut result = Vec::new();
    layout.leaves(&mut result);
    result
}

#[test]
fn split_and_nested_leaves_are_depth_first() {
    let mut layout = LayoutNode::Leaf(pane(1));
    assert!(layout.replace_leaf(
        pane(1),
        LayoutNode::Split {
            id: 1,
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf(pane(1))),
            second: Box::new(LayoutNode::Leaf(pane(2))),
        },
    ));
    assert!(layout.replace_leaf(
        pane(2),
        LayoutNode::Split {
            id: 2,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf(pane(2))),
            second: Box::new(LayoutNode::Leaf(pane(3))),
        },
    ));
    assert_eq!(leaves(&layout), vec![pane(1), pane(2), pane(3)]);
    assert_eq!(layout.leaf_count(), 3);
}

#[test]
fn removing_leaf_prunes_parent_and_is_idempotent() {
    let mut layout = LayoutNode::Split {
        id: 4,
        direction: SplitDirection::Horizontal,
        ratio: 0.5,
        first: Box::new(LayoutNode::Leaf(pane(1))),
        second: Box::new(LayoutNode::Split {
            id: 3,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf(pane(2))),
            second: Box::new(LayoutNode::Leaf(pane(3))),
        }),
    };
    assert!(layout.remove_leaf(pane(2)));
    assert_eq!(leaves(&layout), vec![pane(1), pane(3)]);
    assert!(layout.remove_leaf(pane(1)));
    assert_eq!(layout, LayoutNode::Leaf(pane(3)));
    assert!(!layout.remove_leaf(pane(1)));
    assert!(layout.contains(pane(3)));
    assert!(!layout.contains(pane(2)));
}

#[test]
fn root_leaf_cannot_remove_itself() {
    let mut layout = LayoutNode::Leaf(pane(9));
    assert!(!layout.remove_leaf(pane(9)));
    // Workspace 会在标签级补新窗格；树操作本身不伪造会话。
    assert_eq!(layout, LayoutNode::Leaf(pane(9)));
}

#[test]
fn split_ratio_updates_target_node_only() {
    use super::workspace::apply_split_ratio;

    let mut layout = LayoutNode::Split {
        id: 1,
        direction: SplitDirection::Horizontal,
        ratio: 0.5,
        first: Box::new(LayoutNode::Leaf(pane(1))),
        second: Box::new(LayoutNode::Split {
            id: 2,
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf(pane(2))),
            second: Box::new(LayoutNode::Leaf(pane(3))),
        }),
    };
    apply_split_ratio(&mut layout, 2, 0.3);
    let LayoutNode::Split { first, second, .. } = &layout else {
        panic!("根节点应是分割");
    };
    assert!(
        matches!(first.as_ref(), LayoutNode::Leaf(_)),
        "未命中的子树不变"
    );
    assert!(matches!(
        second.as_ref(),
        LayoutNode::Split { ratio: 0.3, .. }
    ));

    // 不存在的 id：静默跳过，树保持不变
    apply_split_ratio(&mut layout, 99, 0.9);
    let LayoutNode::Split { second, .. } = &layout else {
        panic!("根节点应是分割");
    };
    assert!(matches!(
        second.as_ref(),
        LayoutNode::Split { ratio: 0.3, .. }
    ));
}

#[test]
fn unified_shortcut_conflicts_detect_cross_kind() {
    use super::workspace::{BindingOwner, ShortcutBinding, shortcut_conflict_with};

    let bindings = vec![
        ShortcutBinding {
            owner: BindingOwner::Builtin,
            normalized: "ctrl+t".into(),
            label: "新建标签".into(),
        },
        ShortcutBinding {
            owner: BindingOwner::Command("ssh".into()),
            normalized: "ctrl+alt+1".into(),
            label: "运行命令 ssh".into(),
        },
    ];
    // 撞内置
    assert_eq!(
        shortcut_conflict_with(&bindings, "Ctrl+T", None).as_deref(),
        Some("内置快捷键")
    );
    // 撞命令
    assert_eq!(
        shortcut_conflict_with(&bindings, "ctrl+alt+1", None).as_deref(),
        Some("命令「ssh」")
    );
    // 编辑自身豁免
    assert!(
        shortcut_conflict_with(
            &bindings,
            "ctrl+alt+1",
            Some(&BindingOwner::Command("ssh".into()))
        )
        .is_none()
    );
    // 大小写与空格等价
    assert_eq!(
        shortcut_conflict_with(&bindings, " CTRL + ALT + 1 ", None).as_deref(),
        Some("命令「ssh」")
    );
    // 非法格式返回 None：格式校验由保存路径先行的 normalize 报错承担
    assert!(shortcut_conflict_with(&bindings, "ctrl", None).is_none());
}

/// 搜索模态三开关（alt+c/r/w）注册进内置注册表：all_shortcut_bindings 的
/// 内置段逐条来自 BUILTIN_SHORTCUTS（归一化失败跳过），用户把命令/服务器/
/// 动态码绑到同键时 shortcut_conflict_with 必须命中 BindingOwner::Builtin
/// ——工作区 capture 分发先于搜索拦截，注册表标红是唯一的失灵预警。
#[test]
fn search_toggle_shortcuts_are_registered_for_conflict_detection() {
    use super::workspace::{
        BUILTIN_SHORTCUTS, BindingOwner, ShortcutBinding, shortcut_conflict_with,
    };
    use laterterm_config::normalize_shortcut;

    // 三键进注册表且 label 正确（进 all_shortcut_bindings → 设置页「内置」组展示）
    for (keystroke, label) in [
        ("alt+c", "搜索：大小写"),
        ("alt+r", "搜索：正则"),
        ("alt+w", "搜索：全字"),
    ] {
        assert_eq!(
            BUILTIN_SHORTCUTS
                .iter()
                .find(|(key, _)| *key == keystroke)
                .map(|(_, label)| *label),
            Some(label),
            "{keystroke} 应注册进 BUILTIN_SHORTCUTS"
        );
    }
    // 与 all_shortcut_bindings 内置段同构地构建注册表，冲突检测命中 Builtin
    let bindings: Vec<ShortcutBinding> = BUILTIN_SHORTCUTS
        .iter()
        .filter_map(|(keystroke, label)| {
            normalize_shortcut(keystroke)
                .ok()
                .map(|normalized| ShortcutBinding {
                    owner: BindingOwner::Builtin,
                    normalized,
                    label: (*label).to_string(),
                })
        })
        .collect();
    for key in ["alt+c", "alt+r", "alt+w"] {
        assert_eq!(
            shortcut_conflict_with(&bindings, key, None).as_deref(),
            Some("内置快捷键"),
            "用户绑 {key} 到命令/服务器/动态码时必须得到内置冲突预警"
        );
    }
}

#[test]
fn settings_scroll_spy_follows_judge_line_and_pins_bottom() {
    use super::workspace::{SettingsSection, settings_scroll_spy};

    let tops = [
        (SettingsSection::Appearance, 0.0),
        (SettingsSection::Font, 300.0),
        (SettingsSection::Scrolling, 500.0),
        (SettingsSection::Shortcuts, 700.0),
        (SettingsSection::Terminal, 900.0),
        (SettingsSection::Servers, 1100.0),
    ];
    // 顶部：判定线在首分区内
    assert_eq!(
        settings_scroll_spy(&tops, 80.0, false),
        SettingsSection::Appearance
    );
    // 判定线越过第二分区起点 → 高亮跟随
    assert_eq!(
        settings_scroll_spy(&tops, 310.0, false),
        SettingsSection::Font
    );
    // 判定线正好落在分区起点上 → 该分区接管（边界含等号）
    assert_eq!(
        settings_scroll_spy(&tops, 500.0, false),
        SettingsSection::Scrolling
    );
    // 矮分区（如「服务器」）起点永远够不到判定线，滚到底必须兜底选中
    assert_eq!(
        settings_scroll_spy(&tops, 950.0, true),
        SettingsSection::Servers
    );
    // 空列表兜底默认分区
    assert_eq!(
        settings_scroll_spy(&[], 80.0, false),
        SettingsSection::Appearance
    );
}

#[test]
fn settings_sections_have_unique_labels_and_anchor_order() {
    use super::workspace::SettingsSection;

    let labels: Vec<_> = SettingsSection::ALL.iter().map(|s| s.label()).collect();
    let mut unique = labels.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(labels.len(), unique.len(), "导航标签必须互异（导航项可辨）");
    // 分区下标 = 滚动容器直接子元素下标，跳转/侦测都按它对位
    for (index, section) in SettingsSection::ALL.iter().enumerate() {
        assert_eq!(
            section.index(),
            index,
            "{section:?} 下标必须与 ALL 排布一致"
        );
    }
}

/// 主题色板纯函数：ANSI 行序固定（normal 黑→白 / bright 亮黑→亮白）且逐值
/// 拷贝——预览卡展示与真实窗格色板同源，行序串位会让预览与实际不一致。
/// 三个实例分别钉：19 色各异（行序与逐值）、gruvbox_dark（background==black、
/// cursor==foreground==bright_white 等重值不得互串）、缺省回退基准 tokyo_night
/// （主题 TOML 缺键的回退值即此，theme.rs Deserialize 从它起底）。
#[test]
fn palette_preview_orders_ansi_rows_and_copies_values() {
    use super::workspace::{PalettePreview, palette_preview};
    use laterterm_config::theme::ThemeColors;

    fn assert_rows(
        preview: &PalettePreview,
        expected_normal: [(&str, u32); 8],
        expected_bright: [(&str, u32); 8],
    ) {
        assert_eq!(preview.normal, expected_normal, "ANSI 普通行序或取值串位");
        assert_eq!(preview.bright, expected_bright, "ANSI 亮色行序或取值串位");
    }

    // 19 色各异：行序与逐值拷贝一一对位
    let distinct = ThemeColors {
        background: 0x010001,
        foreground: 0x010002,
        cursor: 0x010003,
        black: 0x010004,
        red: 0x010005,
        green: 0x010006,
        yellow: 0x010007,
        blue: 0x010008,
        magenta: 0x010009,
        cyan: 0x01000A,
        white: 0x01000B,
        bright_black: 0x01000C,
        bright_red: 0x01000D,
        bright_green: 0x01000E,
        bright_yellow: 0x01000F,
        bright_blue: 0x010010,
        bright_magenta: 0x010011,
        bright_cyan: 0x010012,
        bright_white: 0x010013,
    };
    let preview = palette_preview(&distinct);
    assert_eq!(preview.background, 0x010001);
    assert_eq!(preview.foreground, 0x010002);
    assert_eq!(preview.cursor, 0x010003);
    assert_rows(
        &preview,
        [
            ("黑", 0x010004),
            ("红", 0x010005),
            ("绿", 0x010006),
            ("黄", 0x010007),
            ("蓝", 0x010008),
            ("紫", 0x010009),
            ("青", 0x01000A),
            ("白", 0x01000B),
        ],
        [
            ("亮黑", 0x01000C),
            ("亮红", 0x01000D),
            ("亮绿", 0x01000E),
            ("亮黄", 0x01000F),
            ("亮蓝", 0x010010),
            ("亮紫", 0x010011),
            ("亮青", 0x010012),
            ("亮白", 0x010013),
        ],
    );

    // gruvbox_dark：background==black、foreground==cursor==bright_white，
    // 重值字段（如把 background 的值串到 black 位）不会被发现——全数组兜底
    let gruvbox = ThemeColors::gruvbox_dark();
    let preview = palette_preview(&gruvbox);
    assert_eq!(preview.background, 0x282828);
    assert_eq!(preview.foreground, 0xEBDBB2);
    assert_eq!(preview.cursor, 0xEBDBB2);
    assert_rows(
        &preview,
        [
            ("黑", gruvbox.black),
            ("红", gruvbox.red),
            ("绿", gruvbox.green),
            ("黄", gruvbox.yellow),
            ("蓝", gruvbox.blue),
            ("紫", gruvbox.magenta),
            ("青", gruvbox.cyan),
            ("白", gruvbox.white),
        ],
        [
            ("亮黑", gruvbox.bright_black),
            ("亮红", gruvbox.bright_red),
            ("亮绿", gruvbox.bright_green),
            ("亮黄", gruvbox.bright_yellow),
            ("亮蓝", gruvbox.bright_blue),
            ("亮紫", gruvbox.bright_magenta),
            ("亮青", gruvbox.bright_cyan),
            ("亮白", gruvbox.bright_white),
        ],
    );

    // 缺省回退基准 tokyo_night（主题缺键回退值），同样不许串位
    let fallback = ThemeColors::tokyo_night();
    let preview = palette_preview(&fallback);
    assert_eq!(preview.background, 0x1A1B26);
    assert_eq!(preview.foreground, 0xC0CAF5);
    assert_eq!(preview.cursor, 0xC0CAF5);
    assert_rows(
        &preview,
        [
            ("黑", fallback.black),
            ("红", fallback.red),
            ("绿", fallback.green),
            ("黄", fallback.yellow),
            ("蓝", fallback.blue),
            ("紫", fallback.magenta),
            ("青", fallback.cyan),
            ("白", fallback.white),
        ],
        [
            ("亮黑", fallback.bright_black),
            ("亮红", fallback.bright_red),
            ("亮绿", fallback.bright_green),
            ("亮黄", fallback.bright_yellow),
            ("亮蓝", fallback.bright_blue),
            ("亮紫", fallback.bright_magenta),
            ("亮青", fallback.bright_cyan),
            ("亮白", fallback.bright_white),
        ],
    );
}

/// window_background_appearance 三态映射（启动首帧 window_options 与运行期
/// apply_appearance 共用的唯一口径）：o≥1 → Opaque、o<1+blur → Blurred、
/// o<1 无 blur → Transparent。边界 1.0 必须落在 Opaque（差 1e-6 的浮点
/// 误差走向透明会无谓请求合成器）。
#[test]
fn window_background_appearance_three_state_mapping() {
    use super::workspace::window_background_appearance;
    use laterterm_config::Appearance;

    let opaque = Appearance {
        background_opacity: 1.0,
        ..Appearance::default()
    };
    assert!(matches!(
        window_background_appearance(&opaque),
        gpui::WindowBackgroundAppearance::Opaque
    ));
    // o=1 + blur 开：映射仍 Opaque（无透明底可模糊），提示层据此不误导
    let opaque_blur = Appearance {
        background_opacity: 1.0,
        background_blur: true,
        ..Appearance::default()
    };
    assert!(matches!(
        window_background_appearance(&opaque_blur),
        gpui::WindowBackgroundAppearance::Opaque
    ));

    let blurred = Appearance {
        background_opacity: 0.7,
        background_blur: true,
        ..Appearance::default()
    };
    assert!(matches!(
        window_background_appearance(&blurred),
        gpui::WindowBackgroundAppearance::Blurred
    ));

    let transparent = Appearance {
        background_opacity: 0.7,
        ..Appearance::default()
    };
    assert!(matches!(
        window_background_appearance(&transparent),
        gpui::WindowBackgroundAppearance::Transparent
    ));
}

/// WebDAV 合并应用链路（gpui 测试）：MockWebDav 备好远端快照，按生产
/// run_webdav_task 的两段式（后台 merge_from_remote → 主线程
/// finish_webdav_merge）驱动整条链，钉住「落盘 + 同轨应用 + 回声等值吸收 +
/// local-only 恒本地 + 主题补齐/备份」。
mod webdav_merge_chain {
    use crate::quake::QuakeInput;
    use crate::webdav_sync::{
        MockWebDav, SNAPSHOT_MAIN, WebDavAuth, build_upload_snapshot, merge_from_remote,
        themes_dir_for,
    };
    use crate::workspace::WorkspaceView;
    use gpui::TestAppContext;
    use laterterm_config::sync::ConfigSnapshot;
    use laterterm_secrets::{SecretStore, totp_account, webdav_account};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// 密钥环内存实现（laterterm-secrets MemoryStore 同款口径）。
    struct MemorySecrets(Mutex<BTreeMap<String, String>>);

    impl SecretStore for MemorySecrets {
        fn get(&self, account: &str) -> anyhow::Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }
        fn set(&self, account: &str, secret: &str) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(account.to_string(), secret.to_string());
            Ok(())
        }
        fn delete(&self, account: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(account);
            Ok(())
        }
    }

    /// 构造前注入默认 StartupSettings：直连构造会回退读真实
    /// ~/.config/laterterm/settings.toml，本机残留令默认值断言机器态假红
    /// （workspace.rs inject_default_startup_settings 先例同款）。
    fn inject_default_startup_settings(cx: &mut TestAppContext) {
        cx.set_global(crate::StartupSettings(laterterm_config::Settings::default()));
    }

    /// 临时目录（进程号隔离；断言完成后整体清理）。
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "laterterm-workspace-webdav-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 本机 A 的 settings.toml：local-only 四类取本机值，quake 本机开启
    /// （合并后远程将关闭它，验证 quake 联动）。注意 TOML 语法：顶层键
    /// （shell）必须写在任何 [表] 头之前。
    const LOCAL_SETTINGS: &str = "shell = \"/usr/bin/fish\"\n\n[appearance]\nfont_size = 13.0\ndownloads_dir = \"/home/a/Downloads\"\n\n[sync]\nurl = \"https://dav-a.example.com/dav\"\nusername = \"alice\"\n\n[quake]\nenabled = true\nauto_hide_on_blur = true\n";
    const LOCAL_SOLARIZED: &str = "[colors]\nbackground = \"#002B36\"\n";
    const LOCAL_ONLY_THEME: &str = "[colors]\nbackground = \"#111111\"\n";
    const REMOTE_NEW_THEME: &str = "[colors]\nbackground = \"#ABCDEF\"\n";
    const REMOTE_SOLARIZED: &str = "[colors]\nbackground = \"#654321\"\n";

    /// 合并全链路主测试：远端快照（字号冲突远程胜 + restore_session 远端新增
    /// + quake 远程关闭 + [sync]/shell/downloads_dir 与本机不同）+ 远端主题
    /// （一个新增、一个同名覆盖）。
    #[gpui::test]
    fn webdav_merge_applies_chain_and_absorbs_echo(cx: &mut TestAppContext) {
        let dir = temp_dir("chain");
        let settings_path = dir.join("settings.toml");
        let themes_dir = themes_dir_for(&settings_path);
        std::fs::create_dir_all(&themes_dir).unwrap();
        std::fs::write(&settings_path, LOCAL_SETTINGS).unwrap();
        std::fs::write(themes_dir.join("solarized.toml"), LOCAL_SOLARIZED).unwrap();
        std::fs::write(themes_dir.join("local-only.toml"), LOCAL_ONLY_THEME).unwrap();

        // 密钥环样例密钥（WebDAV 密码 + TOTP 种子）：只进密钥环，此后任何
        // 快照/落盘产物都不得出现
        let secrets = Arc::new(MemorySecrets(Mutex::new(BTreeMap::from([
            (webdav_account().unwrap(), "webdav密码SECRET42".to_string()),
            (
                totp_account("跳板机").unwrap(),
                "JBSWY3DPEHPK3PXP".to_string(),
            ),
        ]))));

        inject_default_startup_settings(cx);
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        // 合并前本机内存态：落盘路径指向临时目录、开着主题预览、quake 已启用
        // （内存态直改 + 状态机 SetEnabled，不 spawn portal worker）
        workspace.update_in(cx, |view, _window, _cx| {
            view.test_set_settings_save_path(settings_path.clone());
            view.test_set_secret_store(secrets.clone());
            view.test_set_theme_preview(Some("solarized".into()));
            view.test_settings_mut().quake.enabled = true;
            view.test_quake_machine_mut()
                .handle(QuakeInput::SetEnabled(true));
        });
        assert!(
            workspace.read_with(cx, |view, _| view.test_quake_machine().is_enabled()),
            "前置：quake 状态机应已启用"
        );
        let version_before = workspace.read_with(cx, |view, _| view.test_settings_version());

        // 后台段：GET 快照 → 双备份 → 合并 → 主题补齐（MockWebDav 无网络）
        let snapshot = ConfigSnapshot::build(
            "restore_session = true\n\n[appearance]\nfont_size = 15.5\n\n[quake]\nenabled = false\nauto_hide_on_blur = true\n\n[sync]\nurl = \"https://dav-b.example.com/dav\"\nusername = \"bob\"\n".into(),
            vec![
                ("new-theme".into(), REMOTE_NEW_THEME.into()),
                ("solarized".into(), REMOTE_SOLARIZED.into()),
            ],
            "0.3.0",
            1_700_000_000,
        );
        let dav = MockWebDav::new().with_file(SNAPSHOT_MAIN, &snapshot.to_toml_string().unwrap());
        let auth = WebDavAuth {
            username: "alice".into(),
            password: secrets.get(&webdav_account().unwrap()).unwrap().unwrap(),
        };
        let (merged, report) =
            merge_from_remote(&dav, &auth, &settings_path, &themes_dir, "20231114T221320Z")
                .unwrap();

        // 主线程段：合并收尾（先落盘再同轨应用）。用 update（非窗口更新）
        // 对齐生产 this.update 回写上下文——apply_appearance 走 None 分支
        // （handle.update），与后台任务回写路径一致。
        workspace.update(cx, |view, cx| {
            view.finish_webdav_merge(merged, report, cx);
        });

        workspace.read_with(cx, |view, _| {
            // 1. settings_version 推进（persist + apply 各一次）
            assert!(
                view.test_settings_version() > version_before,
                "合并后 settings_version 必须推进"
            );
            // 2a. 合并设置已进内存：远程胜 + 远端新增
            assert_eq!(view.test_settings().appearance.font_size, 15.5);
            assert!(view.test_settings().restore_session);
            // 2b. local-only 恒本地（[sync]/shell/downloads_dir 与远端不同仍取本机）
            assert_eq!(
                view.test_settings().sync.url,
                "https://dav-a.example.com/dav"
            );
            assert_eq!(view.test_settings().shell.as_deref(), Some("/usr/bin/fish"));
            assert_eq!(
                view.test_settings().appearance.downloads_dir,
                "/home/a/Downloads"
            );
            // 3. 开着主题预览点合并 → 预览作废
            assert_eq!(view.test_theme_preview(), None, "合并后旧主题预览必须作废");
            // 4. quake 开关变更触发 sync_quake_worker：远程关闭 → 状态机停用
            //（非 Linux 的 sync_quake_worker 为空实现，断言仅在 Linux 有义）
            #[cfg(target_os = "linux")]
            assert!(
                !view.test_quake_machine().is_enabled(),
                "远程关闭 quake 必须联动状态机停用"
            );
        });

        // 5. settings 与 themes 落盘均在 tempdir（注入路径，绝不碰真实配置）
        let saved = std::fs::read_to_string(&settings_path).unwrap();
        assert!(saved.contains("font_size = 15.5"), "{saved}");
        assert!(saved.contains("dav-a.example.com"), "{saved}");
        assert!(saved.contains("/usr/bin/fish"), "{saved}");
        assert!(saved.contains("/home/a/Downloads"), "{saved}");
        assert!(saved.contains("restore_session = true"), "{saved}");
        assert_eq!(
            std::fs::read_to_string(themes_dir.join("new-theme.toml")).unwrap(),
            REMOTE_NEW_THEME,
            "远端新增主题已落盘"
        );
        assert_eq!(
            std::fs::read_to_string(themes_dir.join("solarized.toml")).unwrap(),
            REMOTE_SOLARIZED,
            "同名不同内容主题远程胜覆盖"
        );
        assert_eq!(
            std::fs::read_to_string(themes_dir.join("local-only.toml")).unwrap(),
            LOCAL_ONLY_THEME,
            "本地独有主题一律不动"
        );
        // 双备份：settings.toml.bak-<ts> 与 themes.bak-<ts>/ 均为合并前内容
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.toml.bak-20231114T221320Z")).unwrap(),
            LOCAL_SETTINGS
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("themes.bak-20231114T221320Z/solarized.toml"))
                .unwrap(),
            LOCAL_SOLARIZED
        );

        // 6. watcher 回声等值吸收：合并落盘触发的 reload 重读同一份内容，
        // 等值比较吸收，不得再推进版本（TOML roundtrip 无损的回归钉）
        let version_after_merge = workspace.read_with(cx, |view, _| view.test_settings_version());
        workspace.update(cx, |view, cx| view.test_reload_from_disk(cx));
        workspace.read_with(cx, |view, _| {
            assert_eq!(
                view.test_settings_version(),
                version_after_merge,
                "回声 reload 必须被等值比较吸收"
            );
            assert_eq!(view.test_theme_preview(), None);
        });

        // 7. 密钥环样例密钥不出现在快照与落盘产物
        let snapshot_body = build_upload_snapshot(&settings_path, &themes_dir).unwrap();
        for secret in ["webdav密码SECRET42", "JBSWY3DPEHPK3PXP"] {
            assert!(!snapshot_body.contains(secret), "快照泄漏样例密钥 {secret}");
            assert!(
                !saved.contains(secret),
                "落盘 settings 泄漏样例密钥 {secret}"
            );
        }
        for entry in std::fs::read_dir(&themes_dir).unwrap().flatten() {
            let body = std::fs::read_to_string(entry.path()).unwrap();
            for secret in ["webdav密码SECRET42", "JBSWY3DPEHPK3PXP"] {
                assert!(!body.contains(secret), "主题落盘产物泄漏样例密钥 {secret}");
            }
        }

        // 会话泄漏会让测试进程收尾告警（workspace.rs 测试同款口径）
        workspace.read_with(cx, |view, _| view.test_close_sessions());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 合并报错路径不触碰工作区状态：401 时 finish_webdav_merge 不被调用
    /// （run_webdav_task 的 match 只在 Ok(MergeApplied) 才应用），这里直接
    /// 钉 merge_from_remote 的 401 文案与工作区零副作用。
    #[gpui::test]
    fn webdav_merge_error_leaves_workspace_untouched(cx: &mut TestAppContext) {
        let dir = temp_dir("chain-error");
        let settings_path = dir.join("settings.toml");
        let themes_dir = themes_dir_for(&settings_path);
        std::fs::write(&settings_path, LOCAL_SETTINGS).unwrap();

        inject_default_startup_settings(cx);
        let (workspace, cx) = cx.add_window_view(WorkspaceView::new);
        workspace.update_in(cx, |view, _window, _cx| {
            view.test_set_settings_save_path(settings_path.clone());
        });
        let version_before = workspace.read_with(cx, |view, _| view.test_settings_version());

        let dav = MockWebDav::new().with_error(
            &format!("GET {SNAPSHOT_MAIN}"),
            crate::webdav_sync::WebDavError::Auth,
        );
        let error = merge_from_remote(
            &dav,
            &WebDavAuth {
                username: "alice".into(),
                password: "wrong".into(),
            },
            &settings_path,
            &themes_dir,
            "20231114T221320Z",
        )
        .unwrap_err();
        assert_eq!(error, "认证失败（账号或密码错误）");
        assert!(!error.contains("wrong"), "错误文案不得携带密码");

        // 错误路径无合并载荷可应用：内存设置与版本均未动（调用方只回写
        // webdav_result，见 run_webdav_task 的 match 分支）
        workspace.read_with(cx, |view, _| {
            assert_eq!(view.test_settings().appearance.font_size, 13.0);
            assert_eq!(view.test_settings_version(), version_before);
        });

        workspace.read_with(cx, |view, _| view.test_close_sessions());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
