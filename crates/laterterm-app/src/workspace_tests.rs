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
