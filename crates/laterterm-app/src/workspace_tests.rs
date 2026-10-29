use super::workspace::{LayoutNode, SplitDirection, TabId, TabStatus};

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

fn leaves(layout: &LayoutNode) -> Vec<TabId> {
    let mut result = Vec::new();
    layout.leaves(&mut result);
    result
}

#[test]
fn split_and_nested_leaves_are_depth_first() {
    let mut layout = LayoutNode::Leaf(TabId(1));
    assert!(layout.replace_leaf(
        TabId(1),
        LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            first: Box::new(LayoutNode::Leaf(TabId(1))),
            second: Box::new(LayoutNode::Leaf(TabId(2))),
        },
    ));
    assert!(layout.replace_leaf(
        TabId(2),
        LayoutNode::Split {
            direction: SplitDirection::Vertical,
            first: Box::new(LayoutNode::Leaf(TabId(2))),
            second: Box::new(LayoutNode::Leaf(TabId(3))),
        },
    ));
    assert_eq!(leaves(&layout), vec![TabId(1), TabId(2), TabId(3)]);
}

#[test]
fn removing_leaf_prunes_parent_and_is_idempotent() {
    let mut layout = LayoutNode::Split {
        direction: SplitDirection::Horizontal,
        first: Box::new(LayoutNode::Leaf(TabId(1))),
        second: Box::new(LayoutNode::Split {
            direction: SplitDirection::Vertical,
            first: Box::new(LayoutNode::Leaf(TabId(2))),
            second: Box::new(LayoutNode::Leaf(TabId(3))),
        }),
    };
    assert!(layout.remove_leaf(TabId(2)));
    assert_eq!(leaves(&layout), vec![TabId(1), TabId(3)]);
    assert!(layout.remove_leaf(TabId(1)));
    assert_eq!(layout, LayoutNode::Leaf(TabId(3)));
    assert!(!layout.remove_leaf(TabId(1)));
    assert!(layout.contains(TabId(3)));
    assert!(!layout.contains(TabId(2)));
}

#[test]
fn removing_last_leaf_reports_empty_operation_without_corrupting_tree() {
    let mut layout = LayoutNode::Leaf(TabId(9));
    assert!(!layout.remove_leaf(TabId(9)));
    // Workspace 会在这个状态下补一个新叶子；树操作本身不伪造会话。
    assert_eq!(layout, LayoutNode::Leaf(TabId(9)));
}
