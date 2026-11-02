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
    assert!(matches!(first.as_ref(), LayoutNode::Leaf(_)), "未命中的子树不变");
    assert!(matches!(second.as_ref(), LayoutNode::Split { ratio: 0.3, .. }));

    // 不存在的 id：静默跳过，树保持不变
    apply_split_ratio(&mut layout, 99, 0.9);
    let LayoutNode::Split { second, .. } = &layout else {
        panic!("根节点应是分割");
    };
    assert!(matches!(second.as_ref(), LayoutNode::Split { ratio: 0.3, .. }));
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
    assert!(shortcut_conflict_with(
        &bindings,
        "ctrl+alt+1",
        Some(&BindingOwner::Command("ssh".into()))
    )
    .is_none());
    // 大小写与空格等价
    assert_eq!(
        shortcut_conflict_with(&bindings, " CTRL + ALT + 1 ", None).as_deref(),
        Some("命令「ssh」")
    );
    // 非法格式返回 None：格式校验由保存路径先行的 normalize 报错承担
    assert!(shortcut_conflict_with(&bindings, "ctrl", None).is_none());
}
