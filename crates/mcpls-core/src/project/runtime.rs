//! Project-owned translator, edit, result, and pagination runtime.

#![allow(clippy::redundant_pub_crate)]

#[allow(clippy::wildcard_imports)]
use super::*;
#[allow(clippy::wildcard_imports)]
use super::{actor::*, identity::*, registry::*, state::*};

/// Result of consuming and applying one project-owned edit plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedEditPlan {
    /// Opaque identifier of the consumed plan.
    pub plan_id: PlanId,
    /// Human-readable operations captured by the preview.
    pub operations: Vec<String>,
    /// Unified diff captured by the preview.
    pub unified_diff: String,
    /// Complete immutable unified diff retained for bounded receipt resources.
    pub complete_unified_diff: String,
    /// Files replaced successfully.
    pub committed_files: Vec<PathBuf>,
    /// Optional semantic verification outcome for a specialized refactor.
    pub verification: Option<VerificationStatus>,
    /// Post-commit provider convergence results for workspace changes.
    pub provider_synchronization: Vec<ProviderSynchronization>,
}

/// A successful apply response that did not mutate the workspace yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditNotReady {
    /// Plan that remains valid for a same-plan retry.
    pub plan_id: PlanId,
    /// Caller-visible paths currently held by another edit.
    pub blocked_paths: Vec<PathBuf>,
    /// Suggested delay before retrying.
    pub retry_after_ms: u64,
}

/// A successful apply response whose immutable plan became stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditConflict {
    /// Plan that must be previewed again.
    pub plan_id: PlanId,
    /// Paths whose preconditions no longer hold.
    pub changed_paths: Vec<PathBuf>,
    /// Stable reason code for clients.
    pub reason: String,
}

/// Expected and successful outcomes of a workspace-edit apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyEditPlanOutcome {
    /// The filesystem transaction committed.
    Applied(AppliedEditPlan),
    /// Another edit currently owns an overlapping path set.
    NotReady(EditNotReady),
    /// The plan's preconditions changed before it could commit.
    Conflict(EditConflict),
}

impl AppliedEditPlan {
    pub(super) fn detail_json(&self) -> serde_json::Value {
        serde_json::json!({
            "plan_id": self.plan_id.as_str(),
            "operations": self.operations,
            "unified_diff": self.complete_unified_diff,
            "committed_files": self.committed_files,
            "verification": self.verification.map(VerificationStatus::as_str),
            "provider_synchronization": self.provider_synchronization.iter().map(|result| serde_json::json!({
                "provider": result.provider,
                "synchronized": result.synchronized,
                "watched_file_notifications": result.watched_file_notifications,
                "message": result.message,
            })).collect::<Vec<_>>(),
        })
    }

    pub(super) fn estimated_bytes(&self) -> usize {
        self.complete_unified_diff.len()
            + self.unified_diff.len()
            + self.operations.iter().map(String::len).sum::<usize>()
            + self
                .provider_synchronization
                .iter()
                .map(|result| result.provider.len() + result.message.as_deref().map_or(0, str::len))
                .sum::<usize>()
    }

    pub(super) fn project_events(&self) -> [ProjectEvent; 2] {
        [
            ProjectEvent::FilesChanged {
                paths: self.committed_files.clone(),
            },
            ProjectEvent::EditApplied {
                plan_id: self.plan_id.clone(),
                committed_files: self.committed_files.clone(),
                operation_count: self.operations.len(),
            },
        ]
    }
}

pub(super) fn merge_provider_synchronization(
    results: &mut Vec<ProviderSynchronization>,
    result: ProviderSynchronization,
) {
    let Some(existing) = results
        .iter_mut()
        .find(|existing| existing.provider == result.provider)
    else {
        results.push(result);
        return;
    };
    existing.synchronized &= result.synchronized;
    existing.watched_file_notifications = existing
        .watched_file_notifications
        .saturating_add(result.watched_file_notifications);
    if let Some(message) = result.message {
        existing.message = Some(existing.message.take().map_or_else(
            || message.clone(),
            |current| format!("{current}; {message}"),
        ));
    }
}

pub(super) fn planned_text_changes(plan: &EditPlan) -> Vec<(PathBuf, String)> {
    plan.files()
        .iter()
        .filter(|snapshot| snapshot.original_content() != snapshot.planned_content())
        .map(|snapshot| {
            (
                snapshot.path().clone(),
                snapshot.planned_content().to_string(),
            )
        })
        .collect()
}

/// Explicit syntax accepted by the structural preview tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StructuralDialect {
    /// rust-analyzer's `experimental/ssr` rule syntax.
    RustAnalyzerSsr,
    /// ast-grep's pattern and replacement-template syntax.
    AstGrep,
}

impl StructuralDialect {
    /// Return the stable MCP wire value.
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RustAnalyzerSsr => "rust_analyzer_ssr",
            Self::AstGrep => "ast_grep",
        }
    }

    /// Return the implementation selected by this explicit dialect.
    #[must_use]
    pub(crate) const fn engine(self) -> &'static str {
        match self {
            Self::RustAnalyzerSsr => "rust_analyzer",
            Self::AstGrep => "ast_grep",
        }
    }
}

/// Actor-owned inputs for one structural search or replacement preview.
#[derive(Debug, Clone)]
pub(crate) struct StructuralReplaceRequest {
    pub(crate) file_path: String,
    pub(crate) dialect: StructuralDialect,
    pub(crate) query: String,
    pub(crate) replacement: Option<String>,
    pub(crate) language_id: Option<String>,
    pub(crate) parse_only: bool,
    pub(crate) encoding: PositionEncoding,
}

/// Write-free result of a structural search or replacement request.
#[derive(Debug, Clone)]
pub(crate) struct StructuralPreview {
    /// Stored plan when a replacement matched and was previewed.
    pub(crate) artifact: Option<PreviewArtifact>,
    /// Explicit parser/replacement syntax selected by the caller.
    pub(crate) dialect: StructuralDialect,
    /// Matched source ranges before replacement.
    pub(crate) matches: Vec<StructuralMatch>,
    /// Exact source snapshots containing those matches, listed once per file.
    pub(crate) matched_files: Vec<StructuralMatchedFile>,
    /// Whether only parser validation was requested.
    pub(crate) parse_only: bool,
}

/// Snapshot metadata needed to fetch source context without repeating file paths per match.
#[derive(Debug, Clone)]
pub(crate) struct StructuralMatchedFile {
    pub(crate) path: PathBuf,
    pub(crate) content_hash: String,
    pub(crate) document_version: Option<i32>,
    pub(crate) total_bytes: usize,
    pub(crate) total_lines: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct PathRenameRequest {
    pub(crate) old_path: String,
    pub(crate) new_path: String,
    pub(crate) encoding: PositionEncoding,
}

#[derive(Debug, Clone)]
pub(crate) struct PathRenamePreview {
    pub(crate) artifact: PreviewArtifact,
    pub(crate) providers: Vec<String>,
    pub(crate) semantic_edit_count: usize,
}

/// One LSP edit request that must be generated and snapshotted atomically.
pub(crate) enum GeneratedEditRequest {
    Rename {
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    },
    Format {
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    },
    RangeFormat {
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        tab_size: u32,
        insert_spaces: bool,
    },
    MoveItem {
        file_path: String,
        start: (u32, u32),
        end: (u32, u32),
        direction: String,
    },
}

pub(crate) struct GeneratedEditPreview {
    pub(crate) supported: bool,
    pub(crate) artifact: Option<PreviewArtifact>,
}

const WORKSPACE_SYMBOL_CACHE_MAX_ENTRIES: usize = 128;

/// Coordinate target recovered from an actor-owned snapshot handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedSymbolTarget {
    pub(crate) file_path: String,
    pub(crate) line: u32,
    pub(crate) character: u32,
}

pub(super) struct ProjectRuntime {
    pub(super) translator: Translator,
    pub(super) edit_plans: EditPlanStore,
    edit_safety: Option<EditSafetyConfig>,
    code_actions: CodeActionStore,
    pub(super) symbol_handles: std::sync::Mutex<SymbolHandleStore>,
    workspace_symbol_results: std::sync::Mutex<HashMap<String, WorkspaceSymbolResult>>,
    inspect_symbol_batch_pages: std::sync::Mutex<InspectSymbolBatchPageStore>,
    pub(super) deferred_results: std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    deferred_scope: Option<String>,
    inline_module_checks: HashMap<PlanId, InlineModuleSemanticCheck>,
    applied_edit_receipts: VecDeque<AppliedEditPlan>,
    applied_edit_receipt_bytes: usize,
    edit_conflicts: VecDeque<EditConflict>,
    pub(super) active_edit_workers: usize,
    activation_health: ActivationHealth,
    pub(super) activation_started_at: Option<Instant>,
    generation: u64,
    automatic_restart: AutomaticRestartPolicy,
}

#[derive(Debug, Clone)]
pub(super) struct InlineModuleSemanticCheck {
    source_path: PathBuf,
    destination_path: PathBuf,
    module_name: String,
    source_position: lsp_types::Position,
    pre_verification: VerificationStatus,
}

pub(super) struct PreparedEditPlan {
    pub(super) plan: EditPlan,
    pub(super) boundary: WorkspaceBoundary,
    pub(super) backup_policy: Option<BackupPolicy>,
    semantic_check: Option<InlineModuleSemanticCheck>,
    resource_operations: Vec<FileOperation>,
    text_changes: Vec<(PathBuf, String)>,
    open_documents: Vec<(PathBuf, i32, String)>,
    audit: EditAuditRecord,
    pub(super) documents: std::sync::Arc<crate::bridge::DocumentTracker>,
    lease: EditLease,
}

pub(super) enum PreparedEditResult {
    AlreadyApplied(AppliedEditPlan),
    AlreadyConflicted(EditConflict),
    Ready(Box<PreparedEditPlan>),
}

pub(super) const LANGUAGE_SERVER_EXITED: &str = "language server exited";
pub(super) const MAX_AUTOMATIC_RESTART_ATTEMPTS: usize = 3;
pub(super) const MAX_INLINE_MODULE_CHECKS: usize = 256;
pub(super) const MAX_INLINE_HOVER_CONTENT_BYTES: usize = 4 * 1024;
pub(super) const MAX_EDIT_ADMISSION_WAIT: Duration = Duration::from_secs(5);
pub(super) const MAX_APPLIED_EDIT_RECEIPTS: usize = 256;
pub(super) const MAX_APPLIED_EDIT_RECEIPT_BYTES: usize = 32 * 1024 * 1024;
pub(super) const DEFAULT_RUST_RESIDENCY_LIMIT: usize = 5;
pub(super) const AUTOMATIC_RESTART_BACKOFF: [Duration; MAX_AUTOMATIC_RESTART_ATTEMPTS] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
];

pub(super) fn defer_oversized_hover_contents(
    result: &mut HoverResult,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String> {
    if result.contents.len() <= MAX_INLINE_HOVER_CONTENT_BYTES {
        return Ok(());
    }

    let contents = std::mem::take(&mut result.contents);
    let inline_contents =
        crate::util::truncate_string(contents.clone(), MAX_INLINE_HOVER_CONTENT_BYTES);
    let encoded = serde_json::json!({"contents": contents});
    let encoded_bytes = serde_json::to_vec(&encoded).map_err(|error| error.to_string())?;
    let snapshot_hash = format!("{:x}", Sha256::digest(&encoded_bytes));
    let reference = deferred_results
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert_scoped_kind(encoded, snapshot_hash, scope, "hover_contents");
    result.contents = inline_contents;
    result.contents_resource = Some(reference);
    result.truncated = true;
    Ok(())
}

pub(super) const fn position_in_mcp_range(
    line: u32,
    character: u32,
    range: &crate::bridge::Range,
) -> bool {
    let after_start =
        line > range.start.line || (line == range.start.line && character >= range.start.character);
    let before_end =
        line < range.end.line || (line == range.end.line && character <= range.end.character);
    after_start && before_end
}

pub(super) fn structural_matches_from_workspace_edit(
    edit: &WorkspaceEdit,
) -> Result<Vec<StructuralMatch>, String> {
    let normalized = normalize(edit.clone()).expect("workspace edit normalization is infallible");
    let mut matches = Vec::new();
    for operation in normalized.operations {
        let EditOperation::Text { uri, edits, .. } = operation else {
            return Err("rust-analyzer SSR returned an unsupported resource operation".to_string());
        };
        let path = uri_to_path(&uri)
            .ok_or_else(|| "rust-analyzer SSR returned a non-file URI".to_string())?;
        if matches.len().saturating_add(edits.len()) > PreviewLimits::default().max_edits {
            return Err("rust-analyzer SSR exceeded the structural match limit".to_string());
        }
        matches.extend(edits.into_iter().map(|edit| StructuralMatch {
            path: path.clone(),
            range: edit.range,
        }));
    }
    matches.sort_by(|left, right| {
        left.path.cmp(&right.path).then_with(|| {
            (
                left.range.start.line,
                left.range.start.character,
                left.range.end.line,
                left.range.end.character,
            )
                .cmp(&(
                    right.range.start.line,
                    right.range.start.character,
                    right.range.end.line,
                    right.range.end.character,
                ))
        })
    });
    Ok(matches)
}

#[allow(clippy::mutable_key_type)]
pub(super) fn compose_path_rename_edit(
    result: WillRenameFilesResult,
    old_path: &Path,
    new_path: &Path,
) -> Result<(WorkspaceEdit, Vec<String>, usize), String> {
    let mut changes = HashMap::new();
    let mut operations = Vec::new();
    let mut annotations = HashMap::new();
    let mut semantic_edit_count = 0usize;
    for edit in result.edits {
        for (uri, edits) in edit.changes.unwrap_or_default() {
            semantic_edit_count = semantic_edit_count.saturating_add(edits.len());
            changes.entry(uri).or_insert_with(Vec::new).extend(edits);
        }
        if let Some(document_changes) = edit.document_changes {
            match document_changes {
                lsp_types::DocumentChanges::Edits(edits) => {
                    semantic_edit_count = semantic_edit_count
                        .saturating_add(edits.iter().map(|edit| edit.edits.len()).sum::<usize>());
                    operations.extend(
                        edits
                            .into_iter()
                            .map(lsp_types::DocumentChangeOperation::Edit),
                    );
                }
                lsp_types::DocumentChanges::Operations(returned) => {
                    for operation in returned {
                        let lsp_types::DocumentChangeOperation::Edit(edit) = operation else {
                            return Err(
                                "workspace/willRenameFiles returned a resource operation; MCPLS adds exactly one requested RenameFile"
                                    .to_string(),
                            );
                        };
                        semantic_edit_count = semantic_edit_count.saturating_add(edit.edits.len());
                        operations.push(lsp_types::DocumentChangeOperation::Edit(edit));
                    }
                }
            }
        }
        for (id, annotation) in edit.change_annotations.unwrap_or_default() {
            if annotations
                .insert(id.clone(), annotation.clone())
                .is_some_and(|existing| existing != annotation)
            {
                return Err(format!(
                    "workspace/willRenameFiles returned conflicting annotation {id}"
                ));
            }
        }
    }
    operations.push(lsp_types::DocumentChangeOperation::Op(
        lsp_types::ResourceOp::Rename(lsp_types::RenameFile {
            old_uri: path_to_uri(old_path).map_err(|error| error.to_string())?,
            new_uri: path_to_uri(new_path).map_err(|error| error.to_string())?,
            options: None,
            annotation_id: None,
        }),
    ));
    Ok((
        WorkspaceEdit {
            changes: (!changes.is_empty()).then_some(changes),
            document_changes: Some(lsp_types::DocumentChanges::Operations(operations)),
            change_annotations: (!annotations.is_empty()).then_some(annotations),
        },
        result.providers,
        semantic_edit_count,
    ))
}

#[derive(Debug, Default)]
pub(super) struct AutomaticRestartPolicy {
    attempts: usize,
}

impl AutomaticRestartPolicy {
    pub(super) fn next(&mut self) -> Option<AutomaticRestartAttempt> {
        let attempt = self.attempts + 1;
        let delay = AUTOMATIC_RESTART_BACKOFF.get(self.attempts).copied()?;
        self.attempts = attempt;
        Some(AutomaticRestartAttempt {
            number: attempt,
            delay,
        })
    }

    pub(super) const fn reset(&mut self) {
        self.attempts = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AutomaticRestartAttempt {
    pub(super) number: usize,
    pub(super) delay: Duration,
}

pub(super) struct StoredCodeAction {
    pub(super) file_path: String,
    pub(super) action: lsp_types::CodeActionOrCommand,
    pub(super) created_at: Instant,
}

pub(super) struct CodeActionStore {
    pub(super) entries: HashMap<PlanId, StoredCodeAction>,
    pub(super) ttl: Duration,
    pub(super) max_entries: usize,
}

pub(super) const CODE_ACTION_PAGE_SIZE: usize = 64;
pub(super) const COMPLETION_PAGE_SIZE: usize = 64;
pub(super) const SIGNATURE_PAGE_SIZE: usize = 32;
// Keep the compact page below the shared response ceiling even after hint
// identities and pagination metadata are attached.
pub(super) const INLAY_HINT_PAGE_SIZE: usize = 8;

pub(super) fn inlay_hint_page_bounds(
    hints: &[crate::bridge::translator::InlayHintEntry],
    page_token: Option<&str>,
) -> Result<(std::ops::Range<usize>, String, Option<String>), String> {
    let snapshot_identity = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(hints).map_err(|error| error.to_string())?)
    );
    let offset = page_token
        .map(|token| {
            let (identity, offset) = token
                .split_once(':')
                .ok_or_else(|| "invalid inlay hint page_token".to_owned())?;
            if identity != snapshot_identity {
                return Err("inlay hint page_token belongs to a different snapshot".to_owned());
            }
            offset
                .parse::<usize>()
                .map_err(|_| "invalid inlay hint page_token".to_owned())
        })
        .transpose()?
        .unwrap_or(0);
    if offset > hints.len() {
        return Err("inlay hint page_token is outside the provider snapshot".to_owned());
    }
    let end = offset.saturating_add(INLAY_HINT_PAGE_SIZE).min(hints.len());
    let next = (end < hints.len()).then(|| format!("{snapshot_identity}:{end}"));
    Ok((offset..end, snapshot_identity, next))
}

pub(super) fn inlay_hint_identity(
    hint: &crate::bridge::translator::InlayHintEntry,
    ordinal: usize,
) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(ordinal, hint)).unwrap_or_default())
    )
}

pub(super) fn defer_oversized_inlay_hint_payloads(
    result: &mut InlayHintsResult,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String> {
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        <= MAX_NOTIFICATION_RESULT_BYTES
    {
        return Ok(());
    }
    let complete = serde_json::to_value(&result.hints)
        .map_err(|error| format!("failed to store inlay hints: {error}"))?;
    result.hints_resource = Some(store_diagnostic_payload(
        complete,
        "inlay_hints",
        deferred_results,
        scope,
    )?);
    result.truncated = true;
    for hint in &mut result.hints {
        hint.label_parts = None;
        hint.tooltip = None;
        hint.text_edit = None;
        hint.data = None;
    }
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        > MAX_NOTIFICATION_RESULT_BYTES
    {
        for hint in &mut result.hints {
            hint.label.clear();
        }
    }
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        > MAX_NOTIFICATION_RESULT_BYTES
    {
        return Err("inlay hint result exceeds the response page budget".to_owned());
    }
    Ok(())
}

pub(super) fn signature_page_bounds(
    signatures: &[crate::bridge::translator::SignatureInfo],
    page_token: Option<&str>,
) -> Result<(std::ops::Range<usize>, String, Option<String>), String> {
    let snapshot_identity = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(signatures).map_err(|error| error.to_string())?)
    );
    let offset = page_token
        .map(|token| {
            let (identity, offset) = token
                .split_once(':')
                .ok_or_else(|| "invalid signature page_token".to_owned())?;
            if identity != snapshot_identity {
                return Err("signature page_token belongs to a different snapshot".to_owned());
            }
            offset
                .parse::<usize>()
                .map_err(|_| "invalid signature page_token".to_owned())
        })
        .transpose()?
        .unwrap_or(0);
    if offset > signatures.len() {
        return Err("signature page_token is outside the provider snapshot".to_owned());
    }
    let end = offset
        .saturating_add(SIGNATURE_PAGE_SIZE)
        .min(signatures.len());
    let next = (end < signatures.len()).then(|| format!("{snapshot_identity}:{end}"));
    Ok((offset..end, snapshot_identity, next))
}

pub(super) fn signature_identity(
    signature: &crate::bridge::translator::SignatureInfo,
    ordinal: usize,
) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(ordinal, signature)).unwrap_or_default())
    )
}

pub(super) fn defer_oversized_signature_payloads(
    result: &mut SignatureHelpResult,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String> {
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        <= MAX_NOTIFICATION_RESULT_BYTES
    {
        return Ok(());
    }
    let complete = serde_json::to_value(&result.signatures)
        .map_err(|error| format!("failed to store signatures: {error}"))?;
    result.signatures_resource = Some(store_diagnostic_payload(
        complete,
        "signature_help",
        deferred_results,
        scope,
    )?);
    for signature in &mut result.signatures {
        signature.documentation = None;
        for parameter in &mut signature.parameters {
            parameter.documentation = None;
        }
    }
    Ok(())
}

pub(super) fn completion_page_bounds<T: serde::Serialize>(
    items: &[T],
    page_token: Option<&str>,
) -> Result<(std::ops::Range<usize>, String, Option<String>), String> {
    let snapshot_identity = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(items).map_err(|error| error.to_string())?)
    );
    let offset = page_token
        .map(|token| {
            let (identity, offset) = token
                .split_once(':')
                .ok_or_else(|| "invalid completion page_token".to_owned())?;
            if identity != snapshot_identity {
                return Err("completion page_token belongs to a different snapshot".to_owned());
            }
            offset
                .parse::<usize>()
                .map_err(|_| "invalid completion page_token".to_owned())
        })
        .transpose()?
        .unwrap_or(0);
    if offset > items.len() {
        return Err("completion page_token is outside the provider snapshot".to_owned());
    }
    let end = offset.saturating_add(COMPLETION_PAGE_SIZE).min(items.len());
    let next = (end < items.len()).then(|| format!("{snapshot_identity}:{end}"));
    Ok((offset..end, snapshot_identity, next))
}

pub(super) fn completion_identity(item: &crate::bridge::Completion, ordinal: usize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(ordinal, item)).unwrap_or_default())
    )
}

