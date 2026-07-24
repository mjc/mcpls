//! Executable policy primitives for the `WorkspaceEdit` safety contract.

/// Permission mode for `WorkspaceEdit` operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EditMode {
    /// Permit read-only queries only.
    ReadOnly,
    /// Permit read-only queries and previews.
    Refactor,
    /// Permit previews and explicitly preconditioned file writes.
    Write,
}

/// Operation categories understood by the safety policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EditOperation {
    /// A text edit to an existing file.
    TextEdit,
    /// Create a new file.
    Create,
    /// Rename a file.
    Rename,
    /// Delete a file.
    Delete,
    /// A command-only code action.
    Command,
}

/// Small, fail-closed policy used by preview/apply planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditPolicy {
    mode: EditMode,
}

impl EditPolicy {
    /// Construct a policy for one operating mode.
    #[must_use]
    pub const fn new(mode: EditMode) -> Self {
        Self { mode }
    }

    /// Return whether an operation may be applied under this policy.
    #[must_use]
    pub const fn allows(self, operation: EditOperation) -> bool {
        matches!(self.mode, EditMode::Write) && !matches!(operation, EditOperation::Command)
    }

    /// Return whether this policy permits generating a preview.
    #[must_use]
    pub const fn allows_preview(self) -> bool {
        !matches!(self.mode, EditMode::ReadOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_mode_requires_explicit_applyable_operations() {
        assert!(EditPolicy::new(EditMode::Write).allows(EditOperation::TextEdit));
        assert!(EditPolicy::new(EditMode::Write).allows(EditOperation::Create));
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
}
