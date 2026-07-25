//! MCP handler context.
//!
//! This module provides the shared context for MCP tool handlers.
//! The actual tool implementations use the `#[tool]` macro from rmcp
//! and are defined in the `server` module.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bridge::{ResourceSubscriptions, Translator};
use crate::project::{ProjectHandle, ProjectRegistry, ProjectRegistryError};

/// Shared context for all tool handlers.
///
/// Holds the translator and subscription state. The MCP peer handle is not
/// stored here because resource-update notifications are sent by the pump
/// tasks in `lib.rs`, which own their own `Arc<OnceCell<Peer<RoleServer>>>`.
pub struct HandlerContext {
    /// Translator for converting MCP calls to LSP requests.
    pub translator: Arc<Mutex<Translator>>,
    /// Set of resource URIs the MCP client has subscribed to.
    pub subscriptions: Arc<ResourceSubscriptions>,
    /// Shared registry for project lifecycle operations.
    pub project_registry: ProjectRegistry,
}

impl HandlerContext {
    /// Create a new handler context.
    #[must_use]
    pub fn new(
        translator: Arc<Mutex<Translator>>,
        subscriptions: Arc<ResourceSubscriptions>,
    ) -> Self {
        Self::with_registry(translator, subscriptions, ProjectRegistry::new(32))
    }

    /// Create a handler context with an existing shared project registry.
    #[must_use]
    pub const fn with_registry(
        translator: Arc<Mutex<Translator>>,
        subscriptions: Arc<ResourceSubscriptions>,
        project_registry: ProjectRegistry,
    ) -> Self {
        Self {
            translator,
            subscriptions,
            project_registry,
        }
    }

    /// Return the actor owning a canonicalizable path, if it is registered.
    ///
    /// A missing actor is the compatibility signal used by legacy stdio
    /// callers that still rely on the daemon translator fallback.
    pub async fn actor_for_path(&self, path: impl AsRef<std::path::Path>) -> Option<ProjectHandle> {
        self.project_registry.actor_for_path(path).await.ok()
    }

    /// Return the actor owning a path or the registry's explicit routing error.
    ///
    /// Semantic tools use this path so an unregistered file cannot fall back to
    /// a process-global translator.
    pub async fn required_actor_for_path(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        self.project_registry.actor_for_path(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Translator;

    #[test]
    fn test_handler_context_creation() {
        let translator = Arc::new(Mutex::new(Translator::new()));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let context = HandlerContext::new(translator, subscriptions);
        assert_eq!(Arc::strong_count(&context.translator), 1);
    }
}