pub(super) fn defer_oversized_completion_payloads(
    result: &mut CompletionsResult,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String> {
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        <= MAX_NOTIFICATION_RESULT_BYTES
    {
        return Ok(());
    }
    let complete = serde_json::to_value(&result.items)
        .map_err(|error| format!("failed to store completions: {error}"))?;
    result.items_resource = Some(store_diagnostic_payload(
        complete,
        "completions",
        deferred_results,
        scope,
    )?);
    for item in &mut result.items {
        item.documentation = None;
        item.detail = None;
        item.insert_text = None;
        item.text_edit = None;
    }
    Ok(())
}

pub(super) fn code_action_page_bounds<T: serde::Serialize>(
    actions: &[T],
    page_token: Option<&str>,
) -> Result<(std::ops::Range<usize>, String, Option<String>), String> {
    let snapshot_identity = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(actions).map_err(|error| error.to_string())?)
    );
    let offset = page_token
        .map(|token| {
            let (identity, offset) = token
                .split_once(':')
                .ok_or_else(|| "invalid code action page_token".to_owned())?;
            if identity != snapshot_identity {
                return Err("code action page_token belongs to a different snapshot".to_owned());
            }
            offset
                .parse::<usize>()
                .map_err(|_| "invalid code action page_token".to_owned())
        })
        .transpose()?
        .unwrap_or(0);
    if offset > actions.len() {
        return Err("code action page_token is outside the provider snapshot".to_owned());
    }
    let end = offset
        .saturating_add(CODE_ACTION_PAGE_SIZE)
        .min(actions.len());
    let next = (end < actions.len()).then(|| format!("{snapshot_identity}:{end}"));
    Ok((offset..end, snapshot_identity, next))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SourceSnapshot {
    Version(i32),
    Hash(String),
}

#[derive(Debug, Clone)]
pub(super) struct StoredSymbolTarget {
    file_path: PathBuf,
    pub(super) line: u32,
    pub(super) character: u32,
    snapshot: SourceSnapshot,
    created_at: Instant,
}

impl StoredSymbolTarget {
    pub(super) fn new(
        file_path: PathBuf,
        line: u32,
        character: u32,
        snapshot: SourceSnapshot,
    ) -> Self {
        Self {
            file_path,
            line,
            character,
            snapshot,
            created_at: Instant::now(),
        }
    }
}

pub(super) struct SymbolHandleStore {
    pub(super) entries: HashMap<SymbolHandle, StoredSymbolTarget>,
    pub(super) ttl: Duration,
    pub(super) max_entries: usize,
}

/// Deferred payload and the immutable snapshot that produced it.
#[derive(Debug, Clone)]
pub(crate) struct DeferredResourcePayload {
    pub value: serde_json::Value,
    pub snapshot_hash: String,
}

pub(super) struct StoredDeferredResult {
    value: serde_json::Value,
    snapshot_hash: String,
    created_at: Instant,
    scope: String,
}

pub(super) struct DeferredResultStore {
    entries: HashMap<String, StoredDeferredResult>,
    ttl: Duration,
    max_entries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct LexicalSearchPageState {
    matches: Vec<LexicalSearchMatch>,
    total_matches: usize,
    scanned_files: usize,
    scanned_bytes: usize,
    snapshot_identity: String,
    request_identity: String,
}

pub(super) struct LexicalFileSnapshot {
    path: PathBuf,
    document_version: Option<i32>,
    content_hash: String,
    source: String,
    project_relative_path: String,
}

pub(super) fn lexical_search_request_identity(request: &LexicalSearchRequest) -> String {
    let value = (
        &request.query,
        request.mode,
        request.case,
        request.multiline,
        request.max_files,
        request.include_generated,
        &request.include_paths,
        &request.exclude_paths,
        request.context_lines,
    );
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&value).unwrap_or_default())
    )
}

pub(crate) fn lexical_page_cursor(token: &str, offset: usize) -> String {
    format!("mcpls-deferred:///{token}?offset={offset:020}")
}

#[cfg(test)]
mod tests {
    use super::{lexical_page_cursor, parse_lexical_page_cursor};

    #[test]
    fn lexical_page_cursor_round_trips_the_snapshot_offset() {
        let cursor = lexical_page_cursor("snapshot", 7);
        assert_eq!(parse_lexical_page_cursor(&cursor), Ok(("snapshot", 7)));
    }
}

pub(super) fn parse_lexical_page_cursor(cursor: &str) -> Result<(&str, usize), String> {
    let cursor = cursor.strip_prefix("mcpls-deferred:///").ok_or_else(|| {
        "page_token must be the next_cursor returned by lexical_search".to_owned()
    })?;
    let (token, offset) = cursor
        .split_once("?offset=")
        .ok_or_else(|| "invalid lexical_search page_token".to_owned())?;
    if token.is_empty() {
        return Err("invalid lexical_search page_token".to_owned());
    }
    let offset = offset
        .parse::<usize>()
        .map_err(|_| "invalid lexical_search page_token offset".to_owned())?;
    Ok((token, offset))
}

#[derive(Clone)]
pub(super) struct InspectSymbolBatchSnapshot {
    pub(super) entries: Vec<InspectSymbolBatchEntry>,
    pub(super) inspections_started: usize,
    pub(super) snapshot_identity: String,
    pub(super) truncated: bool,
    pub(super) max_items: usize,
}

pub(super) struct StoredInspectSymbolBatchSnapshot {
    snapshot: InspectSymbolBatchSnapshot,
    scope: String,
    created_at: Instant,
}

pub(super) struct InspectSymbolBatchPageStore {
    entries: HashMap<String, StoredInspectSymbolBatchSnapshot>,
    ttl: Duration,
    max_entries: usize,
}

impl InspectSymbolBatchPageStore {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(15 * 60),
            max_entries: 64,
        }
    }

    pub(super) fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.created_at) < self.ttl);
    }

    pub(super) fn insert(&mut self, snapshot: InspectSymbolBatchSnapshot, scope: &str) -> String {
        self.prune();
        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.created_at)
                .map(|(token, _)| token.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        let token = uuid::Uuid::new_v4().to_string();
        self.entries.insert(
            token.clone(),
            StoredInspectSymbolBatchSnapshot {
                snapshot,
                scope: scope.to_owned(),
                created_at: Instant::now(),
            },
        );
        token
    }

    pub(super) fn read(
        &mut self,
        token: &str,
        scope: &str,
    ) -> Result<InspectSymbolBatchSnapshot, String> {
        self.prune();
        self.entries
            .get(token)
            .filter(|entry| entry.scope == scope)
            .map(|entry| entry.snapshot.clone())
            .ok_or_else(|| "stale_resource: inspect batch page is missing or expired".to_owned())
    }

    pub(super) fn remove(&mut self, token: &str) {
        self.entries.remove(token);
    }
}

pub(super) fn inspect_symbol_batch_cursor(token: &str, offset: usize) -> String {
    format!("mcpls-inspect-batch:///{token}?offset={offset:020}")
}

pub(super) fn parse_inspect_symbol_batch_cursor(cursor: &str) -> Result<(&str, usize), String> {
    let cursor = cursor
        .strip_prefix("mcpls-inspect-batch:///")
        .ok_or_else(|| {
            "page_token must be the next_cursor returned by inspect_symbol_batch".to_owned()
        })?;
    let (token, offset) = cursor
        .split_once("?offset=")
        .ok_or_else(|| "invalid inspect batch page_token".to_owned())?;
    if token.is_empty() {
        return Err("invalid inspect batch page_token".to_owned());
    }
    let offset = offset
        .parse::<usize>()
        .map_err(|_| "invalid inspect batch page_token offset".to_owned())?;
    Ok((token, offset))
}

pub(super) fn update_inspect_symbol_batch_page_metadata(
    result: &mut InspectSymbolBatchResult,
    snapshot: &InspectSymbolBatchSnapshot,
    token: &str,
    offset: usize,
) {
    result.returned_targets = result.entries.len();
    result.remaining_targets = snapshot
        .entries
        .len()
        .saturating_sub(offset + result.returned_targets);
    result.next_cursor = (result.remaining_targets > 0)
        .then(|| inspect_symbol_batch_cursor(token, offset + result.returned_targets));
    result.returned_items = result
        .entries
        .iter()
        .filter_map(|entry| entry.result.as_ref())
        .map(|entry| entry.sections.returned_items())
        .sum();
    result.truncated = snapshot.truncated || result.remaining_targets > 0;
    result.returned_bytes = 0;
    for _ in 0..4 {
        let returned_bytes = serde_json::to_vec(result).map_or(usize::MAX, |json| json.len());
        if result.returned_bytes == returned_bytes {
            break;
        }
        result.returned_bytes = returned_bytes;
    }
}

pub(super) fn update_inspect_symbol_byte_count(result: &mut InspectSymbolResult) {
    // `returned_bytes` is part of the serialized result, so assigning it
    // once under-reports whenever its digit count changes the payload.
    for _ in 0..4 {
        let serialized_bytes = serde_json::to_vec(result).map_or(0, |json| json.len());
        if result.returned_bytes == serialized_bytes {
            break;
        }
        result.returned_bytes = serialized_bytes;
    }
}

pub(super) fn bounded_inspect_symbol_batch_page(
    snapshot: &InspectSymbolBatchSnapshot,
    token: &str,
    offset: usize,
) -> Result<InspectSymbolBatchResult, String> {
    if offset > snapshot.entries.len() {
        return Err("inspect batch page_token offset is outside the retained result".to_owned());
    }
    let max_bytes = crate::bridge::translator::INSPECT_SYMBOL_RESULT_MAX_BYTES;
    let mut result = InspectSymbolBatchResult {
        entries: Vec::new(),
        inspections_started: snapshot.inspections_started,
        total_targets: snapshot.entries.len(),
        returned_targets: 0,
        remaining_targets: snapshot.entries.len().saturating_sub(offset),
        next_cursor: None,
        snapshot_identity: snapshot.snapshot_identity.clone(),
        returned_items: 0,
        budget: crate::bridge::InspectSymbolBudget {
            max_bytes,
            max_items: snapshot.max_items,
        },
        returned_bytes: 0,
        truncated: snapshot.truncated,
    };

    for entry in snapshot.entries.iter().skip(offset) {
        result.entries.push(entry.clone());
        update_inspect_symbol_batch_page_metadata(&mut result, snapshot, token, offset);
        if serde_json::to_vec(&result).map_or(usize::MAX, |json| json.len()) > max_bytes {
            result.entries.pop();
            update_inspect_symbol_batch_page_metadata(&mut result, snapshot, token, offset);
            break;
        }
    }

    if result.entries.is_empty() && offset < snapshot.entries.len() {
        return Err("inspect batch target metadata exceeds the response page budget".to_owned());
    }
    debug_assert!(serde_json::to_vec(&result).is_ok_and(|json| json.len() <= max_bytes));
    Ok(result)
}

impl DeferredResultStore {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(15 * 60),
            max_entries: 128,
        }
    }

    pub(super) fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, result| now.duration_since(result.created_at) < self.ttl);
    }

    pub(super) fn insert_scoped(
        &mut self,
        value: serde_json::Value,
        snapshot_hash: String,
        scope: &str,
    ) -> DeferredResourceReference {
        self.insert_scoped_kind(value, snapshot_hash, scope, "inspect_symbol_section")
    }

    pub(super) fn insert_scoped_kind(
        &mut self,
        value: serde_json::Value,
        snapshot_hash: String,
        scope: &str,
        kind: &str,
    ) -> DeferredResourceReference {
        self.prune();
        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, result)| result.created_at)
                .map(|(token, _)| token.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        let token = uuid::Uuid::new_v4().to_string();
        let total_bytes = serde_json::to_vec(&value).ok().map(|json| json.len());
        self.entries.insert(
            token.clone(),
            StoredDeferredResult {
                value,
                snapshot_hash: snapshot_hash.clone(),
                created_at: Instant::now(),
                scope: scope.to_owned(),
            },
        );
        DeferredResourceReference {
            uri: format!("mcpls-deferred:///{token}"),
            kind: kind.to_owned(),
            snapshot_hash,
            document_version: None,
            total_bytes,
        }
    }

    pub(super) fn invalidate_scope(&mut self, scope: &str) {
        self.entries.retain(|_, result| result.scope != scope);
    }

    pub(super) fn read(&mut self, token: &str) -> Result<DeferredResourcePayload, String> {
        self.read_entry(token, None)
            .map(|result| DeferredResourcePayload {
                value: result.value.clone(),
                snapshot_hash: result.snapshot_hash.clone(),
            })
    }

    pub(super) fn read_scoped(
        &mut self,
        token: &str,
        scope: &str,
    ) -> Result<serde_json::Value, String> {
        self.read_entry(token, Some(scope))
            .map(|result| result.value.clone())
    }

    pub(super) fn read_entry(
        &mut self,
        token: &str,
        scope: Option<&str>,
    ) -> Result<&StoredDeferredResult, String> {
        self.prune();
        self.entries
            .get(token)
            .filter(|result| scope.is_none_or(|scope| result.scope == scope))
            .ok_or_else(|| "stale_resource: deferred result is missing or expired".to_owned())
    }

    pub(super) fn remove(&mut self, token: &str) {
        self.entries.remove(token);
    }
}

pub(super) fn attach_document_symbol_handles(
    store: &mut SymbolHandleStore,
    symbols: &mut [crate::bridge::Symbol],
    path: &Path,
    snapshot: &SourceSnapshot,
    parent: Option<&SymbolHandle>,
) {
    for symbol in symbols {
        symbol.parent_symbol_handle = parent.cloned();
        let handle = store.insert(StoredSymbolTarget::new(
            path.to_path_buf(),
            symbol.selection_range.start.line,
            symbol.selection_range.start.character,
            snapshot.clone(),
        ));
        symbol.symbol_handle = Some(handle.clone());
        if let Some(children) = &mut symbol.children {
            attach_document_symbol_handles(store, children, path, snapshot, Some(&handle));
        }
    }
}

pub(super) fn rendered_workspace_symbol_position(
    rendered: &str,
    name: &str,
    range: &crate::bridge::Range,
) -> Option<(u32, u32)> {
    (!name.is_empty()).then_some(())?;

    rendered.lines().find_map(|rendered_line| {
        let (number, text) = rendered_line.split_once(" | ")?;
        let line = number.trim().parse::<u32>().ok()?;
        if !(range.start.line..=range.end.line).contains(&line) {
            return None;
        }
        symbol_position_in_line(line, text, name)
    })
}

pub(super) fn source_symbol_position(
    source: &str,
    name: &str,
    range: &crate::bridge::Range,
) -> Option<(u32, u32)> {
    source.lines().enumerate().find_map(|(index, text)| {
        let line = u32::try_from(index).ok()?.saturating_add(1);
        (range.start.line..=range.end.line)
            .contains(&line)
            .then(|| symbol_position_in_line(line, text, name))
            .flatten()
    })
}

pub(super) fn symbol_position_in_line(line: u32, text: &str, name: &str) -> Option<(u32, u32)> {
    (!name.is_empty()).then_some(())?;
    let text = text.trim_start();
    if text.starts_with("//") || text.starts_with('#') {
        return None;
    }
    text.match_indices(name).find_map(|(offset, _)| {
        let before = text[..offset].chars().next_back();
        let after = text[offset + name.len()..].chars().next();
        if before.is_none_or(|character| !character.is_alphanumeric() && character != '_')
            && after.is_none_or(|character| !character.is_alphanumeric() && character != '_')
        {
            Some((
                line,
                u32::try_from(text[..offset].encode_utf16().count()).ok()? + 1,
            ))
        } else {
            None
        }
    })
}

pub(super) fn rendered_struct_declaration(
    rendered: &str,
    name: &str,
    range: &crate::bridge::Range,
) -> bool {
    rendered.lines().any(|rendered_line| {
        let Some((number, text)) = rendered_line.split_once(" | ") else {
            return false;
        };
        let Ok(line) = number.trim().parse::<u32>() else {
            return false;
        };
        if line != range.start.line {
            return false;
        }
        let text = text.trim_start();
        let Some((prefix, _)) = text.split_once(name) else {
            return false;
        };
        prefix.split_whitespace().next_back() == Some("struct")
    })
}

pub(super) fn discard_workspace_symbol_struct_uses(symbols: &mut Vec<WorkspaceSymbol>) {
    let declarations = symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == "Struct"
                && matches!(
                    &symbol.location.source,
                    SourceContext::Available(frame)
                        if rendered_struct_declaration(&frame.text, &symbol.name, &symbol.location.range)
                )
        })
        .map(|symbol| (symbol.name.clone(), symbol.location.uri.clone()))
        .collect::<HashSet<_>>();
    symbols.retain(|symbol| {
        symbol.kind != "Struct"
            || !declarations.contains(&(symbol.name.clone(), symbol.location.uri.clone()))
            || matches!(
                &symbol.location.source,
                SourceContext::Available(frame)
                    if rendered_struct_declaration(&frame.text, &symbol.name, &symbol.location.range)
            )
    });
}

#[test]
fn rendered_workspace_symbol_position_skips_docs_and_declaration_prefixes() {
    let range = crate::bridge::Range {
        start: crate::bridge::Position2D {
            line: 65,
            character: 1,
        },
        end: crate::bridge::Position2D {
            line: 65,
            character: 24,
        },
    };

    assert_eq!(
        rendered_workspace_symbol_position(
            "  64 | /// Adds two values.\n  65 | pub fn add(a: i32, b: i32) -> i32 {\n",
            "add",
            &range,
        ),
        Some((65, 8)),
    );
}

#[test]
fn rendered_struct_declaration_rejects_struct_uses() {
    let range = crate::bridge::Range {
        start: crate::bridge::Position2D {
            line: 2,
            character: 13,
        },
        end: crate::bridge::Position2D {
            line: 2,
            character: 18,
        },
    };
    assert!(!rendered_struct_declaration(
        "   1 | pub struct Point { x: f64 }\n   2 | let p = Point { x: 1.0 };\n",
        "Point",
        &range,
    ));
}

pub(super) fn missing_call_hierarchy_item()
-> crate::bridge::InspectSection<crate::bridge::InspectCalls> {
    crate::bridge::InspectSection::unavailable(
        "call hierarchy provider returned no item at the symbol selection",
    )
}

pub(super) async fn inspect_if_requested<T>(
    requested: bool,
    request: impl Future<Output = Result<T, String>>,
) -> Option<Result<T, String>> {
    if !requested {
        return None;
    }
    Some(request.await)
}

impl SymbolHandleStore {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(15 * 60),
            max_entries: 1024,
        }
    }

    pub(super) fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, target| now.duration_since(target.created_at) < self.ttl);
    }

    pub(super) fn insert(&mut self, target: StoredSymbolTarget) -> SymbolHandle {
        self.prune();
        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, target)| target.created_at)
                .map(|(handle, _)| handle.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        let handle = SymbolHandle::new();
        self.entries.insert(handle.clone(), target);
        handle
    }

    pub(super) fn resolve(&mut self, handle: &SymbolHandle) -> Result<StoredSymbolTarget, String> {
        self.prune();
        self.entries.get(handle).cloned().ok_or_else(|| {
            "invalid_symbol_handle: handle is missing, forged, expired, or belongs to another project; rerun symbol discovery"
                .to_owned()
        })
    }
}

impl CodeActionStore {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(15 * 60),
            max_entries: 256,
        }
    }

    pub(super) fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.created_at) < self.ttl);
    }

    pub(super) fn enforce_capacity(&mut self) {
        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.created_at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    pub(super) fn insert(&mut self, action: StoredCodeAction) -> PlanId {
        self.prune();
        self.enforce_capacity();
        let id = PlanId::new();
        self.entries.insert(id.clone(), action);
        id
    }

    pub(super) fn take(&mut self, id: &PlanId) -> Result<StoredCodeAction, String> {
        self.prune();
        self.entries
            .remove(id)
            .ok_or_else(|| format!("code action reference is missing or expired: {id}"))
    }
}

