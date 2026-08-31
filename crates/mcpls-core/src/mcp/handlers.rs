//! MCP handler context.
//!
//! This module provides the shared context for MCP tool handlers.
//! The actual tool implementations use the `#[tool]` macro from rmcp
//! and are defined in the `server` module.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use rmcp::model::{ClientCapabilities, RequestStateCodec, SealOptions};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::bridge::ResourceSubscriptions;
use crate::edit_plan::PlanId;
use crate::mcp::session::SessionEventSink;
use crate::project::{ProjectHandle, ProjectRegistry, ProjectRegistryError};
use crate::transport::{self, SessionManagerHandle, TransportSnapshot};

pub(super) const APPROVAL_INPUT_ID: &str = "approval";
pub(super) const APPROVAL_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_CLAIMED_PLAN_IDS: usize = 256;

#[derive(Debug, Default)]
struct SessionEditPlans {
    owned: HashSet<PlanId>,
    active: HashSet<PlanId>,
    terminal: VecDeque<PlanId>,
}

impl SessionEditPlans {
    fn remember(&mut self, plan_id: PlanId) {
        self.active.remove(&plan_id);
        self.terminal.retain(|claimed| claimed != &plan_id);
        self.owned.insert(plan_id);
    }

    fn claim(&mut self, plan_id: &PlanId) -> bool {
        if !self.owned.remove(plan_id) {
            return self.active.contains(plan_id) || self.terminal.contains(plan_id);
        }
        self.active.insert(plan_id.clone());
        true
    }

    fn finish(&mut self, plan_id: &PlanId) {
        if !self.active.remove(plan_id) {
            return;
        }
        if self.terminal.len() >= MAX_CLAIMED_PLAN_IDS {
            self.terminal.pop_front();
        }
        self.terminal.push_back(plan_id.clone());
    }
}

/// Authenticated, non-source state carried through one approval round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct MutationApprovalState {
    pub(super) session_id: String,
    pub(super) principal: Option<String>,
    pub(super) method: String,
    pub(super) tool_name: String,
    pub(super) project_id: String,
    pub(super) plan_id: String,
    pub(super) arguments_digest: String,
    pub(super) snapshot_hashes: Vec<String>,
    pub(super) versions: Vec<Option<i32>>,
    pub(super) nonce: String,
}

/// Shared context for all tool handlers.
///
/// Holds project routing and subscription state. Language-server translators
/// live inside project actors; the MCP peer handle is not stored here because
/// resource-update notifications are sent by the transport layer.
pub struct HandlerContext {
    /// Stable identifier for this MCP session, used in mutation audit records.
    session_id: String,
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
    /// Previewed and claimed edit plans owned by this MCP session.
    edit_plans: Mutex<SessionEditPlans>,
    /// Seals and authenticates MRTR approval state for this session.
    approval_codec: RequestStateCodec,
    /// Accepted approval nonces awaiting one retry; consumed atomically.
    pending_approvals: Mutex<HashSet<String>>,
    /// Client capability captured at the MCP request boundary.
    client_supports_form_elicitation: RwLock<Option<bool>>,
    /// Whether the negotiated protocol can carry MRTR input-required results.
    client_supports_mrtr: RwLock<Option<bool>>,
}

