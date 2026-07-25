//! MCP handler context.
//!
//! This module provides the shared context for MCP tool handlers.
//! The actual tool implementations use the `#[tool]` macro from rmcp
//! and are defined in the `server` module.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use crate::bridge::{ResourceSubscriptions, Translator};
use crate::mcp::session::SessionEventSink;
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
    /// Per-session event forwarding tasks.
    pub(crate) event_sink: Arc<SessionEventSink>,
    /// Monotonic daemon start point shared by session clones.
    pub(crate) started_at: Instant,
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
        Self::from_registry(subscriptions, project_registry)
    }

    /// Create a handler context from the shared project registry and
    /// session-owned subscriptions.
    #[must_use]
    pub fn from_registry(
        subscriptions: Arc<ResourceSubscriptions>,
        project_registry: ProjectRegistry,
    ) -> Self {
        Self::with_subscriptions(subscriptions, project_registry)
    }

    fn with_subscriptions(
        subscriptions: Arc<ResourceSubscriptions>,
        project_registry: ProjectRegistry,
    ) -> Self {
        let event_sink = Arc::new(SessionEventSink::new(Arc::clone(&subscriptions)));
        Self {
            subscriptions,
            project_registry,
            event_sink,
            started_at: Instant::now(),
        }
    }

    /// Create a session-local context that shares project actors but not
    /// resource subscriptions with another MCP session.
    #[must_use]
    pub fn for_session(&self) -> Self {
        let mut context = Self::from_registry(
            Arc::new(ResourceSubscriptions::new()),
            self.project_registry.clone(),
        );
        context.started_at = self.started_at;
        context
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

    /// Return the owning project identity and actor for a path.
    pub async fn required_project_for_path(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<(crate::project::ProjectId, ProjectHandle), ProjectRegistryError> {
        self.project_registry.project_for_path(path).await
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
        assert_eq!(Arc::strong_count(&context.subscriptions), 2);
    }
}
