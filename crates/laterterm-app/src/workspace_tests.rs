use super::workspace::TabStatus;

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