impl HandlerContext {
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
        let mut approval_key = Vec::with_capacity(32);
        approval_key.extend_from_slice(Uuid::new_v4().as_bytes());
        approval_key.extend_from_slice(Uuid::new_v4().as_bytes());
        Self {
            session_id: Uuid::new_v4().to_string(),
            subscriptions,
            project_registry,
            event_sink,
            transport,
            session_manager,
            started_at: Instant::now(),
            edit_plans: Mutex::new(SessionEditPlans::default()),
            approval_codec: RequestStateCodec::new(approval_key),
            pending_approvals: Mutex::new(HashSet::new()),
            client_supports_form_elicitation: RwLock::new(if cfg!(test) {
                Some(true)
            } else {
                None
            }),
            client_supports_mrtr: RwLock::new(if cfg!(test) { Some(true) } else { None }),
        }
    }

    /// Return this MCP session's stable audit identifier.
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Remember a plan returned by a preview in this session.
    pub(crate) async fn remember_plan(&self, plan_id: PlanId) {
        self.edit_plans.lock().await.remember(plan_id);
    }

    /// Claim a plan for application, consuming this session's ownership token.
    pub(crate) async fn claim_plan(&self, plan_id: &PlanId) -> bool {
        self.edit_plans.lock().await.claim(plan_id)
    }

    /// Mark a claimed plan terminal after apply or conflict. Busy plans stay
    /// active so a retry cannot be evicted while another plan is running.
    pub(crate) async fn finish_plan(&self, plan_id: &PlanId) {
        self.edit_plans.lock().await.finish(plan_id);
    }

    /// Return whether this session owns a plan without consuming its token.
    pub(crate) async fn owns_plan(&self, plan_id: &PlanId) -> bool {
        self.edit_plans.lock().await.owned.contains(plan_id)
    }

    /// Return whether this session still recognizes a previewed plan.
    pub(crate) async fn recognizes_plan(&self, plan_id: &PlanId) -> bool {
        let plans = self.edit_plans.lock().await;
        plans.owned.contains(plan_id)
            || plans.active.contains(plan_id)
            || plans.terminal.contains(plan_id)
    }

    /// Capture whether the connected client can answer form elicitation.
    pub(crate) fn set_client_capabilities(
        &self,
        capabilities: Option<ClientCapabilities>,
        supports_mrtr: bool,
    ) {
        let supports_form = capabilities.and_then(|capabilities| {
            capabilities.elicitation.map(|elicitation| {
                elicitation.form.is_some()
                    || (elicitation.form.is_none() && elicitation.url.is_none())
            })
        });
        *self
            .client_supports_form_elicitation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = supports_form;
        *self
            .client_supports_mrtr
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(supports_mrtr);
    }

    /// Return whether the current MCP client declared form elicitation.
    pub(crate) fn supports_form_elicitation(&self) -> bool {
        self.client_supports_form_elicitation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or(false)
    }

    /// Return whether MRTR and form elicitation are both available.
    pub(crate) fn supports_mutation_approval(&self) -> bool {
        self.supports_form_elicitation()
            && self
                .client_supports_mrtr
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unwrap_or(false)
    }

    /// Seal state with the request binding and a short expiry.
    pub(crate) fn seal_approval_state(
        &self,
        state: &MutationApprovalState,
        associated_data: &[u8],
    ) -> Result<String, rmcp::model::RequestStateError> {
        self.approval_codec.seal_json_with(
            state,
            &SealOptions::new()
                .associated_data(associated_data)
                .ttl(APPROVAL_TTL),
        )
    }

    /// Open and authenticate state with the request binding.
    pub(crate) fn open_approval_state(
        &self,
        sealed: &str,
        associated_data: &[u8],
    ) -> Result<MutationApprovalState, rmcp::model::RequestStateError> {
        self.approval_codec.open_json_with(sealed, associated_data)
    }

    /// Register one nonce for a pending approval round.
    pub(crate) async fn remember_approval(&self, nonce: String) {
        self.pending_approvals.lock().await.insert(nonce);
    }

    /// Consume one pending nonce exactly once.
    pub(crate) async fn consume_approval(&self, nonce: &str) -> bool {
        self.pending_approvals.lock().await.remove(nonce)
    }

    /// Check an accepted approval without consuming it. A busy edit keeps its
    /// approval usable for the same-plan retry.
    pub(crate) async fn has_approval(&self, nonce: &str) -> bool {
        self.pending_approvals.lock().await.contains(nonce)
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

    /// Resolve a handle through every actor group owned by an explicit project.
    pub(crate) async fn resolve_symbol_handle(
        &self,
        id: &crate::project::ProjectId,
        handle: crate::bridge::SymbolHandle,
    ) -> Result<(ProjectHandle, crate::project::ResolvedSymbolTarget), String> {
        self.project_registry
            .resolve_symbol_handle(id, handle)
            .await
    }

    /// Return the primary actor for an explicit registered project.
    pub(crate) async fn required_actor_for_project(
        &self,
        id: &crate::project::ProjectId,
    ) -> Result<ProjectHandle, ProjectRegistryError> {
        self.project_registry.actor(id).await
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

    #[test]
    fn test_handler_context_creation() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let context = HandlerContext::from_registry(subscriptions, ProjectRegistry::new(32));
        assert_eq!(Arc::strong_count(&context.subscriptions), 2);
    }

    #[test]
    fn handler_context_creation_does_not_require_a_global_translator() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let _context = HandlerContext::from_registry(subscriptions, ProjectRegistry::new(32));
    }

    #[tokio::test]
    async fn edit_plan_ownership_is_local_to_one_session() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let context = HandlerContext::from_registry(subscriptions, ProjectRegistry::new(32));
        let session = context.for_session();
        let plan_id = PlanId::new();

        context.remember_plan(plan_id.clone()).await;

        assert!(context.claim_plan(&plan_id).await);
        assert!(context.claim_plan(&plan_id).await);
        assert!(!session.claim_plan(&plan_id).await);
    }

    #[tokio::test]
    async fn concurrent_retries_observe_one_atomic_claim() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let context = HandlerContext::from_registry(subscriptions, ProjectRegistry::new(32));
        let plan_id = PlanId::new();
        context.remember_plan(plan_id.clone()).await;

        let (first, retry) =
            tokio::join!(context.claim_plan(&plan_id), context.claim_plan(&plan_id),);

        assert!(first);
        assert!(retry);
    }
}
