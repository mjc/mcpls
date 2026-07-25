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
/// Holds project routing and subscription state. Language-server translators
/// live inside project actors; the MCP peer handle is not stored here because
/// resource-update notifications are sent by the transport layer.
pub struct HandlerContext {
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
    pub fn with_registry(
        _translator: Arc<Mutex<Translator>>,
        subscriptions: Arc<ResourceSubscriptions>,
        project_registry: ProjectRegistry,
    ) -> Self {
        Self {
            subscriptions,
            project_registry,
        }
    }

    /// Create a session-local context that shares project actors but not
    /// resource subscriptions with another MCP session.
    #[must_use]
    pub fn for_session(&self) -> Self {
        Self {
            subscriptions: Arc::new(ResourceSubscriptions::new()),
            project_registry: self.project_registry.clone(),
        }
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
        assert_eq!(Arc::strong_count(&context.subscriptions), 1);
    }
}