pub(super) fn call_hierarchy_snapshot_hash(items: &[CallHierarchyItemResult]) -> String {
    items
        .iter()
        .find_map(|item| match &item.source {
            Some(SourceContext::Available(frame)) => Some(frame.content_hash.clone()),
            Some(SourceContext::Deferred { resource }) => Some(resource.snapshot_hash.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct WorkspaceSymbolBatchPageState {
    entries: Vec<WorkspaceSymbolBatchEntry>,
    unique_queries: usize,
    provider_requests: usize,
    snapshot_identity: String,
    cache_hit: bool,
    filter_identity: String,
}

fn workspace_symbol_batch_filter_identity(request: &WorkspaceSymbolBatchRequest) -> String {
    let value = (
        &request.kind_filter,
        request.match_mode,
        request.scope,
        request.include_generated,
    );
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&value).unwrap_or_default())
    )
}

pub(super) fn workspace_symbol_batch_cursor(
    token: &str,
    entry_offset: usize,
    symbol_offset: usize,
) -> String {
    format!("mcpls-workspace-symbol-batch:///{token}?entry={entry_offset}&symbol={symbol_offset}")
}

pub(super) fn parse_workspace_symbol_batch_cursor(
    cursor: &str,
) -> Result<(&str, usize, usize), String> {
    let cursor = cursor
        .strip_prefix("mcpls-workspace-symbol-batch:///")
        .ok_or_else(|| {
            "page_token must be the next_cursor returned by workspace_symbol_search".to_owned()
        })?;
    let (token, query) = cursor
        .split_once("?entry=")
        .ok_or_else(|| "invalid workspace-symbol batch page_token".to_owned())?;
    let (entry_offset, symbol_offset) = query
        .split_once("&symbol=")
        .ok_or_else(|| "invalid workspace-symbol batch page_token".to_owned())?;
    if token.is_empty() {
        return Err("invalid workspace-symbol batch page_token".to_owned());
    }
    let entry_offset = entry_offset
        .parse::<usize>()
        .map_err(|_| "invalid workspace-symbol batch entry offset".to_owned())?;
    let symbol_offset = symbol_offset
        .parse::<usize>()
        .map_err(|_| "invalid workspace-symbol batch symbol offset".to_owned())?;
    Ok((token, entry_offset, symbol_offset))
}

#[allow(clippy::too_many_lines)]
pub(super) fn bounded_workspace_symbol_batch_page(
    state: &WorkspaceSymbolBatchPageState,
    token: &str,
    entry_offset: usize,
    symbol_offset: usize,
    max_items: usize,
    max_bytes: usize,
) -> Result<WorkspaceSymbolBatchResult, String> {
    if entry_offset > state.entries.len() {
        return Err(
            "workspace-symbol batch page_token entry is outside the retained result".to_owned(),
        );
    }
    if max_items == 0 {
        return Err("max_items must be positive".to_owned());
    }

    let mut page = WorkspaceSymbolBatchResult {
        entries: Vec::new(),
        unique_queries: state.unique_queries,
        provider_requests: state.provider_requests,
        snapshot_identity: state.snapshot_identity.clone(),
        cache_hit: state.cache_hit,
        returned: 0,
        returned_queries: 0,
        remaining_queries: state.entries.len().saturating_sub(entry_offset),
        next_cursor: None,
        truncated: false,
        max_bytes,
    };
    let mut current_entry = entry_offset;
    let mut current_symbol = symbol_offset;

    'page: while current_entry < state.entries.len() {
        let entry = &state.entries[current_entry];
        let Some(full) = entry.result.as_ref() else {
            let mut trial = page.clone();
            trial.entries.push(entry.clone());
            trial.returned_queries += 1;
            trial.remaining_queries = state.entries.len().saturating_sub(current_entry + 1);
            trial.next_cursor = (current_entry + 1 < state.entries.len())
                .then(|| workspace_symbol_batch_cursor(token, current_entry + 1, 0));
            trial.truncated = trial.next_cursor.is_some();
            if serde_json::to_vec(&trial).map_or(usize::MAX, |encoded| encoded.len()) > max_bytes {
                if page.entries.is_empty() {
                    return Err(
                        "max_bytes is too small to return one workspace-symbol batch identity"
                            .to_owned(),
                    );
                }
                break;
            }
            page = trial;
            current_entry += 1;
            current_symbol = 0;
            continue;
        };

        if current_symbol > full.symbols.len() {
            return Err(
                "workspace-symbol batch page_token symbol is outside the retained result"
                    .to_owned(),
            );
        }
        let remaining_items = max_items.saturating_sub(page.returned);
        if remaining_items == 0 && current_symbol < full.symbols.len() {
            break;
        }
        let mut take = full
            .symbols
            .len()
            .saturating_sub(current_symbol)
            .min(remaining_items);
        let next_position = |take: usize| {
            let next_symbol = current_symbol.saturating_add(take);
            if next_symbol < full.symbols.len() {
                (current_entry, next_symbol)
            } else {
                (current_entry + 1, 0)
            }
        };

        loop {
            let mut bounded = full.clone();
            bounded.symbols = full.symbols[current_symbol..current_symbol + take].to_vec();
            bounded.returned = take;
            bounded.remaining = full
                .total
                .saturating_sub(current_symbol.saturating_add(take));
            bounded.next_cursor = None;
            bounded.max_bytes = Some(max_bytes);
            bounded.truncated =
                full.truncated || current_symbol.saturating_add(take) < full.symbols.len();
            let (next_entry, next_symbol) = next_position(take);
            let mut trial = page.clone();
            trial.entries.push(WorkspaceSymbolBatchEntry {
                query: entry.query.clone(),
                result: Some(bounded),
                reused_from: entry.reused_from,
                skipped_by_budget: false,
            });
            trial.returned = trial.returned.saturating_add(take);
            trial.returned_queries += 1;
            trial.remaining_queries = state.entries.len().saturating_sub(next_entry);
            trial.next_cursor = (next_entry < state.entries.len())
                .then(|| workspace_symbol_batch_cursor(token, next_entry, next_symbol));
            trial.truncated = trial.next_cursor.is_some();
            if serde_json::to_vec(&trial).map_or(usize::MAX, |encoded| encoded.len()) <= max_bytes {
                page = trial;
                current_entry = next_entry;
                current_symbol = next_symbol;
                break;
            }
            if take == 0 {
                if page.entries.is_empty() {
                    return Err(
                        "max_bytes is too small to return one workspace-symbol batch identity"
                            .to_owned(),
                    );
                }
                break 'page;
            }
            take -= 1;
        }
    }

    if page.entries.is_empty() {
        return Err(
            "max_bytes is too small to return one workspace-symbol batch identity".to_owned(),
        );
    }
    page.next_cursor = (current_entry < state.entries.len())
        .then(|| workspace_symbol_batch_cursor(token, current_entry, current_symbol));
    page.remaining_queries = state.entries.len().saturating_sub(current_entry);
    page.truncated = page.next_cursor.is_some()
        || state
            .entries
            .iter()
            .any(|entry| entry.result.as_ref().is_some_and(|result| result.truncated));
    debug_assert!(serde_json::to_vec(&page).is_ok_and(|encoded| encoded.len() <= max_bytes));
    Ok(page)
}
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct WorkspaceSymbolPageState {
    total: usize,
    snapshot_identity: String,
    symbols: Vec<WorkspaceSymbol>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct DocumentSymbolPageState {
    pub(super) total: usize,
    pub(super) snapshot_identity: String,
    pub(super) document_version: Option<i32>,
    pub(super) project_relative_path: Option<String>,
    pub(super) source_resource: DeferredResourceReference,
    pub(super) filters: DocumentSymbolOptions,
    pub(super) symbols: Vec<crate::bridge::Symbol>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct DiagnosticsPageState {
    pub(super) file_path: String,
    pub(super) fresh: bool,
    pub(super) result: DiagnosticsResult,
}

pub(super) fn remaining_diagnostic_occurrences(
    diagnostics: &VecDeque<crate::bridge::Diagnostic>,
    preserve_locations: bool,
) -> usize {
    diagnostics
        .iter()
        .map(|diagnostic| {
            if preserve_locations {
                diagnostic.context.occurrences.len()
            } else {
                diagnostic.context.occurrence_count
            }
        })
        .sum()
}

pub(super) fn set_diagnostics_page_metadata(
    result: &mut DiagnosticsResult,
    remaining_diagnostics: usize,
    remaining_groups: usize,
    source_truncated: bool,
) {
    const CURSOR_PLACEHOLDER: &str = "mcpls-deferred:///00000000-0000-0000-0000-000000000000";

    result.returned_groups = result.diagnostics.len();
    result.remaining_groups = remaining_groups;
    result.omitted_groups = result.remaining_groups;
    result.remaining_diagnostics = remaining_diagnostics;
    let has_continuation = remaining_groups != 0;
    result.next_cursor = has_continuation.then(|| CURSOR_PLACEHOLDER.to_owned());
    result.truncated = source_truncated || has_continuation;
}

pub(super) fn finish_diagnostics_page_metadata(
    result: &mut DiagnosticsResult,
    remaining: &VecDeque<crate::bridge::Diagnostic>,
    source_truncated: bool,
) {
    set_diagnostics_page_metadata(
        result,
        remaining_diagnostic_occurrences(remaining, result.filters.preserve_locations),
        remaining.len(),
        source_truncated,
    );
}

pub(super) fn defer_oversized_diagnostic_related_information(
    result: &mut DiagnosticsResult,
    max_bytes: usize,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String> {
    if serde_json::to_vec(result).map_or(usize::MAX, |encoded| encoded.len()) <= max_bytes {
        return Ok(());
    }
    for diagnostic in &mut result.diagnostics {
        let related = std::mem::take(&mut diagnostic.context.related_information);
        if related.is_empty() {
            continue;
        }
        let encoded = serde_json::to_vec(&related)
            .map_err(|error| format!("failed to store related diagnostics: {error}"))?;
        let value = serde_json::to_value(&related)
            .map_err(|error| format!("failed to store related diagnostics: {error}"))?;
        let snapshot_hash = format!("{:x}", Sha256::digest(&encoded));
        let reference = deferred_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert_scoped_kind(
                value,
                snapshot_hash,
                scope,
                "diagnostic_related_information",
            );
        diagnostic.context.related_information_resource = Some(reference);
    }
    Ok(())
}

pub(super) fn store_diagnostic_payload(
    value: serde_json::Value,
    kind: &str,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<DeferredResourceReference, String> {
    let encoded = serde_json::to_vec(&value)
        .map_err(|error| format!("failed to store diagnostic payload: {error}"))?;
    let snapshot_hash = format!("{:x}", Sha256::digest(&encoded));
    Ok(deferred_results
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert_scoped_kind(value, snapshot_hash, scope, kind))
}

pub(super) fn bound_format_document_result(
    result: &mut FormatDocumentResult,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String> {
    let encoded = serde_json::to_vec(&result.edits)
        .map_err(|error| format!("failed to encode formatting edits: {error}"))?;
    result.total_edits = result.edits.len();
    result.returned_edits = result.edits.len();
    result.edit_bytes = encoded.len();
    result.edit_digest = format!("{:x}", Sha256::digest(&encoded));
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        <= MAX_NOTIFICATION_RESULT_BYTES
    {
        return Ok(());
    }

    let complete = serde_json::to_value(&result.edits)
        .map_err(|error| format!("failed to store formatting edits: {error}"))?;
    result.edits_resource = Some(store_diagnostic_payload(
        complete,
        "format_document_edits",
        deferred_results,
        scope,
    )?);
    result.edits.clear();
    result.returned_edits = 0;
    result.deferred = true;
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        > MAX_NOTIFICATION_RESULT_BYTES
    {
        return Err("format document result exceeds the response budget".to_owned());
    }
    Ok(())
}

pub(super) fn bound_rename_result(
    result: &mut RenameResult,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String> {
    result.total_files = result.changes.len();
    result.total_edits = result.changes.iter().map(|file| file.edits.len()).sum();
    result.total_operations = result.operations.len();
    result.returned_files = result.total_files;
    result.returned_edits = result.total_edits;
    result.returned_operations = result.total_operations;
    let encoded = serde_json::to_vec(&serde_json::json!({
        "changes": &result.changes,
        "operations": &result.operations,
    }))
    .map_err(|error| format!("failed to encode rename workspace edit: {error}"))?;
    result.edit_bytes = encoded.len();
    result.edit_digest = format!("{:x}", Sha256::digest(&encoded));
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        <= MAX_NOTIFICATION_RESULT_BYTES
    {
        return Ok(());
    }

    let complete = serde_json::to_value(&*result)
        .map_err(|error| format!("failed to store rename workspace edit: {error}"))?;
    result.changes_resource = Some(store_diagnostic_payload(
        complete,
        "rename_workspace_edit",
        deferred_results,
        scope,
    )?);
    result.changes.clear();
    result.operations.clear();
    result.returned_files = 0;
    result.returned_edits = 0;
    result.returned_operations = 0;
    result.deferred = true;
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        > MAX_NOTIFICATION_RESULT_BYTES
    {
        return Err("rename result exceeds the response budget".to_owned());
    }
    Ok(())
}

pub(super) trait NotificationMessageEntry {
    fn message(&self) -> &str;
    fn take_message(&mut self) -> String;
    fn set_message(&mut self, message: String, resource: DeferredResourceReference);
}

pub(super) const MAX_NOTIFICATION_RESULT_BYTES: usize = 16 * 1024;

pub(super) fn bound_notification_page<T>(
    entries: &mut Vec<T>,
    total: usize,
    snapshot_identity: &str,
    cursor: Option<&str>,
    mut serialize_with_metadata: impl FnMut(&[T], usize, usize, Option<String>) -> usize,
) -> Result<(), String> {
    let start = cursor
        .map(|cursor| {
            cursor
                .rsplit_once(':')
                .and_then(|(_, offset)| offset.parse::<usize>().ok())
                .ok_or_else(|| format!("invalid notification cursor: {cursor}"))
        })
        .transpose()?
        .unwrap_or(0);

    loop {
        let page_end = start.saturating_add(entries.len());
        let remaining = total.saturating_sub(page_end);
        let next_cursor = (remaining > 0).then(|| format!("{snapshot_identity}:{page_end}"));
        let serialized_bytes =
            serialize_with_metadata(entries, entries.len(), remaining, next_cursor);
        if serialized_bytes <= MAX_NOTIFICATION_RESULT_BYTES || entries.len() <= 1 {
            if serialized_bytes > MAX_NOTIFICATION_RESULT_BYTES {
                return Err("notification result exceeds the response page budget".to_owned());
            }
            return Ok(());
        }
        entries.pop();
    }
}

pub(super) fn defer_notification_messages_for_scope<T>(
    entries: &mut [T],
    kind: &str,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String>
where
    T: NotificationMessageEntry,
{
    for entry in entries {
        if entry.message().len() <= 4 * 1024 {
            continue;
        }
        let message = entry.take_message();
        let reference = store_diagnostic_payload(
            serde_json::Value::String(message),
            kind,
            deferred_results,
            scope,
        )?;
        entry.set_message("[server message deferred]".to_owned(), reference);
    }
    Ok(())
}

pub(super) fn bound_server_logs_result(
    result: &mut ServerLogsResult,
    cursor: Option<&str>,
) -> Result<(), String> {
    let mut logs = std::mem::take(&mut result.logs);
    bound_notification_page(
        &mut logs,
        result.total,
        &result.snapshot_identity,
        cursor,
        |entries, returned, remaining, next_cursor| {
            result.returned = returned;
            result.remaining = remaining;
            result.next_cursor = next_cursor;
            serde_json::to_vec(&ServerLogsResult {
                returned: result.returned,
                remaining: result.remaining,
                total: result.total,
                snapshot_identity: result.snapshot_identity.clone(),
                next_cursor: result.next_cursor.clone(),
                logs: entries.to_vec(),
            })
            .map_or(usize::MAX, |json| json.len())
        },
    )?;
    result.logs = logs;
    Ok(())
}

pub(super) fn bound_server_messages_result(
    result: &mut ServerMessagesResult,
    cursor: Option<&str>,
) -> Result<(), String> {
    let mut messages = std::mem::take(&mut result.messages);
    bound_notification_page(
        &mut messages,
        result.total,
        &result.snapshot_identity,
        cursor,
        |entries, returned, remaining, next_cursor| {
            result.returned = returned;
            result.remaining = remaining;
            result.next_cursor = next_cursor;
            serde_json::to_vec(&ServerMessagesResult {
                returned: result.returned,
                remaining: result.remaining,
                total: result.total,
                snapshot_identity: result.snapshot_identity.clone(),
                next_cursor: result.next_cursor.clone(),
                messages: entries.to_vec(),
            })
            .map_or(usize::MAX, |json| json.len())
        },
    )?;
    result.messages = messages;
    Ok(())
}

pub(super) fn defer_semantic_discovery_payloads(
    result: &mut crate::bridge::SemanticDiscoveryResult,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String> {
    loop {
        if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
            <= MAX_NOTIFICATION_RESULT_BYTES
        {
            return Ok(());
        }
        if !result.locations.is_empty() {
            let locations = std::mem::take(&mut result.locations);
            result.locations_returned = 0;
            result.locations_resource = Some(store_diagnostic_payload(
                serde_json::to_value(locations)
                    .map_err(|error| format!("failed to store discovery locations: {error}"))?,
                "semantic_locations",
                deferred_results,
                scope,
            )?);
            continue;
        }
        if !result.selection_ranges.is_empty() {
            let selection_ranges = std::mem::take(&mut result.selection_ranges);
            result.selection_ranges_returned = 0;
            result.selection_ranges_resource = Some(store_diagnostic_payload(
                serde_json::to_value(selection_ranges)
                    .map_err(|error| format!("failed to store selection ranges: {error}"))?,
                "semantic_selection_ranges",
                deferred_results,
                scope,
            )?);
            continue;
        }
        if let Some(expansion) = result.macro_expansion.take() {
            let value = serde_json::to_value(expansion)
                .map_err(|error| format!("failed to store macro expansion: {error}"))?;
            result.macro_expansion_resource = Some(store_diagnostic_payload(
                value,
                "macro_expansion",
                deferred_results,
                scope,
            )?);
            continue;
        }
        if !result.runnables.is_empty() {
            let runnables = std::mem::take(&mut result.runnables);
            result.runnables_returned = 0;
            result.runnables_resource = Some(store_diagnostic_payload(
                serde_json::Value::Array(runnables),
                "semantic_runnables",
                deferred_results,
                scope,
            )?);
            continue;
        }
        return Ok(());
    }
}

pub(super) fn defer_oversized_code_action_payloads(
    result: &mut CodeActionsResult,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String> {
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        <= MAX_NOTIFICATION_RESULT_BYTES
    {
        return Ok(());
    }
    let complete = serde_json::to_value(&result.actions)
        .map_err(|error| format!("failed to store code actions: {error}"))?;
    result.actions_resource = Some(store_diagnostic_payload(
        complete,
        "code_actions",
        deferred_results,
        scope,
    )?);
    for action in &mut result.actions {
        action.edit = None;
        action.workspace_edit = None;
        action.command = None;
        action.data = None;
    }
    if serde_json::to_vec(result).map_or(usize::MAX, |json| json.len())
        > MAX_NOTIFICATION_RESULT_BYTES
    {
        for action in &mut result.actions {
            action.diagnostics.clear();
        }
    }
    Ok(())
}

impl NotificationMessageEntry for LogEntry {
    fn message(&self) -> &str {
        &self.message
    }

    fn take_message(&mut self) -> String {
        std::mem::take(&mut self.message)
    }

    fn set_message(&mut self, message: String, resource: DeferredResourceReference) {
        self.message = message;
        self.message_resource = Some(resource);
    }
}

impl NotificationMessageEntry for ServerMessage {
    fn message(&self) -> &str {
        &self.message
    }

    fn take_message(&mut self) -> String {
        std::mem::take(&mut self.message)
    }

    fn set_message(&mut self, message: String, resource: DeferredResourceReference) {
        self.message = message;
        self.message_resource = Some(resource);
    }
}

pub(super) fn defer_oversized_diagnostic_payloads(
    result: &mut DiagnosticsResult,
    max_bytes: usize,
    deferred_results: &std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
    scope: &str,
) -> Result<(), String> {
    defer_oversized_diagnostic_related_information(result, max_bytes, deferred_results, scope)?;
    if serde_json::to_vec(result).map_or(usize::MAX, |encoded| encoded.len()) <= max_bytes {
        return Ok(());
    }
    for diagnostic in &mut result.diagnostics {
        if diagnostic.message.len() > 1_024 {
            let message = std::mem::take(&mut diagnostic.message);
            diagnostic.context.message_resource = Some(store_diagnostic_payload(
                serde_json::Value::String(message),
                "diagnostic_message",
                deferred_results,
                scope,
            )?);
            "[diagnostic message deferred]".clone_into(&mut diagnostic.message);
        }
        if diagnostic
            .context
            .data
            .as_ref()
            .and_then(|data| serde_json::to_vec(data).ok())
            .is_some_and(|encoded| encoded.len() > 1_024)
        {
            let Some(data) = diagnostic.context.data.take() else {
                continue;
            };
            diagnostic.context.data_resource = Some(store_diagnostic_payload(
                data,
                "diagnostic_data",
                deferred_results,
                scope,
            )?);
        }
        if serde_json::to_vec(&diagnostic.context.fix_handles)
            .is_ok_and(|encoded| encoded.len() > 1_024)
        {
            let fix_handles = std::mem::take(&mut diagnostic.context.fix_handles);
            diagnostic.context.fix_handles_resource = Some(store_diagnostic_payload(
                serde_json::to_value(fix_handles)
                    .map_err(|error| format!("failed to store diagnostic fix handles: {error}"))?,
                "diagnostic_fix_handles",
                deferred_results,
                scope,
            )?);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn bounded_diagnostics_page(
    mut state: DiagnosticsPageState,
    max_items: usize,
    max_bytes: usize,
) -> Result<(DiagnosticsResult, Option<DiagnosticsPageState>), String> {
    let source_truncated = state.result.truncated;
    let preserve_locations = state.result.filters.preserve_locations;
    let mut remaining = VecDeque::from(std::mem::take(&mut state.result.diagnostics));
    let mut page = DiagnosticsResult {
        diagnostics: Vec::new(),
        source_resource: state.result.source_resource.clone(),
        total_diagnostics: state.result.total_diagnostics,
        returned_diagnostics: 0,
        remaining_diagnostics: state.result.total_diagnostics,
        total_groups: state.result.total_groups,
        returned_groups: 0,
        omitted_groups: state.result.total_groups,
        remaining_groups: state.result.total_groups,
        next_cursor: None,
        snapshot_identity: state.result.snapshot_identity.clone(),
        max_bytes: Some(max_bytes),
        truncated: source_truncated,
        filters: state.result.filters.clone(),
        cache: state.result.cache.clone(),
    };

    while page.diagnostics.len() < max_items {
        let Some(mut diagnostic) = remaining.pop_front() else {
            break;
        };

        if !preserve_locations {
            let represented = diagnostic.context.occurrence_count;
            let mut candidate = page.clone();
            candidate.diagnostics.push(diagnostic.clone());
            candidate.returned_diagnostics += represented;
            set_diagnostics_page_metadata(
                &mut candidate,
                remaining_diagnostic_occurrences(&remaining, false),
                remaining.len(),
                source_truncated,
            );
            if serde_json::to_vec(&candidate).map_or(usize::MAX, |encoded| encoded.len())
                > max_bytes
            {
                remaining.push_front(diagnostic);
                if page.diagnostics.is_empty() {
                    return Err(
                        "byte_limit is too small to return one diagnostic group identity"
                            .to_owned(),
                    );
                }
                break;
            }
            page = candidate;
            continue;
        }

        let offset = diagnostic.context.occurrence_offset;
        let mut occurrences = VecDeque::from(std::mem::take(&mut diagnostic.context.occurrences));
        let mut page_group = diagnostic.clone();
        page_group.context.occurrences.clear();
        let page_before_group = page.clone();
        let mut accepted = 0;

        while let Some(occurrence) = occurrences.pop_front() {
            let mut candidate_group = page_group.clone();
            candidate_group.context.occurrences.push(occurrence.clone());
            let mut candidate = page_before_group.clone();
            candidate.diagnostics.push(candidate_group.clone());
            candidate.returned_diagnostics =
                page_before_group.returned_diagnostics + candidate_group.context.occurrences.len();

            let remaining_occurrences =
                occurrences.len() + remaining_diagnostic_occurrences(&remaining, true);
            let remaining_groups = remaining.len() + usize::from(!occurrences.is_empty());
            set_diagnostics_page_metadata(
                &mut candidate,
                remaining_occurrences,
                remaining_groups,
                source_truncated,
            );
            if serde_json::to_vec(&candidate).map_or(usize::MAX, |encoded| encoded.len())
                > max_bytes
            {
                occurrences.push_front(occurrence);
                break;
            }

            accepted += 1;
            page_group = candidate_group;
            page = candidate;
        }

        if accepted == 0 {
            diagnostic.context.occurrences = occurrences.into();
            remaining.push_front(diagnostic);
            if page.diagnostics.is_empty() {
                return Err(
                    "byte_limit is too small to return one diagnostic occurrence identity"
                        .to_owned(),
                );
            }
            break;
        }

        if !occurrences.is_empty() {
            diagnostic.context.occurrence_offset = offset + accepted;
            diagnostic.context.occurrences = occurrences.into();
            remaining.push_front(diagnostic);
            break;
        }
    }

    finish_diagnostics_page_metadata(&mut page, &remaining, source_truncated);
    let continuation = (!remaining.is_empty()).then(|| {
        state.result.diagnostics = remaining.into();
        state
    });
    Ok((page, continuation))
}

pub(super) fn flatten_document_symbols(
    symbols: Vec<crate::bridge::Symbol>,
) -> Vec<crate::bridge::Symbol> {
    pub(super) fn flatten(symbol: crate::bridge::Symbol, output: &mut Vec<crate::bridge::Symbol>) {
        let mut symbol = symbol;
        let children = symbol.children.take().unwrap_or_default();
        output.push(symbol);
        for child in children {
            flatten(child, output);
        }
    }

    let mut output = Vec::new();
    for symbol in symbols {
        flatten(symbol, &mut output);
    }
    output
}

pub(super) fn clear_document_symbol_sources(symbols: &mut [crate::bridge::Symbol]) {
    for symbol in symbols {
        symbol.source = None;
        if let Some(children) = &mut symbol.children {
            clear_document_symbol_sources(children);
        }
    }
}

pub(super) const fn document_symbol_matches(
    symbol: &crate::bridge::Symbol,
    has_query: bool,
) -> bool {
    !has_query || symbol.match_class.is_some()
}

pub(super) fn bounded_document_symbol_page(
    state: DocumentSymbolPageState,
    max_items: usize,
    max_bytes: usize,
) -> Result<(DocumentSymbolsResult, Option<DocumentSymbolPageState>), String> {
    const CURSOR_PLACEHOLDER: &str = "mcpls-deferred:///00000000-0000-0000-0000-000000000000";

    let DocumentSymbolPageState {
        total,
        snapshot_identity,
        document_version,
        project_relative_path,
        source_resource,
        filters,
        symbols,
    } = state;
    let has_query = filters.query.is_some();
    let mut remaining = VecDeque::from(symbols);
    let mut result = DocumentSymbolsResult {
        symbols: Vec::new(),
        project_relative_path: project_relative_path.clone(),
        source_resource: Some(source_resource.clone()),
        total,
        returned: 0,
        remaining: total,
        next_cursor: None,
        snapshot_identity: Some(snapshot_identity.clone()),
        document_version,
        max_bytes: Some(max_bytes),
        truncated: false,
        filters: filters.clone(),
    };

    while let Some(symbol) = remaining.pop_front() {
        let matched = document_symbol_matches(&symbol, has_query);
        if matched && result.returned >= max_items {
            remaining.push_front(symbol);
            break;
        }
        result.symbols.push(symbol);
        result.returned += usize::from(matched);
        result.remaining = remaining
            .iter()
            .filter(|symbol| document_symbol_matches(symbol, has_query))
            .count();
        result.truncated = !remaining.is_empty();
        result.next_cursor = result.truncated.then(|| CURSOR_PLACEHOLDER.to_owned());
        if serde_json::to_vec(&result).map_or(usize::MAX, |encoded| encoded.len()) <= max_bytes {
            continue;
        }

        let Some(symbol) = result.symbols.pop() else {
            return Err("document-symbol page lost its inserted symbol".to_owned());
        };
        result.returned -= usize::from(matched);
        remaining.push_front(symbol);
        if result.symbols.is_empty() {
            return Err("max_bytes is too small to return one document symbol identity".to_owned());
        }
        break;
    }

    result.remaining = remaining
        .iter()
        .filter(|symbol| document_symbol_matches(symbol, has_query))
        .count();
    result.truncated = !remaining.is_empty();
    result.next_cursor = result.truncated.then(|| CURSOR_PLACEHOLDER.to_owned());
    let remaining = (!remaining.is_empty()).then(|| DocumentSymbolPageState {
        total,
        snapshot_identity,
        document_version,
        project_relative_path,
        source_resource,
        filters,
        symbols: remaining.into(),
    });
    Ok((result, remaining))
}

pub(super) fn workspace_symbol_snapshot_identity(
    symbols: &[WorkspaceSymbol],
) -> Result<String, String> {
    let encoded = serde_json::to_vec(symbols)
        .map_err(|error| format!("failed to identify workspace-symbol snapshot: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

pub(super) fn bounded_workspace_symbol_page(
    state: WorkspaceSymbolPageState,
    max_items: usize,
    max_bytes: usize,
) -> Result<(WorkspaceSymbolResult, Option<WorkspaceSymbolPageState>), String> {
    const CURSOR_PLACEHOLDER: &str = "mcpls-deferred:///00000000-0000-0000-0000-000000000000";

    let total = state.total;
    let snapshot_identity = state.snapshot_identity;
    let mut symbols = state.symbols;
    let mut remaining = VecDeque::from(symbols.split_off(max_items.min(symbols.len())));
    let mut result = WorkspaceSymbolResult {
        symbols,
        total,
        returned: 0,
        remaining: 0,
        next_cursor: None,
        snapshot_identity: Some(snapshot_identity.clone()),
        max_bytes: Some(max_bytes),
        truncated: false,
    };

    loop {
        result.returned = result.symbols.len();
        result.remaining = remaining.len();
        result.truncated = !remaining.is_empty();
        result.next_cursor = result.truncated.then(|| CURSOR_PLACEHOLDER.to_owned());
        if serde_json::to_vec(&result).map_or(usize::MAX, |encoded| encoded.len()) <= max_bytes {
            break;
        }
        let Some(symbol) = result.symbols.pop() else {
            return Err(
                "max_bytes is too small to return one workspace symbol identity".to_owned(),
            );
        };
        remaining.push_front(symbol);
    }

    let remaining = (!remaining.is_empty()).then(|| WorkspaceSymbolPageState {
        total,
        snapshot_identity,
        symbols: remaining.into(),
    });
    Ok((result, remaining))
}

impl ProjectRuntime {
    #[cfg(test)]
    pub(super) fn new(translator: Translator) -> Self {
        Self::with_edit_safety(translator, None)
    }

    #[cfg(test)]
    pub(super) fn with_edit_safety(
        translator: Translator,
        edit_safety: Option<EditSafetyConfig>,
    ) -> Self {
        Self::with_deferred_results_scoped(
            translator,
            edit_safety,
            std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new())),
            None,
        )
    }

    pub(super) fn with_deferred_results_scoped(
        translator: Translator,
        edit_safety: Option<EditSafetyConfig>,
        deferred_results: std::sync::Arc<std::sync::Mutex<DeferredResultStore>>,
        deferred_scope: Option<String>,
    ) -> Self {
        Self {
            translator,
            edit_plans: EditPlanStore::for_project(),
            edit_safety,
            code_actions: CodeActionStore::new(),
            symbol_handles: std::sync::Mutex::new(SymbolHandleStore::new()),
            workspace_symbol_results: std::sync::Mutex::new(HashMap::new()),
            inspect_symbol_batch_pages: std::sync::Mutex::new(InspectSymbolBatchPageStore::new()),
            deferred_results,
            deferred_scope,
            inline_module_checks: HashMap::new(),
            applied_edit_receipts: VecDeque::new(),
            applied_edit_receipt_bytes: 0,
            edit_conflicts: VecDeque::new(),
            active_edit_workers: 0,
            activation_health: ActivationHealth::Ready,
            activation_started_at: None,
            generation: 0,
            automatic_restart: AutomaticRestartPolicy::default(),
        }
    }

    pub(super) const fn record_activation(&mut self, health: ActivationHealth) {
        self.activation_health = health;
    }

    pub(super) fn readiness_status(&self) -> ProjectStatus {
        activation_status(self.activation_health, self.translator.is_initializing())
    }

    pub(super) const fn begin_transition(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub(super) fn begin_activation(&mut self) {
        self.begin_transition();
        self.activation_started_at = Some(Instant::now());
    }

    pub(super) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) const fn owns_generation(&self, generation: u64) -> bool {
        self.generation == generation
    }

    pub(super) fn source_handle(
        &self,
        source: &SourceContext,
        line: u32,
        character: u32,
    ) -> Option<SymbolHandle> {
        let SourceContext::Available(frame) = source else {
            return None;
        };
        let snapshot = frame.document_version.map_or_else(
            || SourceSnapshot::Hash(frame.content_hash.clone()),
            SourceSnapshot::Version,
        );
        Some(
            self.symbol_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(StoredSymbolTarget::new(
                    PathBuf::from(&frame.path),
                    line,
                    character,
                    snapshot,
                )),
        )
    }

    pub(super) fn attach_location_handle(&self, location: &mut crate::bridge::Location) {
        location.symbol_handle = self.source_handle(
            &location.source,
            location.range.start.line,
            location.range.start.character,
        );
    }

    pub(super) fn attach_location_handles<'a>(
        &self,
        locations: impl IntoIterator<Item = &'a mut crate::bridge::Location>,
    ) {
        locations
            .into_iter()
            .for_each(|location| self.attach_location_handle(location));
    }

    pub(super) async fn attach_workspace_symbol_handle(
        &self,
        symbol: &mut WorkspaceSymbol,
        snapshots: &mut HashMap<PathBuf, (Option<i32>, String, String)>,
    ) {
        let location = &mut symbol.location;
        let Some(path) = location.path.as_deref().map(PathBuf::from) else {
            return;
        };
        let (line, character, snapshot) = match &location.source {
            SourceContext::Available(frame) => {
                let position =
                    rendered_workspace_symbol_position(&frame.text, &symbol.name, &location.range)
                        .unwrap_or((location.range.start.line, location.range.start.character));
                let snapshot = frame.document_version.map_or_else(
                    || SourceSnapshot::Hash(frame.content_hash.clone()),
                    SourceSnapshot::Version,
                );
                (position.0, position.1, snapshot)
            }
            SourceContext::Deferred { resource } => {
                let snapshot = resource.document_version.map_or_else(
                    || SourceSnapshot::Hash(resource.snapshot_hash.clone()),
                    SourceSnapshot::Version,
                );
                let position = self
                    .workspace_symbol_position(&path, &symbol.name, &location.range, snapshots)
                    .await
                    .unwrap_or((location.range.start.line, location.range.start.character));
                (position.0, position.1, snapshot)
            }
            SourceContext::Unavailable {
                reason: SourceUnavailableReason::ResponseBudgetExhausted,
            } => {
                let Some((document_version, content_hash, source)) =
                    self.workspace_symbol_snapshot(&path, snapshots).await
                else {
                    return;
                };
                let position = source_symbol_position(source, &symbol.name, &location.range)
                    .unwrap_or((location.range.start.line, location.range.start.character));
                let snapshot = document_version.map_or_else(
                    || SourceSnapshot::Hash(content_hash.to_owned()),
                    SourceSnapshot::Version,
                );
                (position.0, position.1, snapshot)
            }
            SourceContext::Unavailable { .. } => return,
        };
        location.symbol_handle = Some(
            self.symbol_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(StoredSymbolTarget::new(path, line, character, snapshot)),
        );
    }

    pub(super) async fn attach_workspace_symbol_handles<'a>(
        &self,
        symbols: impl IntoIterator<Item = &'a mut WorkspaceSymbol>,
    ) {
        let mut snapshots = HashMap::new();
        for symbol in symbols {
            self.attach_workspace_symbol_handle(symbol, &mut snapshots)
                .await;
        }
    }

    pub(super) async fn workspace_symbol_position(
        &self,
        path: &Path,
        name: &str,
        range: &crate::bridge::Range,
        snapshots: &mut HashMap<PathBuf, (Option<i32>, String, String)>,
    ) -> Option<(u32, u32)> {
        let (_, _, source) = self.workspace_symbol_snapshot(path, snapshots).await?;
        source_symbol_position(source, name, range)
    }

    pub(super) async fn workspace_symbol_snapshot<'a>(
        &self,
        path: &Path,
        snapshots: &'a mut HashMap<PathBuf, (Option<i32>, String, String)>,
    ) -> Option<&'a (Option<i32>, String, String)> {
        if !snapshots.contains_key(path) {
            let (_, version, hash, source) = self.translator.source_snapshot(path).await.ok()?;
            snapshots.insert(path.to_path_buf(), (version, hash, source));
        }
        snapshots.get(path)
    }

    pub(super) fn attach_reference_handle(&self, reference: &mut crate::bridge::ReferenceUse) {
        reference.symbol_handle = reference.snapshot.as_ref().map(|snapshot| {
            let source_snapshot = snapshot.document_version.map_or_else(
                || SourceSnapshot::Hash(snapshot.content_hash.clone()),
                SourceSnapshot::Version,
            );
            self.symbol_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(StoredSymbolTarget::new(
                    PathBuf::from(&snapshot.path),
                    reference.range[0],
                    reference.range[1],
                    source_snapshot,
                ))
        });
    }

    pub(super) async fn resolve_symbol_target(
        &self,
        handle: &SymbolHandle,
    ) -> Result<StoredSymbolTarget, String> {
        let target = self
            .symbol_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resolve(handle)?;
        let (_, version, hash, _) = self
            .translator
            .source_snapshot(&target.file_path)
            .await
            .map_err(|error| {
                format!(
                    "stale_symbol_handle: source is unavailable; rerun symbol discovery: {error}"
                )
            })?;
        let current = match &target.snapshot {
            SourceSnapshot::Version(expected) => version == Some(*expected),
            SourceSnapshot::Hash(expected) => hash == *expected,
        };
        current.then_some(target).ok_or_else(|| {
            "stale_symbol_handle: source changed; rerun symbol discovery to refresh the handle"
                .to_owned()
        })
    }

    pub(super) fn has_active_workspace_roots(&self, roots: &[PathBuf]) -> bool {
        self.translator.has_active_workspace_roots(roots)
    }

    pub(super) fn activation_is_reusable(&self, status: ProjectStatus, roots: &[PathBuf]) -> bool {
        match status {
            ProjectStatus::Starting | ProjectStatus::Ready => {
                self.has_active_workspace_roots(roots)
            }
            ProjectStatus::Degraded => self.translator.has_workspace_roots(roots),
            ProjectStatus::Restarting
            | ProjectStatus::Dormant
            | ProjectStatus::Stopping
            | ProjectStatus::Stopped
            | ProjectStatus::Failed => false,
        }
    }

    pub(super) fn begin_automatic_restart(&mut self) -> Option<AutomaticRestartAttempt> {
        let attempt = self.automatic_restart.next()?;
        self.begin_activation();
        Some(attempt)
    }

    pub(super) const fn reset_automatic_restart(&mut self) {
        self.automatic_restart.reset();
    }

    pub(super) fn store_edit_plan(&mut self, plan: EditPlan) -> Result<(), String> {
        self.edit_plans
            .insert(plan)
            .map_err(|error| error.to_string())
    }

    pub(super) async fn preview_edit(
        &mut self,
        project_id: &str,
        edit: WorkspaceEdit,
        encoding: PositionEncoding,
        root: &Path,
    ) -> Result<PreviewArtifact, String> {
        let boundary = WorkspaceBoundary::new(root).map_err(|error| error.to_string())?;
        let limits = PreviewLimits::default();
        let documents =
            refresh_workspace_edit_documents(&edit, self.translator.document_tracker(), limits)
                .await
                .map_err(|error| error.to_string())?;
        let artifact =
            preview_workspace_edit(&boundary, project_id, edit, encoding, &documents, limits)
                .map_err(|error| error.to_string())?;
        self.edit_plans
            .insert(artifact.plan.clone())
            .map_err(|error| error.to_string())?;
        Ok(artifact)
    }

    pub(super) async fn preview_generated_edit(
        &mut self,
        project_id: &str,
        request: GeneratedEditRequest,
        encoding: PositionEncoding,
        root: &Path,
    ) -> Result<GeneratedEditPreview, String> {
        let (supported, edit, create_empty_plan) = match request {
            GeneratedEditRequest::Rename {
                file_path,
                line,
                character,
                new_name,
            } => (
                true,
                self.rename_workspace_edit(file_path, line, character, new_name)
                    .await?,
                true,
            ),
            GeneratedEditRequest::Format {
                file_path,
                tab_size,
                insert_spaces,
            } => (
                true,
                self.format_workspace_edit(file_path, tab_size, insert_spaces)
                    .await?,
                true,
            ),
            GeneratedEditRequest::RangeFormat {
                file_path,
                start,
                end,
                tab_size,
                insert_spaces,
            } => {
                let result = self
                    .translator
                    .request_range_format_workspace_edit(
                        file_path,
                        start,
                        end,
                        tab_size,
                        insert_spaces,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                (result.supported, result.edit, false)
            }
            GeneratedEditRequest::MoveItem {
                file_path,
                start,
                end,
                direction,
            } => {
                let result = self
                    .translator
                    .request_move_item_workspace_edit(file_path, start, end, &direction)
                    .await
                    .map_err(|error| error.to_string())?;
                (result.supported, result.edit, false)
            }
        };
        let artifact = match edit.or_else(|| create_empty_plan.then(WorkspaceEdit::default)) {
            Some(edit) => Some(self.preview_edit(project_id, edit, encoding, root).await?),
            None => None,
        };
        Ok(GeneratedEditPreview {
            supported,
            artifact,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn structural_replace_preview(
        &mut self,
        project_id: &str,
        request: StructuralReplaceRequest,
        root: &Path,
    ) -> Result<StructuralPreview, String> {
        let StructuralReplaceRequest {
            file_path,
            dialect,
            query,
            replacement,
            language_id,
            parse_only,
            encoding,
        } = request;
        self.translator
            .validate_path(Path::new(&file_path))
            .map_err(|error| error.to_string())?;
        let (edit, matches, snapshots, verification, producer) = match dialect {
            StructuralDialect::RustAnalyzerSsr => {
                if replacement.is_some() || language_id.is_some() {
                    return Err(
                        "rust_analyzer_ssr accepts the complete rust-analyzer rule in query; replacement and language_id must be omitted"
                            .to_string(),
                    );
                }
                let edit = self
                    .translator
                    .request_rust_analyzer_ssr(file_path.clone(), query, parse_only)
                    .await
                    .map_err(|error| error.to_string())?;
                let matches = if parse_only {
                    Vec::new()
                } else {
                    structural_matches_from_workspace_edit(&edit)?
                };
                (
                    (!parse_only && !matches.is_empty()).then_some(edit),
                    matches,
                    Vec::new(),
                    VerificationStatus::SemanticVerified,
                    EditProducer::RustAnalyzer,
                )
            }
            StructuralDialect::AstGrep => {
                let language = language_id.ok_or_else(|| {
                    "ast_grep requires an explicit language_id; syntax is never inferred or translated"
                        .to_string()
                })?;
                let StructuralSearchResult {
                    edit,
                    matches,
                    snapshots,
                } = self
                    .translator
                    .structural_ast_grep_search(
                        root.to_path_buf(),
                        language,
                        query,
                        replacement,
                        encoding,
                        parse_only,
                    )
                    .await?;
                (
                    edit.filter(|_| !matches.is_empty()),
                    matches,
                    snapshots,
                    VerificationStatus::StructuralUnverified,
                    EditProducer::StructuralAstGrep,
                )
            }
        };
        let scanned_files = self.validate_structural_snapshots(&snapshots).await?;
        let artifact = match edit {
            Some(edit) => {
                let mut artifact = self.preview_edit(project_id, edit, encoding, root).await?;
                artifact.verification = Some(verification);
                artifact.producer = Some(producer);
                Some(artifact)
            }
            None => None,
        };
        let matched_files = artifact.as_ref().map_or(scanned_files, |artifact| {
            let matched_paths = matches
                .iter()
                .map(|matched| matched.path.as_path())
                .collect::<HashSet<_>>();
            artifact
                .plan
                .files()
                .iter()
                .filter(|file| matched_paths.contains(file.path().as_path()))
                .map(|file| StructuralMatchedFile {
                    path: file.path().clone(),
                    content_hash: file.content_hash().to_owned(),
                    document_version: file.version(),
                    total_bytes: file.original_content().len(),
                    total_lines: u32::try_from(file.original_content().lines().count().max(1))
                        .unwrap_or(u32::MAX),
                })
                .collect()
        });
        Ok(StructuralPreview {
            artifact,
            dialect,
            matches,
            matched_files,
            parse_only,
        })
    }

    pub(super) async fn validate_structural_snapshots(
        &self,
        snapshots: &[StructuralFileSnapshot],
    ) -> Result<Vec<StructuralMatchedFile>, String> {
        let mut matched_files = Vec::with_capacity(snapshots.len());
        for expected in snapshots {
            let (path, document_version, content_hash, content) = self
                .translator
                .source_snapshot(&expected.path)
                .await
                .map_err(|error| error.to_string())?;
            if content_hash != expected.content_hash {
                return Err(format!(
                    "stale_resource: {} changed during structural search; rerun the preview",
                    path.display()
                ));
            }
            matched_files.push(StructuralMatchedFile {
                path,
                content_hash,
                document_version,
                total_bytes: content.len(),
                total_lines: u32::try_from(content.lines().count().max(1)).unwrap_or(u32::MAX),
            });
        }
        Ok(matched_files)
    }

    pub(super) async fn path_rename_preview(
        &mut self,
        project_id: &str,
        request: PathRenameRequest,
        root: &Path,
    ) -> Result<PathRenamePreview, String> {
        let boundary = WorkspaceBoundary::new(root).map_err(|error| error.to_string())?;
        let old_path = boundary
            .validate_existing(&request.old_path)
            .map_err(|error| error.to_string())?;
        let new_path = boundary
            .validate_target(&request.new_path)
            .map_err(|error| error.to_string())?;
        if old_path == boundary.root() {
            return Err("cannot rename the project root".to_string());
        }
        if old_path == new_path {
            return Err("old_path and new_path resolve to the same path".to_string());
        }
        if old_path.is_dir() && new_path.starts_with(&old_path) {
            return Err("cannot rename a directory into itself".to_string());
        }
        boundary
            .validate_operation(&crate::edit_paths::FileOperation::Rename {
                from: old_path.clone(),
                to: new_path.clone(),
                overwrite: false,
            })
            .map_err(|error| error.to_string())?;

        let result = self
            .translator
            .request_will_rename_files(&old_path, &new_path)
            .await
            .map_err(|error| error.to_string())?;
        let (edit, providers, semantic_edit_count) =
            compose_path_rename_edit(result, &old_path, &new_path)?;
        let mut artifact = self
            .preview_edit(project_id, edit, request.encoding, root)
            .await?;
        artifact.verification = Some(if semantic_edit_count > 0 {
            VerificationStatus::SemanticVerified
        } else {
            VerificationStatus::StructuralUnverified
        });
        if !providers.is_empty() {
            artifact.producer = Some(EditProducer::LanguageServerFileOperations);
        }
        Ok(PathRenamePreview {
            artifact,
            providers,
            semantic_edit_count,
        })
    }

    pub(super) async fn verify_inline_module_before_preview(
        &self,
        source_path: &Path,
        module_name: &str,
        module_position: Option<lsp_types::Position>,
    ) -> Result<(VerificationStatus, Option<lsp_types::Position>), String> {
        if !self.translator.semantic_server_ready_for_file(source_path) {
            return Ok((VerificationStatus::StructuralUnverified, module_position));
        }
        let Ok(symbols) = self
            .translator
            .handle_document_symbols(
                source_path.display().to_string(),
                DocumentSymbolOptions::internal_tree(),
            )
            .await
        else {
            return Ok((VerificationStatus::StructuralUnverified, module_position));
        };
        let matches = symbols
            .symbols
            .iter()
            .filter(|symbol| {
                logical_module_name(&symbol.name) == logical_module_name(module_name)
                    && symbol.kind.eq_ignore_ascii_case("Module")
                    && module_position.is_none_or(|position| {
                        let line = position.line.saturating_add(1);
                        let character = position.character.saturating_add(1);
                        position_in_mcp_range(line, character, &symbol.range)
                    })
            })
            .count();
        if matches != 1 {
            return Err(format!(
                "rust-analyzer did not identify exactly one module `{module_name}` at the requested location"
            ));
        }
        let selected_position = module_position.or_else(|| {
            symbols
                .symbols
                .iter()
                .find(|symbol| {
                    logical_module_name(&symbol.name) == logical_module_name(module_name)
                        && symbol.kind.eq_ignore_ascii_case("Module")
                })
                .map(|symbol| lsp_types::Position {
                    line: symbol.range.start.line.saturating_sub(1),
                    character: symbol.range.start.character.saturating_sub(1),
                })
        });
        Ok((VerificationStatus::SemanticVerified, selected_position))
    }

    pub(super) async fn move_inline_module_preview(
        &mut self,
        project_id: &str,
        file_path: &str,
        module_name: &str,
        module_position: Option<lsp_types::Position>,
        encoding: PositionEncoding,
        root: &Path,
    ) -> Result<PreviewArtifact, String> {
        let source_path = self
            .translator
            .validate_path(Path::new(file_path))
            .map_err(|error| error.to_string())?;
        let source_override = self
            .translator
            .document_tracker()
            .reconciled_snapshot(&source_path)
            .await
            .map_err(|error| error.to_string())?
            .map(|document| document.content().to_string());
        let (verification, verified_position) = self
            .verify_inline_module_before_preview(&source_path, module_name, module_position)
            .await?;
        let structural_edit = move_inline_module_preview_with_source(
            &source_path,
            module_name,
            encoding,
            source_override.as_deref(),
            module_position,
        )
        .map_err(|error| error.to_string())?;
        let native_edit = if verification == VerificationStatus::SemanticVerified {
            match verified_position {
                Some(position) => {
                    self.native_inline_module_move_edit(&source_path, position)
                        .await
                }
                None => None,
            }
        } else {
            None
        };
        let (edit, producer) = native_edit
            .map_or((structural_edit, EditProducer::StructuralAstGrep), |edit| {
                (edit, EditProducer::RustAnalyzer)
            });
        let mut artifact = self.preview_edit(project_id, edit, encoding, root).await?;
        artifact.verification = Some(verification);
        artifact.producer = Some(producer);
        if let Some(destination_path) = artifact
            .plan
            .files()
            .iter()
            .find(|file| file.was_created())
            .map(|file| file.path().clone())
        {
            if self.inline_module_checks.len() >= MAX_INLINE_MODULE_CHECKS
                && let Some(oldest) = self.inline_module_checks.keys().next().cloned()
            {
                self.inline_module_checks.remove(&oldest);
            }
            let source_position = verified_position.unwrap_or(lsp_types::Position {
                line: 0,
                character: 0,
            });
            self.inline_module_checks.insert(
                artifact.plan.id().clone(),
                InlineModuleSemanticCheck {
                    source_path,
                    destination_path,
                    module_name: module_name.to_string(),
                    source_position,
                    pre_verification: verification,
                },
            );
        }
        Ok(artifact)
    }

    pub(super) async fn native_inline_module_move_edit(
        &self,
        source_path: &Path,
        position: lsp_types::Position,
    ) -> Option<WorkspaceEdit> {
        let line = position.line.saturating_add(1);
        let character = position.character.saturating_add(1);
        let actions = self
            .translator
            .request_code_actions(
                source_path.display().to_string(),
                line,
                character,
                line,
                character,
                Some("refactor.extract".to_string()),
            )
            .await
            .ok()?;
        let mut action = take_code_action_by_assist_id(actions, "move_module_to_file")?;
        if action.disabled.is_some() || action.command.is_some() {
            return None;
        }
        if action.edit.is_none() {
            action = self
                .translator
                .resolve_code_action(&source_path.display().to_string(), action)
                .await
                .ok()?;
        }
        if action.disabled.is_some() || action.command.is_some() {
            return None;
        }
        action.edit
    }

    pub(super) fn take_edit_plan(
        &mut self,
        plan_id: &PlanId,
        project_id: &str,
    ) -> Result<EditPlan, String> {
        self.edit_plans
            .take_for_project(plan_id, project_id)
            .map_err(|error| error.to_string())
    }

    pub(super) fn inspect_edit_plan(
        &self,
        plan_id: &PlanId,
        project_id: &str,
    ) -> Result<crate::edit_plan::EditPlanApprovalSummary, String> {
        self.edit_plans
            .get_for_project(plan_id, project_id)
            .map(EditPlan::approval_summary)
            .map_err(|error| error.to_string())
    }

    pub(super) fn read_edit_plan_diff(
        &self,
        plan_id: &PlanId,
        project_id: &str,
    ) -> Result<String, String> {
        self.edit_plans
            .get_for_project(plan_id, project_id)
            .map(EditPlan::complete_unified_diff)
            .map_err(|error| error.to_string())
    }

    pub(super) fn read_applied_edit_detail(
        &self,
        plan_id: &PlanId,
        project_id: &str,
    ) -> Result<String, String> {
        self.applied_edit_receipts
            .iter()
            .find(|receipt| &receipt.plan_id == plan_id)
            .map(|receipt| serde_json::to_string(&receipt.detail_json()))
            .transpose()
            .map_err(|serialization| serialization.to_string())?
            .ok_or_else(|| {
                format!("applied edit result for project {project_id} is no longer retained")
            })
    }

    pub(super) fn has_edit_plan_receipt_or_conflict(&self, plan_id: &PlanId) -> bool {
        self.applied_edit_receipts
            .iter()
            .any(|receipt| &receipt.plan_id == plan_id)
            || self
                .edit_conflicts
                .iter()
                .any(|conflict| &conflict.plan_id == plan_id)
    }

    pub(super) fn configure_edit_safety(
        &mut self,
        boundary: &WorkspaceBoundary,
    ) -> Result<Option<BackupPolicy>, String> {
        let Some(safety) = self.edit_safety.as_ref() else {
            return Ok(None);
        };
        if let Some(audit) = &safety.audit_log {
            let path = resolve_edit_safety_path(boundary, &audit.path);
            boundary
                .validate_target(&path)
                .map_err(|error| format!("invalid audit log path {}: {error}", path.display()))?;
            let policy = AuditLogPolicy::new(&path, audit.max_bytes, audit.failure_mode)
                .map_err(|error| error.to_string())?;
            self.edit_plans.set_audit_log(policy);
        }
        safety
            .backup
            .as_ref()
            .map(|backup| {
                BackupPolicy::new(
                    boundary,
                    &backup.root,
                    backup.max_archives,
                    backup.max_bytes,
                    backup.failure_mode,
                )
                .map_err(|error| error.to_string())
            })
            .transpose()
    }

    pub(super) fn record_edit_failure(&mut self, audit: EditAuditRecord, error: String) -> String {
        let _ = self
            .edit_plans
            .record_audit_with_policy(audit.failed(error.clone(), false));
        error
    }

    pub(super) fn remember_edit_conflict(&mut self, conflict: EditConflict) -> EditConflict {
        self.edit_conflicts
            .retain(|existing| existing.plan_id != conflict.plan_id);
        if self.edit_conflicts.len() >= MAX_APPLIED_EDIT_RECEIPTS {
            self.edit_conflicts.pop_front();
        }
        self.edit_conflicts.push_back(conflict.clone());
        conflict
    }

    /// Apply a plan directly for the runtime-only test and embedding path.
    ///
    /// The project actor uses the same preparation and commit functions, but
    /// runs the filesystem phase on its bounded blocking worker. Keeping this
    /// small adapter preserves the runtime API without reintroducing the
    /// actor-wide mutation lock into production requests.
    #[cfg(test)]
    pub(super) async fn apply_edit_plan_with_context(
        &mut self,
        plan_id: &PlanId,
        project_id: &str,
        root: &Path,
        session_id: Option<String>,
        principal: Option<String>,
    ) -> Result<AppliedEditPlan, String> {
        let lease = EditCoordinator::new()
            .try_acquire(plan_id.as_str(), Vec::new())
            .map_err(|contention| format!("edit plan is busy: {contention:?}"))?;
        let prepared = self.prepare_edit_plan_with_context(
            plan_id, project_id, root, session_id, principal, lease,
        )?;
        let prepared = match prepared {
            PreparedEditResult::AlreadyApplied(applied) => return Ok(applied),
            PreparedEditResult::AlreadyConflicted(conflict) => return Err(conflict.reason),
            PreparedEditResult::Ready(prepared) => *prepared,
        };
        let apply_result = match prepared.backup_policy.as_ref() {
            Some(policy) => apply_plan_with_documents_and_backup(
                &prepared.boundary,
                &prepared.plan,
                &prepared.documents,
                policy,
            ),
            None => {
                apply_plan_with_documents(&prepared.boundary, &prepared.plan, &prepared.documents)
            }
        };
        match self.finish_prepared_edit(prepared, apply_result).await? {
            ApplyEditPlanOutcome::Applied(applied) => Ok(applied),
            ApplyEditPlanOutcome::Conflict(conflict) => Err(conflict.reason),
            ApplyEditPlanOutcome::NotReady(_) => Err("edit plan was not ready".to_owned()),
        }
    }

    pub(super) async fn verify_inline_module_after_apply(
        &mut self,
        check: &InlineModuleSemanticCheck,
    ) -> VerificationStatus {
        if check.pre_verification != VerificationStatus::SemanticVerified {
            return check.pre_verification;
        }
        let source_symbols = self
            .translator
            .handle_document_symbols(
                check.source_path.display().to_string(),
                DocumentSymbolOptions::internal_tree(),
            )
            .await;
        let destination_symbols = self
            .translator
            .handle_document_symbols(
                check.destination_path.display().to_string(),
                DocumentSymbolOptions::internal_tree(),
            )
            .await;
        let source_diagnostics = self
            .translator
            .handle_actor_diagnostics(
                check.source_path.display().to_string(),
                DiagnosticOptions::default(),
            )
            .await;
        let destination_diagnostics = self
            .translator
            .handle_actor_diagnostics(
                check.destination_path.display().to_string(),
                DiagnosticOptions::default(),
            )
            .await;
        let references = self
            .translator
            .handle_references(
                check.source_path.display().to_string(),
                check.source_position.line.saturating_add(1),
                check.source_position.character.saturating_add(1),
                true,
                SemanticResultLimits::default(),
            )
            .await;
        let source_module_present = source_symbols.is_ok_and(|result| {
            result.symbols.iter().any(|symbol| {
                logical_module_name(&symbol.name) == logical_module_name(&check.module_name)
                    && symbol.kind.eq_ignore_ascii_case("Module")
            })
        });
        if source_module_present
            && destination_symbols.is_ok()
            && source_diagnostics.is_ok()
            && destination_diagnostics.is_ok_and(|result| diagnostics_are_error_free(&result))
            && references.is_ok()
        {
            VerificationStatus::SemanticVerified
        } else {
            VerificationStatus::SemanticPostcheckFailed
        }
    }

    pub(super) fn prepare_edit_plan_with_context(
        &mut self,
        plan_id: &PlanId,
        project_id: &str,
        root: &Path,
        session_id: Option<String>,
        principal: Option<String>,
        lease: EditLease,
    ) -> Result<PreparedEditResult, String> {
        if let Some(applied) = self
            .applied_edit_receipts
            .iter()
            .find(|applied| &applied.plan_id == plan_id)
        {
            return Ok(PreparedEditResult::AlreadyApplied(applied.clone()));
        }
        if let Some(conflict) = self
            .edit_conflicts
            .iter()
            .find(|conflict| &conflict.plan_id == plan_id)
        {
            return Ok(PreparedEditResult::AlreadyConflicted(conflict.clone()));
        }
        let workspace_root = self
            .edit_plans
            .get_for_project(plan_id, project_id)
            .map_err(|error| error.to_string())?
            .workspace_root()
            .map_or_else(|| root.to_path_buf(), Path::to_path_buf);
        let boundary = WorkspaceBoundary::new(workspace_root).map_err(|error| error.to_string())?;
        let backup_policy = self.configure_edit_safety(&boundary)?;
        let plan = self
            .edit_plans
            .take_for_project(plan_id, project_id)
            .map_err(|error| error.to_string())?;
        let semantic_check = self.inline_module_checks.remove(plan_id);
        let resource_operations = plan.file_operations().to_vec();
        let text_changes = planned_text_changes(&plan);
        let open_documents = plan
            .open_document_snapshots()
            .map(|snapshot| {
                (
                    snapshot.path().clone(),
                    snapshot.version().unwrap_or_default(),
                    snapshot.planned_content().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let audit = EditAuditRecord::for_plan_with_context(&plan, session_id, principal);
        Ok(PreparedEditResult::Ready(Box::new(PreparedEditPlan {
            plan,
            boundary,
            backup_policy,
            semantic_check,
            resource_operations,
            text_changes,
            open_documents,
            audit,
            documents: std::sync::Arc::clone(self.translator.document_tracker()),
            lease,
        })))
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn finish_prepared_edit(
        &mut self,
        prepared: PreparedEditPlan,
        apply_result: Result<ApplyReport, ApplyError>,
    ) -> Result<ApplyEditPlanOutcome, String> {
        let PreparedEditPlan {
            plan,
            boundary: _boundary,
            backup_policy: _backup_policy,
            semantic_check,
            resource_operations,
            text_changes,
            open_documents,
            audit,
            documents: _documents,
            lease,
        } = prepared;
        let ApplyReport { committed_files } = match apply_result {
            Ok(report) => report,
            Err(ApplyError::Stale(_)) => {
                let changed_paths = plan
                    .files()
                    .iter()
                    .map(|snapshot| snapshot.path().clone())
                    .collect();
                let conflict = self.remember_edit_conflict(EditConflict {
                    plan_id: plan.id().clone(),
                    changed_paths,
                    reason: "snapshot_changed".to_owned(),
                });
                drop(lease);
                return Ok(ApplyEditPlanOutcome::Conflict(conflict));
            }
            Err(ApplyError::TopologyChanged { .. }) => {
                let changed_paths = plan
                    .files()
                    .iter()
                    .map(|snapshot| snapshot.path().clone())
                    .collect();
                let conflict = self.remember_edit_conflict(EditConflict {
                    plan_id: plan.id().clone(),
                    changed_paths,
                    reason: "snapshot_changed".to_owned(),
                });
                drop(lease);
                return Ok(ApplyEditPlanOutcome::Conflict(conflict));
            }
            Err(ApplyError::Operation(OperationValidationError::DestinationExists(path))) => {
                let conflict = self.remember_edit_conflict(EditConflict {
                    plan_id: plan.id().clone(),
                    changed_paths: vec![path],
                    reason: "snapshot_changed".to_owned(),
                });
                drop(lease);
                return Ok(ApplyEditPlanOutcome::Conflict(conflict));
            }
            Err(error) => {
                return Err(self.record_edit_failure(audit, error.to_string()));
            }
        };
        let audit_failure = self
            .edit_plans
            .record_audit_with_policy(audit.clone().committed(committed_files.clone()))
            .err()
            .map(|error| error.to_string());
        if audit_failure.is_some() {
            self.edit_plans
                .record_audit(audit.committed(committed_files.clone()));
        }
        // Persist a receipt and release the path lease at the filesystem
        // commit point. Provider/LSP convergence is best-effort and must not
        // hold another editor's paths hostage.
        let mut applied = AppliedEditPlan {
            plan_id: plan.id().clone(),
            operations: plan.operations().to_vec(),
            unified_diff: plan.unified_diff().to_string(),
            complete_unified_diff: plan.complete_unified_diff(),
            committed_files,
            verification: None,
            provider_synchronization: Vec::new(),
        };
        let applied_bytes = applied.estimated_bytes();
        while self.applied_edit_receipts.len() >= MAX_APPLIED_EDIT_RECEIPTS
            || self
                .applied_edit_receipt_bytes
                .saturating_add(applied_bytes)
                > MAX_APPLIED_EDIT_RECEIPT_BYTES
        {
            let Some(evicted) = self.applied_edit_receipts.pop_front() else {
                break;
            };
            self.applied_edit_receipt_bytes = self
                .applied_edit_receipt_bytes
                .saturating_sub(evicted.estimated_bytes());
        }
        if applied_bytes <= MAX_APPLIED_EDIT_RECEIPT_BYTES {
            self.applied_edit_receipt_bytes = self
                .applied_edit_receipt_bytes
                .saturating_add(applied_bytes);
            self.applied_edit_receipts.push_back(applied.clone());
        }
        drop(lease);

        let (provider_synchronization, verification) = self
            .synchronize_applied_edit(
                &resource_operations,
                &text_changes,
                open_documents,
                semantic_check.as_ref(),
                audit_failure,
            )
            .await;
        applied.verification = verification;
        applied.provider_synchronization = provider_synchronization;
        if let Some(receipt) = self
            .applied_edit_receipts
            .iter_mut()
            .find(|receipt| receipt.plan_id == applied.plan_id)
        {
            self.applied_edit_receipt_bytes = self
                .applied_edit_receipt_bytes
                .saturating_sub(receipt.estimated_bytes());
            *receipt = applied.clone();
            self.applied_edit_receipt_bytes = self
                .applied_edit_receipt_bytes
                .saturating_add(receipt.estimated_bytes());
        }
        Ok(ApplyEditPlanOutcome::Applied(applied))
    }

    pub(super) async fn synchronize_applied_edit(
        &mut self,
        resource_operations: &[FileOperation],
        text_changes: &[(PathBuf, String)],
        open_documents: Vec<(PathBuf, i32, String)>,
        semantic_check: Option<&InlineModuleSemanticCheck>,
        audit_failure: Option<String>,
    ) -> (Vec<ProviderSynchronization>, Option<VerificationStatus>) {
        let mut document_sync_failures = Vec::new();
        let mut tracker_sync_failures = Vec::new();
        for (path, version, content) in open_documents {
            match self
                .translator
                .apply_open_document_content(&path, version, content)
                .await
            {
                Ok(failures) => {
                    document_sync_failures.extend(failures);
                    // `apply_open_document_content` records the committed
                    // text as a local edit so ordinary unsaved changes remain
                    // protected. Once the filesystem phase has also committed
                    // that same text, establish disk provenance immediately;
                    // otherwise a later formatter rewrite is indistinguishable
                    // from an unsaved edit and every preview conflicts until
                    // the project is restarted.
                    if let Err(error) = self
                        .translator
                        .document_tracker()
                        .reconciled_snapshot(&path)
                        .await
                    {
                        tracker_sync_failures.push(error.to_string());
                    }
                }
                Err(error) => tracker_sync_failures.push(error.to_string()),
            }
        }
        let mut provider_synchronization = self
            .translator
            .synchronize_resource_operations(resource_operations)
            .await;
        for result in self.translator.synchronize_text_changes(text_changes).await {
            merge_provider_synchronization(&mut provider_synchronization, result);
        }
        for (provider, error) in document_sync_failures {
            let message = format!("open-document synchronization failed: {error}");
            if let Some(result) = provider_synchronization
                .iter_mut()
                .find(|result| result.provider == provider.as_str())
            {
                result.synchronized = false;
                result.message = Some(result.message.take().map_or_else(
                    || message.clone(),
                    |existing| format!("{message}; {existing}"),
                ));
            } else {
                provider_synchronization.push(ProviderSynchronization {
                    provider: provider.to_string(),
                    synchronized: false,
                    watched_file_notifications: 0,
                    message: Some(message),
                });
            }
        }
        for error in tracker_sync_failures {
            merge_provider_synchronization(
                &mut provider_synchronization,
                ProviderSynchronization {
                    provider: "document_tracker".to_string(),
                    synchronized: false,
                    watched_file_notifications: 0,
                    message: Some(error),
                },
            );
        }
        let verification = if let Some(check) = semantic_check {
            Some(self.verify_inline_module_after_apply(check).await)
        } else {
            None
        };
        if let Some(error) = audit_failure {
            provider_synchronization.push(ProviderSynchronization {
                provider: "audit".to_string(),
                synchronized: false,
                watched_file_notifications: 0,
                message: Some(error),
            });
        }
        (provider_synchronization, verification)
    }

    pub(super) async fn hover(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<HoverResult, String> {
        let mut result = self
            .translator
            .handle_hover(file_path.clone(), line, character)
            .await
            .map_err(|error| error.to_string())?;
        defer_oversized_hover_contents(
            &mut result,
            &self.deferred_results,
            self.deferred_scope.as_deref().unwrap_or_default(),
        )?;
        let target = result.range.as_ref().map_or((line, character), |range| {
            (range.start.line, range.start.character)
        });
        result.symbol_handle = self.source_handle(&result.source, target.0, target.1);
        Ok(result)
    }

    pub(super) async fn definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<DefinitionResult, String> {
        let mut result = self
            .translator
            .handle_definition_page(file_path, line, character, page_token.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        self.attach_location_handles(&mut result.locations);
        Ok(result)
    }

    pub(super) async fn references(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
        limits: SemanticResultLimits,
        page_offset: Option<usize>,
    ) -> Result<ReferencesResult, String> {
        let mut result = self
            .translator
            .handle_references_page(
                file_path,
                line,
                character,
                include_declaration,
                limits,
                page_offset,
            )
            .await
            .map_err(|error| error.to_string())?;
        for group in &mut result.groups {
            for reference in &mut group.references {
                self.attach_reference_handle(reference);
            }
        }
        if let Some(declaration) = result.declaration.as_mut() {
            self.attach_location_handle(declaration);
        }
        Ok(result)
    }

    pub(super) async fn read_source_resource(
        &self,
        resource: SourceResource,
        max_response_bytes: usize,
    ) -> Result<SourceFrame, String> {
        self.translator
            .read_source_resource_with_max_bytes(&resource, max_response_bytes)
            .await
            .map_err(|error| error.to_string())
    }

    pub(super) fn defer_inspect_section<T: Serialize>(
        &self,
        section: &mut crate::bridge::InspectSection<T>,
        snapshot_hash: &str,
    ) {
        let Some(data) = section.data.take() else {
            return;
        };
        let Ok(value) = serde_json::to_value(data) else {
            return;
        };
        let provider = section
            .provider
            .clone()
            .unwrap_or_else(|| "mcpls".to_owned());
        let reference = self
            .deferred_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert_scoped(
                value,
                snapshot_hash.to_owned(),
                self.deferred_scope.as_deref().unwrap_or_default(),
            );
        *section = crate::bridge::InspectSection::deferred(
            provider,
            section.total,
            section.returned,
            "response_budget_exhausted",
            reference,
        );
    }

    pub(super) async fn resolve_symbol_handle(
        &self,
        symbol_handle: SymbolHandle,
    ) -> Result<ResolvedSymbolTarget, String> {
        let target = self.resolve_symbol_target(&symbol_handle).await?;
        Ok(ResolvedSymbolTarget {
            file_path: target.file_path.to_string_lossy().into_owned(),
            line: target.line,
            character: target.character,
        })
    }

    pub(super) async fn diagnostics(
        &mut self,
        file_path: String,
        options: DiagnosticOptions,
    ) -> Result<DiagnosticsResult, String> {
        self.diagnostics_page(file_path, options, true).await
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn diagnostics_page(
        &mut self,
        file_path: String,
        mut options: DiagnosticOptions,
        fresh: bool,
    ) -> Result<DiagnosticsResult, String> {
        let page_token = options.page_token.take();
        let scope = self.deferred_scope.clone().unwrap_or_default();
        let state = if let Some(page_token) = page_token {
            let token = page_token
                .strip_prefix("mcpls-deferred:///")
                .ok_or_else(|| {
                    "page_token must be the next_cursor returned by get_diagnostics".to_owned()
                })?;
            let value = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_scoped(token, &scope)?;
            let state: DiagnosticsPageState = serde_json::from_value(value)
                .map_err(|error| format!("invalid diagnostics page: {error}"))?;
            if state.file_path != file_path || state.fresh != fresh {
                return Err("page_token belongs to a different diagnostics request".to_owned());
            }
            state
        } else {
            if options.item_limit == 0 || options.item_limit > 1_000 {
                return Err("item_limit must be between 1 and 1000".to_owned());
            }
            if !(4_096..=1_048_576).contains(&options.byte_limit) {
                return Err("byte_limit must be between 4096 and 1048576".to_owned());
            }

            let mut collection_options = options.clone();
            collection_options.item_limit = usize::MAX;
            let mut result = if fresh {
                self.translator
                    .handle_actor_diagnostics(file_path.clone(), collection_options)
                    .await
                    .map_err(|error| error.to_string())?
            } else {
                self.translator
                    .handle_cached_diagnostics(&file_path, collection_options)
                    .await
                    .map_err(|error| error.to_string())?
            };
            if fresh {
                self.attach_diagnostic_fix_handles(&mut result).await;
            }
            result.filters = options;
            if let Ok((path, document_version, content_hash, content)) =
                self.translator.source_snapshot(Path::new(&file_path)).await
            {
                let total_lines = u32::try_from(content.lines().count().max(1)).unwrap_or(u32::MAX);
                result.source_resource = Some(DeferredResourceReference {
                    uri: make_source_uri(
                        &path,
                        1,
                        1,
                        total_lines,
                        1,
                        &content_hash,
                        document_version,
                    )
                    .map_err(|error| error.to_string())?,
                    kind: "source_context".to_owned(),
                    snapshot_hash: content_hash,
                    document_version,
                    total_bytes: Some(content.len()),
                });
            }
            if serde_json::to_vec(&result).map_or(usize::MAX, |encoded| encoded.len())
                > result.filters.byte_limit
            {
                let scope = self.deferred_scope.as_deref().unwrap_or_default();
                let max_bytes = result.filters.byte_limit;
                defer_oversized_diagnostic_payloads(
                    &mut result,
                    max_bytes,
                    &self.deferred_results,
                    scope,
                )?;
            }
            let encoded = serde_json::to_vec(&result.diagnostics)
                .map_err(|error| format!("failed to identify diagnostics snapshot: {error}"))?;
            result.snapshot_identity = Some(format!("{:x}", Sha256::digest(encoded)));
            result.max_bytes = Some(result.filters.byte_limit);
            DiagnosticsPageState {
                file_path,
                fresh,
                result,
            }
        };

        let max_items = state.result.filters.item_limit;
        let max_bytes = state.result.filters.byte_limit;
        let (mut result, continuation) = bounded_diagnostics_page(state, max_items, max_bytes)?;
        if let Some(continuation) = continuation {
            let snapshot_identity = continuation
                .result
                .snapshot_identity
                .clone()
                .ok_or_else(|| "diagnostics page is missing its snapshot identity".to_owned())?;
            let value = serde_json::to_value(continuation)
                .map_err(|error| format!("failed to store diagnostics page: {error}"))?;
            let reference = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped(value, snapshot_identity, &scope);
            result.next_cursor = Some(reference.uri);
        }
        debug_assert!(serde_json::to_vec(&result).is_ok_and(|encoded| encoded.len() <= max_bytes));
        Ok(result)
    }

    pub(super) async fn attach_diagnostic_fix_handles(&mut self, result: &mut DiagnosticsResult) {
        for diagnostic in &mut result.diagnostics {
            let Some(file_path) = diagnostic.context.path.clone() else {
                continue;
            };
            let Ok(actions) = self
                .translator
                .request_code_actions(
                    file_path.clone(),
                    diagnostic.range.start.line,
                    diagnostic.range.start.character,
                    diagnostic.range.end.line,
                    diagnostic.range.end.character,
                    Some(lsp_types::CodeActionKind::QUICKFIX.as_str().to_owned()),
                )
                .await
            else {
                continue;
            };
            diagnostic.context.fix_handles = actions
                .into_iter()
                .map(|action| {
                    self.code_actions
                        .insert(StoredCodeAction {
                            file_path: file_path.clone(),
                            action,
                            created_at: Instant::now(),
                        })
                        .to_string()
                })
                .collect();
        }
    }

    pub(super) async fn rename(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult, String> {
        let mut result = self
            .translator
            .handle_rename(file_path, line, character, new_name)
            .await
            .map_err(|error| error.to_string())?;
        bound_rename_result(
            &mut result,
            &self.deferred_results,
            self.deferred_scope.as_deref().unwrap_or_default(),
        )?;
        Ok(result)
    }

    pub(super) async fn rename_workspace_edit(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<Option<WorkspaceEdit>, String> {
        self.translator
            .request_rename_workspace_edit(file_path, line, character, new_name)
            .await
            .map_err(|error| error.to_string())
    }

    pub(super) async fn completions(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        trigger: Option<String>,
        page_token: Option<String>,
    ) -> Result<CompletionsResult, String> {
        let mut result = self
            .translator
            .handle_completions(file_path, line, character, trigger)
            .await
            .map_err(|error| error.to_string())?;
        let (page, snapshot_identity, next_cursor) =
            completion_page_bounds(&result.items, page_token.as_deref())?;
        let total_items = result.items.len();
        let page_start = page.start;
        let mut items = result.items[page].to_vec();
        for (index, item) in items.iter_mut().enumerate() {
            let identity = completion_identity(item, page_start + index);
            item.completion_id = Some(identity.clone());
            item.insertion_handle = Some(identity);
        }
        result.items = items;
        result.total_items = total_items;
        result.returned_items = result.items.len();
        result.remaining_items = total_items.saturating_sub(page_start + result.items.len());
        result.next_cursor = next_cursor;
        result.snapshot_identity = snapshot_identity;
        defer_oversized_completion_payloads(
            &mut result,
            &self.deferred_results,
            self.deferred_scope.as_deref().unwrap_or_default(),
        )?;
        Ok(result)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn document_symbols(
        &self,
        request: DocumentSymbolPageRequest,
    ) -> Result<DocumentSymbolsResult, String> {
        if request.options.limit == 0 || request.options.limit > 1_000 {
            return Err("limit must be between 1 and 1000".to_owned());
        }
        if !(4_096..=1_048_576).contains(&request.max_bytes) {
            return Err("max_bytes must be between 4096 and 1048576".to_owned());
        }

        let scope = self.deferred_scope.as_deref().unwrap_or_default();
        let state = if let Some(page_token) = request.page_token {
            let token = page_token
                .strip_prefix("mcpls-deferred:///")
                .ok_or_else(|| {
                    "page_token must be the next_cursor returned by get_document_symbols".to_owned()
                })?;
            let value = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_scoped(token, scope)?;
            serde_json::from_value(value)
                .map_err(|error| format!("invalid document-symbol page: {error}"))?
        } else {
            let mut result = self
                .translator
                .handle_document_symbols_for_page(
                    request.file_path.clone(),
                    request.options.clone(),
                )
                .await
                .map_err(|error| error.to_string())?;
            let snapshot_identity = result.snapshot_identity.clone().ok_or_else(|| {
                "document-symbol result is missing its snapshot identity".to_owned()
            })?;
            let (path, document_version, content_hash, content) = self
                .translator
                .source_snapshot(Path::new(&request.file_path))
                .await
                .map_err(|error| error.to_string())?;
            if content_hash != snapshot_identity {
                return Err("source changed while preparing the document-symbol page".to_owned());
            }
            let snapshot = document_version.map_or_else(
                || SourceSnapshot::Hash(content_hash),
                SourceSnapshot::Version,
            );
            attach_document_symbol_handles(
                &mut self
                    .symbol_handles
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                &mut result.symbols,
                &path,
                &snapshot,
                None,
            );
            clear_document_symbol_sources(&mut result.symbols);
            let total_lines = u32::try_from(content.lines().count().max(1)).unwrap_or(u32::MAX);
            let source_resource = DeferredResourceReference {
                uri: make_source_uri(
                    &path,
                    1,
                    1,
                    total_lines,
                    1,
                    &snapshot_identity,
                    document_version,
                )
                .map_err(|error| error.to_string())?,
                kind: "source_context".to_owned(),
                snapshot_hash: snapshot_identity.clone(),
                document_version,
                total_bytes: Some(content.len()),
            };
            DocumentSymbolPageState {
                total: result.total,
                snapshot_identity,
                document_version: result.document_version,
                project_relative_path: result.project_relative_path,
                source_resource,
                filters: request.options.clone(),
                symbols: flatten_document_symbols(result.symbols),
            }
        };

        let max_items = state.filters.limit as usize;
        let (mut result, remaining) =
            bounded_document_symbol_page(state, max_items, request.max_bytes)?;
        if let Some(remaining) = remaining {
            let snapshot_identity = remaining.snapshot_identity.clone();
            let value = serde_json::to_value(remaining)
                .map_err(|error| format!("failed to store document-symbol page: {error}"))?;
            let reference = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped(value, snapshot_identity, scope);
            result.next_cursor = Some(reference.uri);
        }
        debug_assert!(
            serde_json::to_vec(&result).is_ok_and(|encoded| encoded.len() <= request.max_bytes)
        );
        Ok(result)
    }

    pub(super) async fn format_document(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<FormatDocumentResult, String> {
        let mut result = self
            .translator
            .handle_format_document(file_path, tab_size, insert_spaces)
            .await
            .map_err(|error| error.to_string())?;
        bound_format_document_result(
            &mut result,
            &self.deferred_results,
            self.deferred_scope.as_deref().unwrap_or_default(),
        )?;
        Ok(result)
    }

    pub(super) async fn format_workspace_edit(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<Option<WorkspaceEdit>, String> {
        self.translator
            .request_format_workspace_edit(file_path, tab_size, insert_spaces)
            .await
            .map_err(|error| error.to_string())
    }

    pub(super) async fn semantic_discovery(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        kind: SemanticDiscoveryKind,
        page_token: Option<String>,
    ) -> Result<SemanticDiscoveryResult, String> {
        let mut result = self
            .translator
            .request_semantic_discovery(file_path, line, character, kind, page_token.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        self.attach_location_handles(&mut result.locations);
        defer_semantic_discovery_payloads(
            &mut result,
            &self.deferred_results,
            self.deferred_scope.as_deref().unwrap_or_default(),
        )?;
        Ok(result)
    }

    pub(super) async fn workspace_symbol(
        &self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
        match_mode: WorkspaceSymbolMatchMode,
        scope: WorkspaceSymbolScope,
        include_generated: bool,
    ) -> Result<WorkspaceSymbolResult, String> {
        let mut result = self
            .translator
            .handle_workspace_symbol_with_generated(
                query,
                kind_filter,
                limit,
                match_mode,
                scope,
                include_generated,
            )
            .await
            .map_err(|error| error.to_string())?;
        discard_workspace_symbol_struct_uses(&mut result.symbols);
        result.returned = result.symbols.len();
        self.attach_workspace_symbol_handles(&mut result.symbols)
            .await;
        Ok(result)
    }
    pub(super) async fn workspace_symbol_page(
        &self,
        request: WorkspaceSymbolPageRequest,
    ) -> Result<WorkspaceSymbolResult, String> {
        if request.max_items == 0 || request.max_items > 1_000 {
            return Err("max_items must be between 1 and 1000".to_owned());
        }
        if !(4_096..=1_048_576).contains(&request.max_bytes) {
            return Err("max_bytes must be between 4096 and 1048576".to_owned());
        }

        let scope = self.deferred_scope.as_deref().unwrap_or_default();
        let state = if let Some(page_token) = request.page_token {
            let token = page_token
                .strip_prefix("mcpls-deferred:///")
                .ok_or_else(|| {
                    "page_token must be the next_cursor returned by workspace_symbol_search"
                        .to_owned()
                })?;
            let value = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_scoped(token, scope)?;
            serde_json::from_value(value)
                .map_err(|error| format!("invalid workspace-symbol page: {error}"))?
        } else {
            let mut result = self
                .translator
                .handle_workspace_symbol_all_with_generated(
                    request.query,
                    request.kind_filter,
                    request.match_mode,
                    request.scope,
                    request.include_generated,
                )
                .await
                .map_err(|error| error.to_string())?;
            discard_workspace_symbol_struct_uses(&mut result.symbols);
            self.attach_workspace_symbol_handles(&mut result.symbols)
                .await;
            let snapshot_identity = workspace_symbol_snapshot_identity(&result.symbols)?;
            WorkspaceSymbolPageState {
                total: result.symbols.len(),
                snapshot_identity,
                symbols: result.symbols,
            }
        };

        let (mut result, remaining) =
            bounded_workspace_symbol_page(state, request.max_items, request.max_bytes)?;
        if let Some(remaining) = remaining {
            let snapshot_identity = remaining.snapshot_identity.clone();
            let value = serde_json::to_value(remaining)
                .map_err(|error| format!("failed to store workspace-symbol page: {error}"))?;
            let reference = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped(value, snapshot_identity, scope);
            result.next_cursor = Some(reference.uri);
        }
        debug_assert!(
            serde_json::to_vec(&result).is_ok_and(|encoded| encoded.len() <= request.max_bytes)
        );
        Ok(result)
    }

    async fn workspace_symbol_complete(
        &self,
        query: String,
        kind_filter: Option<String>,
        match_mode: WorkspaceSymbolMatchMode,
        scope: WorkspaceSymbolScope,
        include_generated: bool,
    ) -> Result<WorkspaceSymbolResult, String> {
        let mut result = self
            .translator
            .handle_workspace_symbol_all_with_generated(
                query,
                kind_filter,
                match_mode,
                scope,
                include_generated,
            )
            .await
            .map_err(|error| error.to_string())?;
        discard_workspace_symbol_struct_uses(&mut result.symbols);
        self.attach_workspace_symbol_handles(&mut result.symbols)
            .await;
        result.returned = result.symbols.len();
        result.remaining = result.total.saturating_sub(result.returned);
        result.next_cursor = None;
        result.max_bytes = None;
        Ok(result)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn workspace_symbol_batch(
        &self,
        request: WorkspaceSymbolBatchRequest,
    ) -> Result<WorkspaceSymbolBatchResult, String> {
        let scope = self.deferred_scope.as_deref().unwrap_or_default();
        let (state, token, entry_offset, symbol_offset, continuation) = if let Some(page_token) =
            request.page_token.as_deref()
        {
            let (token, entry_offset, symbol_offset) =
                parse_workspace_symbol_batch_cursor(page_token)?;
            let value = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_scoped(token, scope)?;
            let state: WorkspaceSymbolBatchPageState = serde_json::from_value(value)
                .map_err(|error| format!("invalid workspace-symbol batch page: {error}"))?;
            if state.filter_identity != workspace_symbol_batch_filter_identity(&request) {
                return Err(
                    "page_token belongs to a different workspace-symbol batch request".to_owned(),
                );
            }
            (state, token.to_owned(), entry_offset, symbol_offset, true)
        } else {
            let snapshot_identity = self.workspace_snapshot_identity().await?;
            let filter_identity = workspace_symbol_batch_filter_identity(&request);
            let mut seen = HashMap::new();
            let mut entries = Vec::with_capacity(request.queries.len());
            let mut provider_requests = 0;
            let mut cache_hit = false;
            let mut unique_queries = 0;

            for query in request.queries.iter().cloned() {
                if let Some(&reused_from) = seen.get(&query) {
                    entries.push(WorkspaceSymbolBatchEntry {
                        query,
                        result: None,
                        reused_from: Some(reused_from),
                        skipped_by_budget: false,
                    });
                    continue;
                }

                let entry_index = entries.len();
                seen.insert(query.clone(), entry_index);
                unique_queries += 1;
                let cache_key = format!(
                    "{}\0{}\0{:?}\0{}\0{:?}\0{:?}",
                    snapshot_identity,
                    query,
                    request.kind_filter,
                    request.include_generated,
                    request.match_mode,
                    request.scope,
                );
                let cached = self
                    .workspace_symbol_results
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&cache_key)
                    .cloned();
                let result = if let Some(result) = cached {
                    cache_hit = true;
                    result
                } else {
                    let result = self
                        .workspace_symbol_complete(
                            query.clone(),
                            request.kind_filter.clone(),
                            request.match_mode,
                            request.scope,
                            request.include_generated,
                        )
                        .await?;
                    provider_requests += 1;
                    let mut cache = self
                        .workspace_symbol_results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if cache.len() >= WORKSPACE_SYMBOL_CACHE_MAX_ENTRIES {
                        cache.clear();
                    }
                    cache.insert(cache_key, result.clone());
                    result
                };
                entries.push(WorkspaceSymbolBatchEntry {
                    query,
                    result: Some(result),
                    reused_from: None,
                    skipped_by_budget: false,
                });
            }

            let state = WorkspaceSymbolBatchPageState {
                entries,
                unique_queries,
                provider_requests,
                snapshot_identity,
                cache_hit,
                filter_identity,
            };
            let value = serde_json::to_value(&state)
                .map_err(|error| format!("failed to store workspace-symbol batch: {error}"))?;
            let reference = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped_kind(
                    value,
                    state.snapshot_identity.clone(),
                    scope,
                    "workspace_symbol_batch_page",
                );
            let token = reference
                .uri
                .strip_prefix("mcpls-deferred:///")
                .ok_or_else(|| "workspace-symbol batch cursor has an invalid URI".to_owned())?
                .to_owned();
            (state, token, 0, 0, false)
        };

        let page = bounded_workspace_symbol_batch_page(
            &state,
            &token,
            entry_offset,
            symbol_offset,
            request.max_items,
            request.max_bytes,
        )?;
        if !continuation && page.next_cursor.is_none() {
            self.deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&token);
        }
        Ok(page)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn lexical_search(
        &self,
        request: LexicalSearchRequest,
    ) -> Result<LexicalSearchScan, String> {
        const LEXICAL_CONTEXT_BYTES: usize = 16 * 1024;
        let request_identity = lexical_search_request_identity(&request);
        let scope = self.deferred_scope.as_deref().unwrap_or_default();
        let (state, token, offset) = if let Some(page_token) = request.page_token.as_deref() {
            let (token, offset) = parse_lexical_page_cursor(page_token)?;
            let value = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_scoped(token, scope)?;
            let state: LexicalSearchPageState = serde_json::from_value(value)
                .map_err(|error| format!("invalid lexical-search page: {error}"))?;
            if state.request_identity != request_identity {
                return Err("page_token belongs to a different lexical_search request".to_owned());
            }
            (state, token.to_owned(), offset)
        } else {
            let paths = collect_project_paths_filtered(
                self.translator.workspace_roots(),
                request.include_generated,
                request.max_files,
                &request.include_paths,
                &request.exclude_paths,
            )
            .await?;
            let mut matches = Vec::new();
            let mut total_matches: usize = 0;
            let mut scanned_bytes: usize = 0;
            let mut scanned_files: usize = 0;
            let mut source_budget = SourceBudget::new(LEXICAL_CONTEXT_BYTES);
            for path in paths {
                scanned_files += 1;
                let (path, document_version, content_hash, source) =
                    match self.translator.source_snapshot(&path).await {
                        Ok(snapshot) => snapshot,
                        Err(error) if is_invalid_utf8_error(&error) => {
                            continue;
                        }
                        Err(error) => return Err(error.to_string()),
                    };
                scanned_bytes = scanned_bytes.saturating_add(source.len());
                let ranges = find_matches(
                    &source,
                    &request.query,
                    request.mode,
                    request.case,
                    request.multiline,
                )?;
                total_matches = total_matches.saturating_add(ranges.len());
                let project_relative_path = self
                    .translator
                    .workspace_roots()
                    .iter()
                    .find_map(|root| path.strip_prefix(root).ok())
                    .map(|relative| relative.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        "lexical search found a path outside its project roots".to_owned()
                    })?;
                for byte_range in ranges {
                    let start =
                        byte_offset_to_position(&source, byte_range.start, PositionEncoding::Utf8)
                            .ok_or_else(|| {
                                "lexical match start is not a valid text position".to_owned()
                            })?;
                    let end =
                        byte_offset_to_position(&source, byte_range.end, PositionEncoding::Utf8)
                            .ok_or_else(|| {
                                "lexical match end is not a valid text position".to_owned()
                            })?;
                    let source_uri = make_source_uri(
                        &path,
                        start.line.saturating_add(1),
                        start.character.saturating_add(1),
                        end.line.saturating_add(1),
                        end.character.saturating_add(1),
                        &content_hash,
                        document_version,
                    )
                    .map_err(|error| error.to_string())?;
                    let source = if request.context_lines == 0 {
                        None
                    } else {
                        Some(
                            self.translator
                                .lexical_source_context(
                                    &path,
                                    crate::bridge::Range {
                                        start: crate::bridge::Position2D {
                                            line: start.line.saturating_add(1),
                                            character: start.character.saturating_add(1),
                                        },
                                        end: crate::bridge::Position2D {
                                            line: end.line.saturating_add(1),
                                            character: end.character.saturating_add(1),
                                        },
                                    },
                                    &mut source_budget,
                                    request.context_lines,
                                )
                                .await,
                        )
                    };
                    matches.push(LexicalSearchMatch {
                        project_relative_path: project_relative_path.clone(),
                        document_version,
                        content_hash: content_hash.clone(),
                        source_uri,
                        source,
                        byte_range,
                    });
                }
            }
            let encoded = serde_json::to_vec(&matches)
                .map_err(|error| format!("failed to identify lexical snapshot: {error}"))?;
            let snapshot_identity = format!("{:x}", Sha256::digest(encoded));
            let state = LexicalSearchPageState {
                matches,
                total_matches,
                scanned_files,
                scanned_bytes,
                snapshot_identity,
                request_identity,
            };
            let value = serde_json::to_value(&state)
                .map_err(|error| format!("failed to store lexical page: {error}"))?;
            let token = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped_kind(
                    value,
                    state.snapshot_identity.clone(),
                    scope,
                    "lexical_search_page",
                )
                .uri
                .trim_start_matches("mcpls-deferred:///")
                .to_owned();
            (state, token, 0)
        };

        if offset > state.matches.len() {
            return Err(
                "lexical_search page_token offset is outside the retained result".to_owned(),
            );
        }
        let end = offset
            .saturating_add(request.max_matches)
            .min(state.matches.len());
        Ok(LexicalSearchScan {
            matches: state.matches[offset..end].to_vec(),
            total_matches: state.total_matches,
            scanned_files: state.scanned_files,
            scanned_bytes: state.scanned_bytes,
            offset,
            page_token: token,
            snapshot_identity: state.snapshot_identity,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn lexical_search_batch(
        &self,
        request: LexicalSearchBatchRequest,
    ) -> Result<LexicalSearchBatchResult, String> {
        if request.queries.is_empty() {
            return Err("lexical query batch must not be empty".to_owned());
        }
        let paths = collect_project_paths_filtered(
            self.translator.workspace_roots(),
            request.include_generated,
            request.max_files,
            &request.include_paths,
            &request.exclude_paths,
        )
        .await?;
        let mut files = Vec::with_capacity(paths.len());
        let mut scanned_bytes: usize = 0;
        let mut scanned_files: usize = 0;
        for path in paths {
            scanned_files += 1;
            let (path, document_version, content_hash, source) =
                match self.translator.source_snapshot(&path).await {
                    Ok(snapshot) => snapshot,
                    Err(error) if is_invalid_utf8_error(&error) => {
                        continue;
                    }
                    Err(error) => return Err(error.to_string()),
                };
            let project_relative_path = self
                .translator
                .workspace_roots()
                .iter()
                .find_map(|root| path.strip_prefix(root).ok())
                .map(|relative| relative.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    "lexical search found a path outside its project roots".to_owned()
                })?;
            scanned_bytes = scanned_bytes.saturating_add(source.len());
            files.push(LexicalFileSnapshot {
                path,
                document_version,
                content_hash,
                source,
                project_relative_path,
            });
        }
        let mut snapshot_hasher = Sha256::new();
        for file in &files {
            snapshot_hasher.update(file.project_relative_path.as_bytes());
            snapshot_hasher.update(file.content_hash.as_bytes());
        }
        let snapshot_identity = format!("{:x}", snapshot_hasher.finalize());
        let mut seen = HashMap::new();
        let mut entries = Vec::with_capacity(request.queries.len());
        let mut returned = 0;
        let mut truncated = false;
        let mut source_budget = SourceBudget::new(16 * 1024);
        for query in request.queries {
            if let Some(&reused_from) = seen.get(&query) {
                entries.push(crate::bridge::lexical::LexicalSearchBatchEntry {
                    query,
                    result: None,
                    reused_from: Some(reused_from),
                    skipped_by_budget: false,
                });
                continue;
            }
            let entry_index = entries.len();
            seen.insert(query.clone(), entry_index);
            let remaining = request.max_matches.saturating_sub(returned);
            if remaining == 0 {
                truncated = true;
                entries.push(crate::bridge::lexical::LexicalSearchBatchEntry {
                    query,
                    result: None,
                    reused_from: None,
                    skipped_by_budget: true,
                });
                continue;
            }
            let mut matches = Vec::new();
            let mut total_matches: usize = 0;
            for file in &files {
                let ranges = find_matches(
                    &file.source,
                    &query,
                    request.mode,
                    request.case,
                    request.multiline,
                )?;
                total_matches = total_matches.saturating_add(ranges.len());
                for byte_range in ranges
                    .into_iter()
                    .take(remaining.saturating_sub(matches.len()))
                {
                    let start = byte_offset_to_position(
                        &file.source,
                        byte_range.start,
                        PositionEncoding::Utf8,
                    )
                    .ok_or_else(|| "lexical match start is not a valid text position".to_owned())?;
                    let end = byte_offset_to_position(
                        &file.source,
                        byte_range.end,
                        PositionEncoding::Utf8,
                    )
                    .ok_or_else(|| "lexical match end is not a valid text position".to_owned())?;
                    let source_uri = make_source_uri(
                        &file.path,
                        start.line.saturating_add(1),
                        start.character.saturating_add(1),
                        end.line.saturating_add(1),
                        end.character.saturating_add(1),
                        &file.content_hash,
                        file.document_version,
                    )
                    .map_err(|error| error.to_string())?;
                    let source = if request.context_lines == 0 {
                        None
                    } else {
                        Some(
                            self.translator
                                .lexical_source_context(
                                    &file.path,
                                    crate::bridge::Range {
                                        start: crate::bridge::Position2D {
                                            line: start.line.saturating_add(1),
                                            character: start.character.saturating_add(1),
                                        },
                                        end: crate::bridge::Position2D {
                                            line: end.line.saturating_add(1),
                                            character: end.character.saturating_add(1),
                                        },
                                    },
                                    &mut source_budget,
                                    request.context_lines,
                                )
                                .await,
                        )
                    };
                    matches.push(LexicalSearchMatch {
                        project_relative_path: file.project_relative_path.clone(),
                        document_version: file.document_version,
                        content_hash: file.content_hash.clone(),
                        source_uri,
                        source,
                        byte_range,
                    });
                }
            }
            let query_identity = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&matches).unwrap_or_default())
            );
            let query_truncated = total_matches > matches.len();
            truncated |= query_truncated;
            returned += matches.len();
            entries.push(crate::bridge::lexical::LexicalSearchBatchEntry {
                query,
                result: Some(crate::bridge::lexical::LexicalSearchResult {
                    returned: matches.len(),
                    total: total_matches,
                    remaining: total_matches.saturating_sub(matches.len()),
                    scanned_files,
                    scanned_bytes,
                    snapshot_identity: format!("{snapshot_identity}:{query_identity}"),
                    max_bytes: request.max_bytes,
                    truncated: query_truncated,
                    next_cursor: None,
                    matches,
                }),
                reused_from: None,
                skipped_by_budget: false,
            });
        }
        Ok(LexicalSearchBatchResult {
            unique_queries: seen.len(),
            entries,
            scanned_files,
            scanned_bytes,
            returned,
            truncated,
            max_matches: request.max_matches,
            max_bytes: request.max_bytes,
            snapshot_identity,
        })
    }

    pub(super) async fn workspace_snapshot_identity(&self) -> Result<String, String> {
        let mut paths = Vec::new();
        for root in self.translator.workspace_roots() {
            for entry in WalkBuilder::new(root)
                .standard_filters(true)
                .build()
                .flatten()
            {
                if entry.file_type().is_some_and(|kind| kind.is_file()) {
                    paths.push(entry.into_path());
                }
            }
        }
        paths.sort_unstable();
        let mut hasher = Sha256::new();
        for path in paths {
            let snapshot = match self.translator.source_snapshot(&path).await {
                Ok(snapshot) => snapshot,
                Err(error) if error.to_string().contains("valid UTF-8") => continue,
                Err(error) => return Err(error.to_string()),
            };
            let (_, version, content_hash, _) = snapshot;
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update(version.unwrap_or_default().to_le_bytes());
            hasher.update(content_hash.as_bytes());
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    pub(super) async fn workspace_symbol_in_path(
        &self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
        match_mode: WorkspaceSymbolMatchMode,
        scope: WorkspaceSymbolScope,
        path: &Path,
    ) -> Result<WorkspaceSymbolResult, String> {
        let mut result = self
            .translator
            .handle_workspace_symbol_in_path(query, kind_filter, limit, match_mode, scope, path)
            .await
            .map_err(|error| error.to_string())?;
        discard_workspace_symbol_struct_uses(&mut result.symbols);
        result.returned = result.symbols.len();
        self.attach_workspace_symbol_handles(&mut result.symbols)
            .await;
        Ok(result)
    }

    // One actor-owned operation intentionally makes the snapshot boundary visible.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn inspect_symbol(
        &self,
        mut request: InspectSymbolRequest,
    ) -> Result<InspectSymbolResult, String> {
        use crate::bridge::{
            InspectCalls, InspectSection, InspectSymbolResolution, InspectSymbolSectionKind,
            InspectSymbolSections,
        };

        request.budget.max_bytes = request
            .budget
            .max_bytes
            .min(crate::bridge::translator::INSPECT_SYMBOL_RESULT_MAX_BYTES);

        let (resolution, target) = if let Some(handle) = request.symbol_handle.clone() {
            let target = match self.resolve_symbol_target(&handle).await {
                Ok(target) => target,
                Err(error) if error.starts_with("stale_symbol_handle:") => {
                    return Ok(InspectSymbolResult {
                        resolution: InspectSymbolResolution::Stale {
                            symbol_handle: handle,
                            reason: error,
                            retryable: true,
                        },
                        sections: InspectSymbolSections::default(),
                        budget: request.budget,
                        returned_bytes: 0,
                        truncated: false,
                    });
                }
                Err(error) => return Err(error),
            };
            (
                InspectSymbolResolution::Selected {
                    symbol: None,
                    symbol_handle: Some(handle),
                },
                Some((
                    target.file_path.to_string_lossy().into_owned(),
                    target.line,
                    target.character,
                    None,
                )),
            )
        } else {
            let query = request
                .query
                .as_ref()
                .filter(|query| !query.is_empty())
                .ok_or_else(|| "query or symbol_handle is required".to_owned())?;
            let mut result = if let Some(path) = request.path.as_ref() {
                let path = PathBuf::from(path);
                let path = if path.is_absolute() {
                    path
                } else {
                    self.translator
                        .workspace_roots()
                        .first()
                        .ok_or_else(|| "project has no workspace root".to_owned())?
                        .join(path)
                };
                let path = dunce::canonicalize(path).map_err(|error| error.to_string())?;
                self.workspace_symbol_in_path(
                    query.clone(),
                    request.kind.clone(),
                    request.candidate_limit,
                    WorkspaceSymbolMatchMode::Exact,
                    WorkspaceSymbolScope::Project,
                    &path,
                )
                .await?
            } else {
                self.workspace_symbol(
                    query.clone(),
                    request.kind.clone(),
                    request.candidate_limit,
                    WorkspaceSymbolMatchMode::Exact,
                    WorkspaceSymbolScope::Project,
                    false,
                )
                .await?
            };
            result.symbols.retain(|symbol| {
                request.container.as_ref().is_none_or(|container| {
                    symbol.container_name.as_deref() == Some(container.as_str())
                })
            });
            match result.symbols.len() {
                0 => (InspectSymbolResolution::NotFound, None),
                1 => {
                    let symbol = result.symbols.remove(0);
                    let symbol_range = symbol.location.range.clone();
                    let target = if let Some(handle) = symbol.location.symbol_handle.as_ref() {
                        let stored = match self.resolve_symbol_target(handle).await {
                            Ok(stored) => stored,
                            Err(error) if error.starts_with("stale_symbol_handle:") => {
                                return Ok(InspectSymbolResult {
                                    resolution: InspectSymbolResolution::Stale {
                                        symbol_handle: handle.clone(),
                                        reason: error,
                                        retryable: true,
                                    },
                                    sections: InspectSymbolSections::default(),
                                    budget: request.budget,
                                    returned_bytes: 0,
                                    truncated: false,
                                });
                            }
                            Err(error) => return Err(error),
                        };
                        Some((
                            stored.file_path.to_string_lossy().into_owned(),
                            stored.line,
                            stored.character,
                            Some(symbol_range),
                        ))
                    } else {
                        symbol.location.path.clone().map(|path| {
                            (
                                path,
                                symbol.location.range.start.line,
                                symbol.location.range.start.character,
                                Some(symbol_range),
                            )
                        })
                    };
                    (
                        InspectSymbolResolution::Selected {
                            symbol_handle: symbol.location.symbol_handle.clone(),
                            symbol: Some(Box::new(symbol)),
                        },
                        target,
                    )
                }
                _ => (
                    InspectSymbolResolution::Ambiguous {
                        candidates: result.symbols,
                    },
                    None,
                ),
            }
        };

        let mut sections = InspectSymbolSections::default();
        let Some((file_path, line, character, symbol_range)) = target else {
            let mut result = InspectSymbolResult {
                resolution,
                sections,
                budget: request.budget,
                returned_bytes: 0,
                truncated: false,
            };
            while serde_json::to_vec(&result).map_or(usize::MAX, |json| json.len())
                > result.budget.max_bytes
            {
                let InspectSymbolResolution::Ambiguous { candidates } = &mut result.resolution
                else {
                    break;
                };
                if candidates.len() <= 1 || candidates.pop().is_none() {
                    break;
                }
                result.truncated = true;
            }
            update_inspect_symbol_byte_count(&mut result);
            return Ok(result);
        };
        let limits = SemanticResultLimits {
            total: request.budget.max_items,
            per_file: request.budget.max_items,
            per_symbol: request.budget.max_items,
        };
        let wants_hover = request.wants(InspectSymbolSectionKind::Declaration)
            || request.wants(InspectSymbolSectionKind::Hover);
        let wants_definitions = request.wants(InspectSymbolSectionKind::Definitions);
        let wants_implementations = request.wants(InspectSymbolSectionKind::Implementations);
        let wants_references = request.wants(InspectSymbolSectionKind::References);
        let wants_calls = request.wants(InspectSymbolSectionKind::Calls);
        let wants_tests = request.wants(InspectSymbolSectionKind::Tests);
        let wants_runnables = request.wants(InspectSymbolSectionKind::Runnables);
        let wants_diagnostics = request.wants(InspectSymbolSectionKind::Diagnostics);
        let diagnostics_options = DiagnosticOptions {
            item_limit: request.budget.max_items,
            byte_limit: request.budget.max_bytes,
            ..DiagnosticOptions::default()
        };

        let hover_request = Box::pin(async {
            self.translator
                .handle_hover(file_path.clone(), line, character)
                .await
                .map_err(|error| error.to_string())
        });
        let definitions_request = Box::pin(async {
            self.translator
                .handle_definition(file_path.clone(), line, character)
                .await
                .map_err(|error| error.to_string())
        });
        let implementations_request = Box::pin(async {
            self.translator
                .handle_implementation(file_path.clone(), line, character)
                .await
                .map_err(|error| error.to_string())
        });
        let references_request = Box::pin(async {
            self.translator
                .handle_references_page(file_path.clone(), line, character, true, limits, Some(0))
                .await
                .map_err(|error| error.to_string())
        });
        let calls_request = Box::pin(async {
            let prepared = self
                .translator
                .handle_call_hierarchy_prepare(file_path.clone(), line, character)
                .await
                .map_err(|error| error.to_string())?;
            let Some(first) = prepared.items.first() else {
                return Ok(missing_call_hierarchy_item());
            };
            let provider = prepared.provider;
            let item = serde_json::to_value(first).map_err(|error| error.to_string())?;
            let (incoming, outgoing) = tokio::join!(
                self.translator
                    .handle_incoming_calls(item.clone(), limits, None),
                self.translator.handle_outgoing_calls(item, limits, None),
            );
            let incoming = incoming.map_err(|error| error.to_string())?;
            let outgoing = outgoing.map_err(|error| error.to_string())?;
            let total = incoming.total_calls + outgoing.total_calls;
            let returned = incoming.returned_calls + outgoing.returned_calls;
            let truncated = incoming.truncated || outgoing.truncated;
            Ok(InspectSection::available(
                provider,
                total,
                returned,
                truncated,
                InspectCalls { incoming, outgoing },
            ))
        });
        let tests_request = Box::pin(async {
            self.translator
                .request_semantic_discovery(
                    file_path.clone(),
                    line,
                    character,
                    SemanticDiscoveryKind::RelatedTests,
                    None,
                )
                .await
                .map_err(|error| error.to_string())
        });
        let runnables_request = Box::pin(async {
            self.translator
                .request_semantic_discovery(
                    file_path.clone(),
                    line,
                    character,
                    SemanticDiscoveryKind::Runnables,
                    None,
                )
                .await
                .map_err(|error| error.to_string())
        });
        let diagnostics_request = Box::pin(async {
            self.translator
                .handle_cached_diagnostics(&file_path, diagnostics_options)
                .await
                .map_err(|error| error.to_string())
        });
        // A newly opened document can make the first semantic request receive
        // ContentModified while its language server settles. Complete the
        // declaration preflight before fanning out independent sections so
        // they share that synchronization instead of retrying together.
        let hover = inspect_if_requested(wants_hover, hover_request).await;
        let (definitions, implementations, references, calls, tests, runnables, diagnostics) = tokio::join!(
            inspect_if_requested(wants_definitions, definitions_request),
            inspect_if_requested(wants_implementations, implementations_request),
            inspect_if_requested(wants_references, references_request),
            inspect_if_requested(wants_calls, calls_request),
            inspect_if_requested(wants_tests, tests_request),
            inspect_if_requested(wants_runnables, runnables_request),
            inspect_if_requested(wants_diagnostics, diagnostics_request),
        );

        if let Some(hover) = hover {
            match hover {
                Ok(hover) => {
                    let mut hover = hover;
                    let target = hover.range.as_ref().map_or((line, character), |range| {
                        (range.start.line, range.start.character)
                    });
                    hover.symbol_handle = self.source_handle(&hover.source, target.0, target.1);
                    if request.wants(InspectSymbolSectionKind::Declaration) {
                        sections.declaration = InspectSection::available(
                            hover.provider.clone(),
                            1,
                            1,
                            hover.truncated,
                            hover.source.clone(),
                        );
                    }
                    if request.wants(InspectSymbolSectionKind::Hover) {
                        sections.hover = InspectSection::available(
                            hover.provider.clone(),
                            1,
                            1,
                            hover.truncated,
                            hover,
                        );
                    }
                }
                Err(error) => {
                    if request.wants(InspectSymbolSectionKind::Declaration) {
                        sections.declaration = InspectSection::unavailable(error.clone());
                    }
                    if request.wants(InspectSymbolSectionKind::Hover) {
                        sections.hover = InspectSection::unavailable(error);
                    }
                }
            }
        }
        if let Some(definitions) = definitions {
            sections.definitions = match definitions {
                Ok(mut result) => {
                    self.attach_location_handles(&mut result.locations);
                    InspectSection::available(
                        result.provider.clone(),
                        result.locations.len(),
                        result.locations.len(),
                        result.truncated,
                        result,
                    )
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }
        if let Some(implementations) = implementations {
            sections.implementations = match implementations {
                Ok(mut result) => {
                    self.attach_location_handles(&mut result.locations);
                    InspectSection::available(
                        result.provider.clone(),
                        result.locations.len(),
                        result.locations.len(),
                        result.truncated,
                        result,
                    )
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }
        if let Some(references) = references {
            sections.references = match references {
                Ok(mut result) => {
                    for group in &mut result.groups {
                        for reference in &mut group.references {
                            self.attach_reference_handle(reference);
                        }
                    }
                    if let Some(declaration) = result.declaration.as_mut() {
                        self.attach_location_handle(declaration);
                    }
                    InspectSection::available(
                        result.provider.clone(),
                        result.total_references,
                        result.returned_references,
                        result.truncated,
                        result,
                    )
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }
        if let Some(calls) = calls {
            sections.calls = match calls {
                Ok(mut section) => {
                    if let Some(calls) = section.data.as_mut() {
                        for call in &mut calls.incoming.calls {
                            let item = &mut call.from;
                            item.symbol_handle = item.source.as_ref().and_then(|source| {
                                self.source_handle(
                                    source,
                                    item.selection_range.start.line,
                                    item.selection_range.start.character,
                                )
                            });
                        }
                        for call in &mut calls.outgoing.calls {
                            let item = &mut call.to;
                            item.symbol_handle = item.source.as_ref().and_then(|source| {
                                self.source_handle(
                                    source,
                                    item.selection_range.start.line,
                                    item.selection_range.start.character,
                                )
                            });
                        }
                    }
                    section
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }
        for (result, section) in [
            (tests, &mut sections.tests),
            (runnables, &mut sections.runnables),
        ] {
            let Some(result) = result else { continue };
            *section = match result {
                Ok(result) if !result.supported => InspectSection::unsupported(
                    result.provider,
                    "provider does not support section",
                ),
                Ok(mut result) => {
                    self.attach_location_handles(&mut result.locations);
                    InspectSection::available(
                        result.provider.clone(),
                        result.runnables.len(),
                        result.runnables.len(),
                        result.truncated,
                        result,
                    )
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }
        if let Some(diagnostics) = diagnostics {
            sections.diagnostics = match diagnostics {
                Ok(mut result) => {
                    result.diagnostics.retain(|diagnostic| {
                        symbol_range
                            .as_ref()
                            .map_or(diagnostic.range.start.line == line, |range| {
                                diagnostic.range.start.line <= range.end.line
                                    && diagnostic.range.end.line >= range.start.line
                            })
                    });
                    let relevant = result.diagnostics.len();
                    result.total_diagnostics = relevant;
                    result.returned_diagnostics = relevant;
                    result.total_groups = relevant;
                    result.returned_groups = relevant;
                    result.omitted_groups = 0;
                    InspectSection::available(
                        "lsp/diagnostics",
                        relevant,
                        relevant,
                        result.truncated,
                        result,
                    )
                }
                Err(error) => InspectSection::unavailable(error),
            };
        }

        let mut result = InspectSymbolResult {
            resolution,
            sections,
            budget: request.budget,
            returned_bytes: 0,
            truncated: false,
        };
        let snapshot_hash = self
            .translator
            .source_snapshot(Path::new(&file_path))
            .await
            .map_or_else(
                |_| format!("generation:{}", self.generation),
                |(_, _, hash, _)| hash,
            );
        macro_rules! drop_section_if_over_budget {
            ($field:ident) => {
                if serde_json::to_vec(&result).map_or(usize::MAX, |json| json.len())
                    > result.budget.max_bytes
                    && result.sections.$field.completeness
                        != crate::bridge::InspectSectionCompleteness::NotRequested
                    && result.sections.$field.completeness
                        != crate::bridge::InspectSectionCompleteness::Deferred
                {
                    self.defer_inspect_section(&mut result.sections.$field, &snapshot_hash);
                    result.truncated = true;
                }
            };
        }
        drop_section_if_over_budget!(runnables);
        drop_section_if_over_budget!(hover);
        drop_section_if_over_budget!(definitions);
        drop_section_if_over_budget!(diagnostics);
        drop_section_if_over_budget!(tests);
        drop_section_if_over_budget!(references);
        drop_section_if_over_budget!(calls);
        drop_section_if_over_budget!(implementations);
        drop_section_if_over_budget!(declaration);
        if serde_json::to_vec(&result).map_or(usize::MAX, |json| json.len())
            > result.budget.max_bytes
            && let InspectSymbolResolution::Selected { symbol, .. } = &mut result.resolution
        {
            *symbol = None;
            result.truncated = true;
        }
        update_inspect_symbol_byte_count(&mut result);
        Ok(result)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn collect_inspect_symbol_batch(
        &self,
        request: InspectSymbolBatchRequest,
        scope: &str,
    ) -> Result<InspectSymbolBatchSnapshot, String> {
        let target_count = request.targets.len();
        if target_count == 0
            || target_count > crate::bridge::translator::INSPECT_SYMBOL_BATCH_MAX_TARGETS
        {
            return Err("between 1 and 16 symbol targets are required".to_owned());
        }
        let identity_bytes = serde_json::to_vec(&request.targets)
            .map_err(|error| error.to_string())?
            .len();
        let available_bytes = request.budget.max_bytes.saturating_sub(
            identity_bytes
                + crate::bridge::translator::INSPECT_SYMBOL_BATCH_RESPONSE_OVERHEAD_BYTES,
        );
        let target_budget = crate::bridge::InspectSymbolBudget {
            max_bytes: (available_bytes / target_count)
                .min(crate::bridge::translator::INSPECT_SYMBOL_BATCH_MAX_ENTRY_BYTES),
            max_items: request.budget.max_items / target_count,
        };
        if target_budget.max_bytes
            < crate::bridge::translator::INSPECT_SYMBOL_BATCH_MIN_BYTES_PER_TARGET
            || target_budget.max_items == 0
        {
            return Err("batch budget is too small for every symbol target".to_owned());
        }

        let mut unique_targets = Vec::new();
        let mut target_indices = HashMap::new();
        let mut target_sources = Vec::with_capacity(target_count);
        for target in &request.targets {
            let index = target_indices.get(target).copied().unwrap_or_else(|| {
                let index = unique_targets.len();
                target_indices.insert(target.clone(), index);
                unique_targets.push(target.clone());
                index
            });
            target_sources.push(index);
        }

        let inspections = unique_targets.into_iter().map(|target| {
            let inspect_request = InspectSymbolRequest {
                symbol_handle: target.symbol_handle.clone(),
                query: target.query.clone(),
                kind: target.kind.clone(),
                path: target.path.clone(),
                container: target.container,
                candidate_limit: request.candidate_limit,
                sections: request.sections.clone(),
                budget: target_budget,
            };
            async move { Box::pin(self.inspect_symbol(inspect_request)).await }
        });
        let unique_results = futures::future::join_all(inspections).await;
        let mut entries = request
            .targets
            .into_iter()
            .zip(target_sources)
            .map(|(target, source)| match &unique_results[source] {
                Ok(result) => InspectSymbolBatchEntry {
                    target,
                    result: Some(result.clone()),
                    error: None,
                    resource: None,
                },
                Err(error) => InspectSymbolBatchEntry {
                    target,
                    result: None,
                    error: Some(error.clone()),
                    resource: None,
                },
            })
            .collect::<Vec<_>>();
        let truncated = entries
            .iter()
            .filter_map(|entry| entry.result.as_ref())
            .any(|result| result.truncated);
        let encoded = serde_json::to_vec(&entries)
            .map_err(|error| format!("failed to identify inspect batch snapshot: {error}"))?;
        let snapshot_identity = format!("{:x}", Sha256::digest(encoded));
        for entry in &mut entries {
            if serde_json::to_vec(entry).map_or(usize::MAX, |json| json.len())
                <= crate::bridge::translator::INSPECT_SYMBOL_BATCH_MAX_ENTRY_BYTES
            {
                continue;
            }
            let value = serde_json::to_value(&*entry)
                .map_err(|error| format!("failed to defer inspect batch entry: {error}"))?;
            let resource = self
                .deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped(value, snapshot_identity.clone(), scope);
            entry.result = None;
            entry.error = None;
            entry.resource = Some(resource);
        }
        Ok(InspectSymbolBatchSnapshot {
            inspections_started: unique_results.len(),
            entries,
            snapshot_identity,
            truncated,
            max_items: request.budget.max_items,
        })
    }

    pub(super) async fn inspect_symbol_batch(
        &self,
        request: InspectSymbolBatchRequest,
    ) -> Result<InspectSymbolBatchResult, String> {
        let scope = self.deferred_scope.as_deref().unwrap_or_default();
        if let Some(page_token) = request.page_token.as_deref() {
            if !request.targets.is_empty() {
                return Err("targets must be empty when page_token is supplied".to_owned());
            }
            let (token, offset) = parse_inspect_symbol_batch_cursor(page_token)?;
            let snapshot = self
                .inspect_symbol_batch_pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read(token, scope)?;
            return bounded_inspect_symbol_batch_page(&snapshot, token, offset);
        }

        let snapshot = self.collect_inspect_symbol_batch(request, scope).await?;
        let token = self
            .inspect_symbol_batch_pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(snapshot.clone(), scope);
        let result = bounded_inspect_symbol_batch_page(&snapshot, &token, 0)?;
        if result.next_cursor.is_none() {
            self.inspect_symbol_batch_pages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&token);
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn code_actions(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
        page_token: Option<String>,
    ) -> Result<CodeActionsResult, String> {
        let actions = self
            .translator
            .handle_code_actions(
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
            )
            .await
            .map_err(|error| error.to_string())?;
        let (page, snapshot_identity, next_cursor) =
            code_action_page_bounds(&actions.actions, page_token.as_deref())?;
        let total_actions = actions.actions.len();
        let page_start = page.start;
        let actions = actions.actions[page].to_vec();
        let mut result = CodeActionsResult {
            returned_actions: actions.len(),
            remaining_actions: total_actions.saturating_sub(page_start + actions.len()),
            total_actions,
            next_cursor,
            snapshot_identity,
            actions,
            actions_resource: None,
        };
        defer_oversized_code_action_payloads(
            &mut result,
            &self.deferred_results,
            self.deferred_scope.as_deref().unwrap_or_default(),
        )?;
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn code_action_list(
        &mut self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
        page_token: Option<String>,
    ) -> Result<CodeActionsResult, String> {
        let actions = self
            .translator
            .request_code_actions(
                file_path.clone(),
                start_line,
                start_character,
                end_line,
                end_character,
                kind_filter,
            )
            .await
            .map_err(|error| error.to_string())?;
        let (page, snapshot_identity, next_cursor) =
            code_action_page_bounds(&actions, page_token.as_deref())?;
        let total_actions = actions.len();
        let mut result = Vec::with_capacity(page.len());
        for action in actions[page.clone()].iter().cloned() {
            let id = self.code_actions.insert(StoredCodeAction {
                file_path: file_path.clone(),
                action: action.clone(),
                created_at: Instant::now(),
            });
            result.push(convert_code_action_or_command(action, Some(id.to_string())));
        }
        let mut result = CodeActionsResult {
            returned_actions: result.len(),
            remaining_actions: total_actions.saturating_sub(page.start + result.len()),
            total_actions,
            next_cursor,
            snapshot_identity,
            actions: result,
            actions_resource: None,
        };
        defer_oversized_code_action_payloads(
            &mut result,
            &self.deferred_results,
            self.deferred_scope.as_deref().unwrap_or_default(),
        )?;
        Ok(result)
    }

    pub(super) async fn preview_code_action(
        &mut self,
        action_id: PlanId,
        project_id: &str,
        encoding: PositionEncoding,
        root: &Path,
    ) -> Result<PreviewArtifact, String> {
        let stored = self.code_actions.take(&action_id)?;
        let action = match stored.action {
            lsp_types::CodeActionOrCommand::Command(_) => {
                return Err("command-only code actions are unsupported".to_string());
            }
            lsp_types::CodeActionOrCommand::CodeAction(mut action) => {
                if let Some(reason) = action
                    .disabled
                    .as_ref()
                    .map(|disabled| disabled.reason.clone())
                {
                    return Err(format!("code action is disabled: {reason}"));
                }
                if action.command.is_some() {
                    return Err("code actions with commands are unsupported".to_string());
                }
                if action.edit.is_none() {
                    if action.data.is_none() {
                        return Err("code action has no workspace edit".to_string());
                    }
                    action = self
                        .translator
                        .resolve_code_action(&stored.file_path, action)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                if let Some(reason) = action
                    .disabled
                    .as_ref()
                    .map(|disabled| disabled.reason.clone())
                {
                    return Err(format!("resolved code action is disabled: {reason}"));
                }
                if action.command.is_some() {
                    return Err("resolved code actions with commands are unsupported".to_string());
                }
                action
            }
        };
        let edit = action
            .edit
            .ok_or_else(|| "resolved code action has no workspace edit".to_string())?;
        self.preview_edit(project_id, edit, encoding, root).await
    }

    pub(super) async fn prepare_call_hierarchy(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<CallHierarchyPrepareResult, String> {
        let (provider, kind, total_items, truncated, snapshot_hash, items) =
            if let Some(page_token) = page_token {
                let token = page_token
                    .strip_prefix("mcpls-deferred:///")
                    .ok_or_else(|| "invalid call hierarchy page token".to_owned())?;
                let scope = self.deferred_scope.as_deref().unwrap_or_default();
                let value = self
                    .deferred_results
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .read_scoped(token, scope)?;
                let provider = value
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "invalid call hierarchy page payload".to_owned())?
                    .to_owned();
                let kind = serde_json::from_value(
                    value
                        .get("kind")
                        .cloned()
                        .ok_or_else(|| "invalid call hierarchy page payload".to_owned())?,
                )
                .map_err(|error| format!("invalid call hierarchy page kind: {error}"))?;
                let total_items = value
                    .get("total_items")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| "invalid call hierarchy page count".to_owned())?;
                let truncated = value
                    .get("truncated")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let snapshot_hash = value
                    .get("snapshot_hash")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let items = serde_json::from_value(
                    value
                        .get("items")
                        .cloned()
                        .ok_or_else(|| "invalid call hierarchy page items".to_owned())?,
                )
                .map_err(|error| format!("invalid call hierarchy page items: {error}"))?;
                (provider, kind, total_items, truncated, snapshot_hash, items)
            } else {
                let mut result = self
                    .translator
                    .handle_call_hierarchy_prepare(file_path, line, character)
                    .await
                    .map_err(|error| error.to_string())?;
                for item in &mut result.items {
                    item.symbol_handle = item.source.as_ref().and_then(|source| {
                        self.source_handle(
                            source,
                            item.selection_range.start.line,
                            item.selection_range.start.character,
                        )
                    });
                }
                let snapshot_hash = call_hierarchy_snapshot_hash(&result.items);
                (
                    result.provider,
                    result.kind,
                    result.total_items,
                    result.truncated,
                    snapshot_hash,
                    result.items,
                )
            };

        let (items, remaining) = page_items(items, CALL_HIERARCHY_PAGE_SIZE);
        let next_cursor = remaining.map(|items| {
            let value = serde_json::json!({
                "provider": provider,
                "kind": kind,
                "total_items": total_items,
                "truncated": truncated,
                "snapshot_hash": snapshot_hash,
                "items": items,
            });
            let scope = self.deferred_scope.as_deref().unwrap_or_default();
            self.deferred_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_scoped(value, snapshot_hash.clone(), scope)
                .uri
        });

        let has_next_page = next_cursor.is_some();

        Ok(CallHierarchyPrepareResult {
            provider,
            kind,
            total_items,
            returned_items: items.len(),
            next_cursor,
            truncated: truncated || has_next_page,
            items,
        })
    }

    pub(super) async fn incoming_calls(
        &self,
        item: serde_json::Value,
        limits: SemanticResultLimits,
        page_token: Option<String>,
    ) -> Result<IncomingCallsResult, String> {
        let mut result = self
            .translator
            .handle_incoming_calls(item, limits, page_token)
            .await
            .map_err(|error| error.to_string())?;
        for call in &mut result.calls {
            let item = &mut call.from;
            item.symbol_handle = item.source.as_ref().and_then(|source| {
                self.source_handle(
                    source,
                    item.selection_range.start.line,
                    item.selection_range.start.character,
                )
            });
        }
        Ok(result)
    }

    pub(super) async fn outgoing_calls(
        &self,
        item: serde_json::Value,
        limits: SemanticResultLimits,
        page_token: Option<String>,
    ) -> Result<OutgoingCallsResult, String> {
        let mut result = self
            .translator
            .handle_outgoing_calls(item, limits, page_token)
            .await
            .map_err(|error| error.to_string())?;
        for call in &mut result.calls {
            let item = &mut call.to;
            item.symbol_handle = item.source.as_ref().and_then(|source| {
                self.source_handle(
                    source,
                    item.selection_range.start.line,
                    item.selection_range.start.character,
                )
            });
        }
        Ok(result)
    }

    pub(super) async fn signature_help(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<SignatureHelpResult, String> {
        let mut result = self
            .translator
            .handle_signature_help(file_path, line, character)
            .await
            .map_err(|error| error.to_string())?;
        let (page, snapshot_identity, next_cursor) =
            signature_page_bounds(&result.signatures, page_token.as_deref())?;
        let total_signatures = result.signatures.len();
        let page_start = page.start;
        let mut signatures = result.signatures[page].to_vec();
        for (index, signature) in signatures.iter_mut().enumerate() {
            signature.signature_id = Some(signature_identity(signature, page_start + index));
        }
        result.signatures = signatures;
        result.total_signatures = total_signatures;
        result.returned_signatures = result.signatures.len();
        result.remaining_signatures =
            total_signatures.saturating_sub(page_start + result.signatures.len());
        result.next_cursor = next_cursor;
        result.snapshot_identity = snapshot_identity;
        defer_oversized_signature_payloads(
            &mut result,
            &self.deferred_results,
            self.deferred_scope.as_deref().unwrap_or_default(),
        )?;
        Ok(result)
    }

    pub(super) async fn inlay_hints(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        page_token: Option<String>,
    ) -> Result<InlayHintsResult, String> {
        let mut result = self
            .translator
            .handle_inlay_hints(
                file_path,
                start_line,
                start_character,
                end_line,
                end_character,
            )
            .await
            .map_err(|error| error.to_string())?;
        let (page, snapshot_identity, next_cursor) =
            inlay_hint_page_bounds(&result.hints, page_token.as_deref())?;
        let total_hints = result.hints.len();
        let page_start = page.start;
        let mut hints = result.hints[page].to_vec();
        for (index, hint) in hints.iter_mut().enumerate() {
            let identity = inlay_hint_identity(hint, page_start + index);
            hint.hint_id = Some(identity.clone());
            hint.resolve_handle = Some(identity);
        }
        result.hints = hints;
        result.total_hints = total_hints;
        result.returned_hints = result.hints.len();
        result.remaining_hints = total_hints.saturating_sub(page_start + result.hints.len());
        result.next_cursor = next_cursor;
        result.snapshot_identity = snapshot_identity;
        defer_oversized_inlay_hint_payloads(
            &mut result,
            &self.deferred_results,
            self.deferred_scope.as_deref().unwrap_or_default(),
        )?;
        Ok(result)
    }

    pub(super) async fn go_to_implementation(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<LocationsResult, String> {
        let mut result = self
            .translator
            .handle_implementation_page(file_path, line, character, page_token.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        self.attach_location_handles(&mut result.locations);
        Ok(result)
    }

    pub(super) async fn go_to_type_definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        page_token: Option<String>,
    ) -> Result<LocationsResult, String> {
        let mut result = self
            .translator
            .handle_type_definition_page(file_path, line, character, page_token.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        self.attach_location_handles(&mut result.locations);
        Ok(result)
    }

    pub(super) async fn cached_diagnostics(
        &mut self,
        file_path: &str,
        options: DiagnosticOptions,
    ) -> Result<DiagnosticsResult, String> {
        self.diagnostics_page(file_path.to_owned(), options, false)
            .await
    }

    pub(super) fn has_cached_diagnostics(&self, file_path: &str) -> Result<bool, String> {
        self.translator
            .has_cached_diagnostics(file_path)
            .map_err(|error| error.to_string())
    }

    pub(super) fn validate_path(&self, file_path: &str) -> Result<(), String> {
        self.translator
            .validate_path(Path::new(file_path))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(super) fn source_path_is_authorized(&self, path: &Path) -> bool {
        self.translator.source_path_is_authorized(path)
    }

    pub(super) fn server_logs_page(
        &self,
        limit: usize,
        min_level: Option<String>,
        cursor: Option<&str>,
    ) -> Result<ServerLogsResult, String> {
        let mut result = self
            .translator
            .actor_server_logs_page(limit, min_level, cursor)
            .map_err(|error| error.to_string())?;
        self.defer_notification_messages(&mut result.logs, "diagnostic_log_message")?;
        bound_server_logs_result(&mut result, cursor)?;
        Ok(result)
    }

    pub(super) fn server_messages_page(
        &self,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ServerMessagesResult, String> {
        let mut result = self
            .translator
            .actor_server_messages_page(limit, cursor)
            .map_err(|error| error.to_string())?;
        self.defer_notification_messages(&mut result.messages, "server_message")?;
        bound_server_messages_result(&mut result, cursor)?;
        Ok(result)
    }

    pub(super) fn defer_notification_messages<T>(
        &self,
        entries: &mut [T],
        kind: &str,
    ) -> Result<(), String>
    where
        T: NotificationMessageEntry,
    {
        defer_notification_messages_for_scope(
            entries,
            kind,
            &self.deferred_results,
            self.deferred_scope.as_deref().unwrap_or_default(),
        )
    }

    pub(super) fn server_capabilities(
        &self,
        language_id: Option<&str>,
    ) -> Result<Vec<ServerCapability>, String> {
        self.translator
            .server_capabilities(language_id)
            .map_err(|error| error.to_string())
    }

    pub(super) fn notification(
        &mut self,
        generation: u64,
        server_id: &ServerId,
        notification: LspNotification,
    ) -> Option<ProjectEvent> {
        let completes_initial_load = notification.completes_initial_load();
        match notification {
            LspNotification::PublishDiagnostics(params) => {
                let event = ProjectEvent::DiagnosticsUpdated {
                    uri: params.uri.to_string(),
                    version: params.version,
                    diagnostic_count: params.diagnostics.len(),
                };
                self.translator.notification_cache_mut().store_diagnostics(
                    server_id,
                    &params.uri,
                    params.version,
                    params.diagnostics,
                );
                Some(event)
            }
            LspNotification::LogMessage(params) => {
                self.translator
                    .notification_cache_mut()
                    .store_log_with_generation(generation, params.typ.into(), params.message);
                None
            }
            LspNotification::ShowMessage(params) => {
                self.translator
                    .notification_cache_mut()
                    .store_message_with_generation(generation, params.typ.into(), params.message);
                None
            }
            LspNotification::ServerStatus(_) | LspNotification::Progress { .. } => {
                if completes_initial_load {
                    self.translator.clear_expected_server(server_id);
                }
                None
            }
            LspNotification::Other { .. } => None,
        }
    }

    pub(super) async fn shutdown(&mut self) -> Result<(), String> {
        self.translator
            .shutdown()
            .await
            .map_err(|error| error.to_string())
    }

    pub(super) async fn activate_workspace_roots(
        &mut self,
        roots: Vec<PathBuf>,
        cancellation: CancellationToken,
    ) -> Result<ProjectActivation, String> {
        self.translator
            .activate_project_with_roots_cancelled(roots, cancellation)
            .await
            .map_err(|error| error.to_string())
    }

    pub(super) async fn add_workspace_root(
        &mut self,
        root: PathBuf,
        status: ProjectStatus,
        cancellation: CancellationToken,
    ) -> Result<ProjectActivation, String> {
        if status == ProjectStatus::Degraded
            && !self.has_active_workspace_roots(self.translator.workspace_roots())
        {
            let mut roots = self.translator.workspace_roots().to_vec();
            if !roots.contains(&root) {
                roots.push(root);
            }
            return self.activate_workspace_roots(roots, cancellation).await;
        }
        self.translator
            .add_workspace_root_cancelled(root, cancellation)
            .await
            .map_err(|error| error.to_string())
    }

    pub(super) async fn restart(
        &mut self,
        cancellation: CancellationToken,
    ) -> Result<ProjectActivation, String> {
        let roots = self.translator.workspace_roots().to_vec();
        if roots.is_empty() {
            return Ok(ProjectActivation::ready());
        }
        if self.translator.configured_language_ids().is_empty() {
            return Ok(ProjectActivation::ready());
        }
        self.shutdown().await?;
        self.activate_workspace_roots(roots, cancellation).await
    }

    pub(super) fn summary(&self) -> ProjectRuntimeSummary {
        ProjectRuntimeSummary::from_translator(&self.translator, self.generation)
    }

    pub(super) fn open_document_paths(&self) -> Vec<PathBuf> {
        self.translator.document_tracker().open_paths()
    }

    pub(super) fn has_dirty_documents(&self) -> bool {
        self.translator.document_tracker().has_dirty_documents()
    }
}

pub(super) fn code_action_has_assist_id(action: &lsp_types::CodeAction, expected: &str) -> bool {
    action
        .data
        .as_ref()
        .and_then(|data| data.get("id"))
        .and_then(serde_json::Value::as_str)
        .and_then(|id| id.strip_prefix(expected))
        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(':'))
}

pub(super) fn take_code_action_by_assist_id(
    actions: Vec<lsp_types::CodeActionOrCommand>,
    expected: &str,
) -> Option<lsp_types::CodeAction> {
    actions.into_iter().find_map(|action| match action {
        lsp_types::CodeActionOrCommand::CodeAction(action)
            if code_action_has_assist_id(&action, expected) =>
        {
            Some(action)
        }
        _ => None,
    })
}

pub(super) fn diagnostics_are_error_free(result: &DiagnosticsResult) -> bool {
    result
        .diagnostics
        .iter()
        .all(|diagnostic| !matches!(diagnostic.severity, DiagnosticSeverity::Error))
}

#[allow(clippy::large_futures)]
pub(super) async fn recover_project_after_server_exit(
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
) {
    loop {
        let Some(attempt) = runtime.begin_automatic_restart() else {
            channels.publish_failure(state, LANGUAGE_SERVER_EXITED);
            return;
        };

        state.last_error = Some(format!(
            "{LANGUAGE_SERVER_EXITED}; restarting (attempt {}/{MAX_AUTOMATIC_RESTART_ATTEMPTS})",
            attempt.number
        ));
        channels.publish_status(state, ProjectStatus::Restarting);
        if !channels.gate.is_accepting() {
            return;
        }
        tokio::select! {
            () = tokio::time::sleep(attempt.delay) => {}
            () = channels.gate.wait_for_rejection() => return,
        }
        if !channels.gate.is_accepting() {
            return;
        }
        let cancellation = CancellationToken::new();
        match run_cancellable_transition(
            &channels.gate,
            cancellation.clone(),
            runtime.restart(cancellation),
        )
        .await
        {
            Ok(notification_receivers) => {
                state.last_error = None;
                mark_project_started(
                    notification_receivers,
                    actor_sender,
                    channels,
                    state,
                    runtime,
                );
                return;
            }
            Err(error) if attempt.number < MAX_AUTOMATIC_RESTART_ATTEMPTS => {
                state.sync_runtime(runtime);
                state.last_error = Some(format!(
                    "automatic restart attempt {} failed: {error}",
                    attempt.number
                ));
            }
            Err(error) => {
                state.sync_runtime(runtime);
                channels.publish_failure(state, error);
                return;
            }
        }
    }
}

#[allow(clippy::large_futures)]
pub(super) async fn handle_server_exit(
    generation: u64,
    actor_sender: &mpsc::WeakSender<ProjectRequest>,
    channels: &ProjectActorChannels,
    state: &mut ProjectState,
    runtime: &mut ProjectRuntime,
    residency: Option<&ProjectResidency>,
) {
    if !runtime.owns_generation(generation) {
        return;
    }

    channels.publish(ProjectEvent::ServerExited { generation });
    match state.status {
        ProjectStatus::Ready | ProjectStatus::Degraded => {
            let _recovery_guard = match residency {
                Some(residency) => {
                    if let Some(guard) = residency.try_acquire_existing_for_recovery() {
                        Some(guard)
                    } else {
                        Some(
                            residency
                                .controller
                                .acquire_for(residency.group, RustResidencyMode::Activate)
                                .await,
                        )
                    }
                }
                None => None,
            };
            recover_project_after_server_exit(actor_sender, channels, state, runtime).await;
        }
        ProjectStatus::Starting | ProjectStatus::Restarting => {
            channels.publish_failure(state, LANGUAGE_SERVER_EXITED);
        }
        ProjectStatus::Failed
        | ProjectStatus::Stopping
        | ProjectStatus::Dormant
        | ProjectStatus::Stopped => {}
    }
}
