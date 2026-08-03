use super::*;

#[test]
fn write_mode_requires_explicit_applyable_operations() {
    assert!(EditPolicy::new(EditMode::Write).allows(EditOperation::TextEdit));
    assert!(EditPolicy::new(EditMode::Write).allows(EditOperation::Create));
    assert!(EditPolicy::new(EditMode::Write).allows(EditOperation::Rename));
    assert!(EditPolicy::new(EditMode::Write).allows(EditOperation::Delete));
    assert!(!EditPolicy::new(EditMode::Write).allows(EditOperation::Command));
    assert!(!EditPolicy::new(EditMode::Refactor).allows(EditOperation::Create));
    assert!(!EditPolicy::new(EditMode::ReadOnly).allows(EditOperation::TextEdit));
}

#[test]
fn preview_is_available_only_in_refactor_and_write_modes() {
    assert!(!EditPolicy::new(EditMode::ReadOnly).allows_preview());
    assert!(EditPolicy::new(EditMode::Refactor).allows_preview());
    assert!(EditPolicy::new(EditMode::Write).allows_preview());
}
