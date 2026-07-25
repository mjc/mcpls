//! MCP handler context.
//!
//! This module provides the shared context for MCP tool handlers.
//! The actual tool implementations use the `#[tool]` macro from rmcp
//! and are defined in the `server` module.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use crate::bridge::{ResourceSubscriptions, Translator};
use crate::edit_plan::PlanId;
use crate::mcp::session::SessionEventSink;
use crate::project::{ProjectHandle, ProjectRegistry, ProjectRegistryError};
use crate::transport::{self, SessionManagerHandle, TransportSnapshot};

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
    /// Safe transport details shared by all session snapshots.
    pub(crate) transport: Arc<TransportSnapshot>,
    /// Shared HTTP session store used for non-blocking status counts.
    pub(crate) session_manager: SessionManagerHandle,
    /// Monotonic daemon start point shared by session clones.
    pub(crate) started_at: Instant,
    /// Edit plans previewed by this MCP session.
    owned_plan_ids: Mutex<HashSet<PlanId>>,
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
        Self::with_subscriptions(
            subscriptions,
            project_registry,
            Arc::new(TransportSnapshot::stdio()),
            transport::no_session_manager(),
        )
    }

    /// Create a handler context with explicit daemon transport metadata.
    #[must_use]
    pub(crate) fn from_registry_with_transport(
        subscriptions: Arc<ResourceSubscriptions>,
        project_registry: ProjectRegistry,
        transport: TransportSnapshot,
        session_manager: SessionManagerHandle,
    ) -> Self {
        Self::with_subscriptions(
            subscriptions,
            project_registry,
            Arc::new(transport),
            session_manager,
        )
    }

    fn with_subscriptions(
        subscriptions: Arc<ResourceSubscriptions>,
        project_registry: ProjectRegistry,
        transport: Arc<TransportSnapshot>,
        session_manager: SessionManagerHandle,
    ) -> Self {
        let event_sink = Arc::new(SessionEventSink::new(Arc::clone(&subscriptions)));
        Self {
            subscriptions,
            project_registry,
            event_sink,
            transport,
            session_manager,
            started_at: Instant::now(),
            owned_plan_ids: Mutex::new(HashSet::new()),
        }
    }

    /// Remember a plan returned by a preview in this session.
    pub(crate) async fn remember_plan(&self, plan_id: PlanId) {
        self.owned_plan_ids.lock().await.insert(plan_id);
    }

    /// Claim a plan for application, consuming this session's ownership token.
    pub(crate) async fn claim_plan(&self, plan_id: &PlanId) -> bool {
        self.owned_plan_ids.lock().await.remove(plan_id)
    }

    /// Create a session-local context that shares project actors but not
    /// resource subscriptions with another MCP session.
    #[must_use]
    pub fn for_session(&self) -> Self {
        let mut context = Self::with_subscriptions(
            Arc::new(ResourceSubscriptions::new()),
            self.project_registry.clone(),
            Arc::clone(&self.transport),
            self.session_manager.clone(),
        );
        context.started_at = self.started_at;
        context
    }

    /// Return the number of active HTTP sessions without touching project actors.
    pub(crate) async fn session_count(&self) -> usize {
        transport::session_count(&self.session_manager).await
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
    use crate::edit_plan::PlanId;

    #[test]
    fn test_handler_context_creation() {
        let translator = Arc::new(Mutex::new(Translator::new()));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let context = HandlerContext::new(translator, subscriptions);
        assert_eq!(Arc::strong_count(&context.subscriptions), 2);
    }

    #[tokio::test]
    async fn edit_plan_ownership_is_local_to_one_session() {
        let translator = Arc::new(Mutex::new(Translator::new()));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let context = HandlerContext::new(translator, subscriptions);
        let session = context.for_session();
        let plan_id = PlanId::new();

        context.remember_plan(plan_id.clone()).await;

        assert!(context.claim_plan(&plan_id).await);
        assert!(!context.claim_plan(&plan_id).await);
        assert!(!session.claim_plan(&plan_id).await);
    }
}
