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
            direction: SplitDirection::Horizontal,
            first: Box::new(LayoutNode::Leaf(pane(1))),
            second: Box::new(LayoutNode::Leaf(pane(2))),
        },
    ));
    assert!(layout.replace_leaf(
        pane(2),
        LayoutNode::Split {
            direction: SplitDirection::Vertical,
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
        direction: SplitDirection::Horizontal,
        first: Box::new(LayoutNode::Leaf(pane(1))),
        second: Box::new(LayoutNode::Split {
            direction: SplitDirection::Vertical,
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
