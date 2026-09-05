//! Diagnostics pull/push merging, cache-derived diagnostics, and server
//! log/message retrieval.

use std::path::PathBuf;
use std::sync::Arc;

use lsp_types::{PartialResultParams, TextDocumentIdentifier, WorkDoneProgressParams};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::Translator;
use super::dto::{
    Diagnostic, DiagnosticContext, DiagnosticOccurrence, DiagnosticOptions,
    DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticsResult, Position2D, Range,
    ServerLogsResult, ServerMessagesResult,
};
use super::encoding_ctx::EncodingCtx;
use super::routing::validate_path_against_roots;
use super::source_context::SourceBudget;
use crate::bridge::encoding::PositionEncoding;
use crate::bridge::notifications::RedactionPolicy;
use crate::bridge::{DiagnosticInfo, DocumentTracker, NotificationCache, path_to_uri};
use crate::config::ToolKind;
use crate::error::{Error, Result};

fn notification_page_bounds(
    item_count: usize,
    cursor: Option<&str>,
    snapshot_identity: &str,
    kind: &str,
    page_size: usize,
) -> Result<(std::ops::Range<usize>, Option<String>)> {
    let start = match cursor {
        Some(cursor) => {
            let (identity, offset) = cursor.split_once(':').ok_or_else(|| {
                Error::InvalidToolParams(format!("invalid {kind} cursor: {cursor}"))
            })?;
            if identity != snapshot_identity {
                return Err(Error::InvalidToolParams(format!(
                    "{kind} cursor belongs to a different snapshot"
                )));
            }
            offset
                .parse::<usize>()
                .map_err(|_| Error::InvalidToolParams(format!("invalid {kind} cursor: {cursor}")))?
        }
        None => 0,
    };
    if cursor.is_some() && start >= item_count {
        return Err(Error::InvalidToolParams(format!(
            "{kind} cursor is outside the retained snapshot: {start}"
        )));
    }
    let end = start.saturating_add(page_size).min(item_count);
    let next_cursor =
        (page_size > 0 && end < item_count).then(|| format!("{snapshot_identity}:{end}"));
    Ok((start..end, next_cursor))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticRequestParams {
    text_document: TextDocumentIdentifier,
    #[serde(skip_serializing_if = "Option::is_none")]
    identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_result_id: Option<String>,
    #[serde(flatten)]
    work_done_progress_params: WorkDoneProgressParams,
    #[serde(flatten)]
    partial_result_params: PartialResultParams,
}

fn diagnostic_request_params(text_document: TextDocumentIdentifier) -> DiagnosticRequestParams {
    DiagnosticRequestParams {
        text_document,
        identifier: None,
        previous_result_id: None,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn diagnostic_group_key(
    diagnostic: &Diagnostic,
) -> (&str, &DiagnosticSeverity, Option<&str>, Option<&str>, &str) {
    (
        diagnostic.context.uri.as_str(),
        &diagnostic.severity,
        diagnostic.context.diagnostic_source.as_deref(),
        diagnostic.code.as_deref(),
        diagnostic.message.as_str(),
    )
}

fn diagnostic_location(diagnostic: &Diagnostic) -> DiagnosticOccurrence {
    DiagnosticOccurrence {
        path: diagnostic.context.path.clone(),
        uri: diagnostic.context.uri.clone(),
        range: diagnostic.range.clone(),
    }
}

fn diagnostic_group_id(diagnostic: &Diagnostic) -> String {
    let mut hasher = Sha256::new();
    let severity = match diagnostic.severity {
        DiagnosticSeverity::Error => "error",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Information => "information",
        DiagnosticSeverity::Hint => "hint",
    };
    for part in [
        diagnostic.context.uri.as_str(),
        severity,
        diagnostic
            .context
            .diagnostic_source
            .as_deref()
            .unwrap_or_default(),
        diagnostic.code.as_deref().unwrap_or_default(),
        diagnostic.message.as_str(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn diagnostic_matches(diagnostic: &Diagnostic, options: &DiagnosticOptions) -> bool {
    let selected = |values: &[String], value: Option<&str>| {
        values.is_empty()
            || value.is_some_and(|value| {
                values
                    .iter()
                    .any(|selected| selected.eq_ignore_ascii_case(value))
            })
    };
    let generated = diagnostic
        .context
        .project_relative_path
        .as_deref()
        .is_some_and(|path| {
            std::path::Path::new(path).components().any(|component| {
                matches!(component.as_os_str().to_str(), Some("target" | "generated"))
            })
        });

    (options.severities.is_empty() || options.severities.contains(&diagnostic.severity))
        && selected(
            &options.sources,
            diagnostic.context.diagnostic_source.as_deref(),
        )
        && selected(&options.codes, diagnostic.code.as_deref())
        && (options.include_inactive || diagnostic.code.as_deref() != Some("inactive-code"))
        && (options.include_generated || !generated)
}

fn source_budget_exhausted(diagnostic: &Diagnostic) -> bool {
    let exhausted = |source: &super::dto::SourceContext| {
        matches!(
            source,
            super::dto::SourceContext::Unavailable {
                reason: super::dto::SourceUnavailableReason::ResponseBudgetExhausted
            }
        )
    };
    exhausted(&diagnostic.context.source_frame)
        || diagnostic
            .context
            .related_information
            .iter()
            .any(|related| exhausted(&related.location.source))
}

/// Convert an LSP diagnostic into the MCP-facing `Diagnostic` shape.
///
/// Shared by both the pull-model (`handle_diagnostics`) and cache-derived
/// (`diagnostics_from_cache_entry`) diagnostic paths, so their output never
/// diverges in formatting — `merge_diagnostics`'s dedup logic depends on
/// both sides mapping severity/code identically.
pub(super) async fn diagnostic_to_mcp(
    diag: &lsp_types::Diagnostic,
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
    workspace_roots: &[PathBuf],
    redaction_policy: &RedactionPolicy,
    source_budget: &mut SourceBudget,
) -> Diagnostic {
    let range = ctx.normalize_range(uri, diag.range).await;
    let path = crate::bridge::uri_to_path(uri);
    let mut data = diag.data.clone();
    if let Some(data) = &mut data {
        redaction_policy.redact_json(data);
    }
    let mut related_information = Vec::new();
    for related in diag.related_information.iter().flatten() {
        related_information.push(DiagnosticRelatedInformation {
            location: ctx
                .location(workspace_roots, related.location.clone(), source_budget)
                .await,
            message: related.message.clone(),
        });
    }
    Diagnostic {
        range: range.clone(),
        severity: match diag.severity {
            Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
            Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
            Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
            // INFORMATION and None (no severity reported) both fall here.
            _ => DiagnosticSeverity::Information,
        },
        message: diag.message.clone(),
        code: diag.code.as_ref().map(|c| match c {
            lsp_types::NumberOrString::Number(n) => n.to_string(),
            lsp_types::NumberOrString::String(s) => s.clone(),
        }),
        context: DiagnosticContext {
            project_relative_path: path.as_ref().and_then(|path| {
                workspace_roots.iter().find_map(|root| {
                    path.strip_prefix(root)
                        .ok()
                        .map(|path| path.to_string_lossy().into_owned())
                })
            }),
            path: path.map(|path| path.to_string_lossy().into_owned()),
            uri: uri.to_string(),
            source_frame: ctx
                .source_context(workspace_roots, uri, range, source_budget)
                .await,
            diagnostic_source: diag.source.clone(),
            code_description: diag
                .code_description
                .as_ref()
                .map(|description| description.href.to_string()),
            tags: diag.tags.as_ref().map_or_else(Vec::new, |tags| {
                tags.iter()
                    .filter_map(|tag| match *tag {
                        lsp_types::DiagnosticTag::UNNECESSARY => Some("unnecessary".to_owned()),
                        lsp_types::DiagnosticTag::DEPRECATED => Some("deprecated".to_owned()),
                        _ => None,
                    })
                    .collect()
            }),
            related_information,
            data,
            ..DiagnosticContext::default()
        },
    }
}

impl Translator {
    /// Resolve the LSP-side cache key (URI string) for a cached-diagnostics lookup.
    ///
    /// Split out from the cache read itself so callers (e.g. the
    /// `get_cached_diagnostics` MCP tool) can do the path `canonicalize()` and
    /// workspace-boundary check *before* taking the `NotificationCache` lock —
    /// that lock is also needed by `diagnostics_pump` to store incoming
    /// notifications, so nothing that isn't a plain map lookup should run
    /// while it's held.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is invalid or outside workspace boundaries.
    pub fn cached_diagnostics_uri(workspace_roots: &[PathBuf], file_path: &str) -> Result<String> {
        let path = PathBuf::from(file_path);
        let validated_path = validate_path_against_roots(&path, workspace_roots)?;

        // Use path_to_uri (strips \\?\ on Windows) so the key matches what
        // rust-analyzer stores in publishDiagnostics notifications.
        Ok(path_to_uri(&validated_path)?.to_string())
    }

    /// Handle diagnostics request.
    ///
    /// Merges the LSP pull-model response (`textDocument/diagnostic`) with
    /// whatever is already cached from `textDocument/publishDiagnostics` push
    /// notifications for the same file, so this returns the same diagnostics
    /// `get_cached_diagnostics` would for the file at the same point in time
    /// (see #244 — rust-analyzer's pull endpoint omits flycheck/clippy-sourced
    /// diagnostics, and empirically also some native ones, that are only ever
    /// delivered via the push path). If the pull request itself fails (e.g. a
    /// push-only server answering `-32601`, or a timeout), a non-empty cache
    /// entry is returned as a cache-only result instead of propagating the
    /// error, since the cache is not required to be fresher than the pull
    /// response to be useful here.
    ///
    /// The cache is read only after the pull request settles (success or
    /// failure) and held only for the lookup itself — never across the LSP
    /// round-trip — matching the lock-ordering discipline documented on
    /// `cached_diagnostics_uri`. Like `get_cached_diagnostics`, the cache is
    /// treated as eventually consistent: a cached entry may reflect a
    /// slightly older document version than the fresh pull result if an edit
    /// landed inside the server's flycheck debounce window.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP pull request fails and the cache holds no
    /// diagnostics for the file either, or if the file cannot be opened.
    pub async fn handle_diagnostics(
        &self,
        file_path: String,
        notification_cache: &Mutex<NotificationCache>,
    ) -> Result<DiagnosticsResult> {
        self.handle_diagnostics_with_options(
            file_path,
            notification_cache,
            DiagnosticOptions::default(),
        )
        .await
    }

    /// Handle diagnostics with explicit filters and response budgets.
    ///
    /// # Errors
    ///
    /// Returns an error when the document cannot be routed or both pull and
    /// cached diagnostics are unavailable.
    pub async fn handle_diagnostics_with_options(
        &self,
        file_path: String,
        notification_cache: &Mutex<NotificationCache>,
        options: DiagnosticOptions,
    ) -> Result<DiagnosticsResult> {
        let (server_id, client, uri) = self
            .prepare_document(&file_path, ToolKind::Diagnostics)
            .await?;
        let ctx = self.encoding_ctx(&server_id);

        let params = diagnostic_request_params(TextDocumentIdentifier { uri: uri.clone() });

        let pull_response: Result<lsp_types::DocumentDiagnosticReportResult> = client
            .request("textDocument/diagnostic", params, client.request_timeout())
            .await;

        let diag_info = {
            let cache = notification_cache.lock().await;
            cache.get_diagnostics(uri.as_str()).cloned()
        };

        match pull_response {
            Ok(response) => {
                let items = match response {
                    lsp_types::DocumentDiagnosticReportResult::Report(report) => match report {
                        lsp_types::DocumentDiagnosticReport::Full(full) => {
                            full.full_document_diagnostic_report.items
                        }
                        lsp_types::DocumentDiagnosticReport::Unchanged(_) => vec![],
                    },
                    lsp_types::DocumentDiagnosticReportResult::Partial(_) => vec![],
                };
                let mut diagnostics = Vec::with_capacity(items.len());
                let mut source_budget = SourceBudget::new(options.byte_limit);
                for d in &items {
                    diagnostics.push(
                        diagnostic_to_mcp(
                            d,
                            &ctx,
                            &uri,
                            &self.workspace_roots,
                            &self.redaction_policy,
                            &mut source_budget,
                        )
                        .await,
                    );
                }
                let pull = DiagnosticsResult::raw(diagnostics);
                let merged = Self::merge_diagnostics_enriched(
                    pull,
                    diag_info.as_ref(),
                    ctx.encoding,
                    &self.document_tracker,
                    &self.workspace_roots,
                    &self.redaction_policy,
                    &mut source_budget,
                )
                .await;
                Ok(Self::finish_diagnostics(merged.diagnostics, options))
            }
            Err(e) => {
                let cache_only = Self::diagnostics_from_cache_entry_enriched(
                    diag_info.as_ref(),
                    ctx.encoding,
                    &self.document_tracker,
                    &self.workspace_roots,
                    &self.redaction_policy,
                    &mut SourceBudget::new(options.byte_limit),
                )
                .await;
                if cache_only.diagnostics.is_empty() {
                    Err(e)
                } else {
                    Ok(Self::finish_diagnostics(cache_only.diagnostics, options))
                }
            }
        }
    }

    /// Convert a cached diagnostics entry into the MCP-facing result shape.
    ///
    /// Takes an already-cloned `Option<&DiagnosticInfo>` (out of the
    /// `NotificationCache` lock) rather than the cache itself, so this
    /// mapping — which is not a bounded operation for a large diagnostics set
    /// — never runs while the cache is locked.
    ///
    /// `encoding` is the negotiated encoding of the server that published
    /// these diagnostics; pass `PositionEncoding::Utf16` when no live server
    /// context is available (e.g. a cache-only read with no resolved owner).
    #[must_use]
    pub async fn diagnostics_from_cache_entry(
        diag_info: Option<&DiagnosticInfo>,
        encoding: PositionEncoding,
        tracker: &Arc<DocumentTracker>,
    ) -> DiagnosticsResult {
        Self::diagnostics_from_cache_entry_enriched(
            diag_info,
            encoding,
            tracker,
            &[],
            &RedactionPolicy::default(),
            &mut SourceBudget::default(),
        )
        .await
    }

    pub(super) async fn diagnostics_from_cache_entry_enriched(
        diag_info: Option<&DiagnosticInfo>,
        encoding: PositionEncoding,
        tracker: &Arc<DocumentTracker>,
        workspace_roots: &[PathBuf],
        redaction_policy: &RedactionPolicy,
        source_budget: &mut SourceBudget,
    ) -> DiagnosticsResult {
        let diagnostics = match diag_info {
            Some(diag_info) => {
                let ctx = EncodingCtx {
                    encoding,
                    tracker: tracker.clone(),
                    approved_source_paths: Arc::new(std::sync::Mutex::new(
                        std::collections::HashSet::new(),
                    )),
                };
                let mut result = Vec::with_capacity(diag_info.diagnostics.len());
                for d in &diag_info.diagnostics {
                    result.push(
                        diagnostic_to_mcp(
                            d,
                            &ctx,
                            &diag_info.uri,
                            workspace_roots,
                            redaction_policy,
                            source_budget,
                        )
                        .await,
                    );
                }
                result
            }
            None => Vec::new(),
        };

        DiagnosticsResult::raw(diagnostics)
    }

    /// Merge push-model diagnostics from the notification cache into a
    /// pull-model (`textDocument/diagnostic`) result.
    ///
    /// rust-analyzer's pull endpoint omits diagnostics that are only ever
    /// delivered via `textDocument/publishDiagnostics` push notifications —
    /// not just flycheck/clippy lints, but empirically (verified against a
    /// live rust-analyzer 1.97.1 session, see #244) some native diagnostics
    /// too. Those are cached separately in `NotificationCache`.
    ///
    /// Where the *same* logical problem is reported through both paths, the
    /// two representations were observed to differ in both `range` and
    /// rendered `message`. Captured example, a "not all trait items
    /// implemented" (E0046) error for one `impl` block: pull reported range
    /// `(96,7)-(96,12)` (the trait name) with message "not all trait items
    /// implemented, missing: `fn hello`"; the push notification for the same
    /// error reported range `(95,1)-(95,32)` (the impl block) with message
    /// "not all trait items implemented, missing: `hello`\nmissing `hello`
    /// in implementation" — same `code`/`severity`, adjacent but distinct
    /// ranges, different message text. Exact field equality never dedups
    /// cases like that.
    ///
    /// Given that, a cache entry is treated as a duplicate of a pull entry
    /// when both carry a `code`, the `(severity, code)` pair matches, *and*
    /// the two ranges are either overlapping or start within
    /// `DUPLICATE_RANGE_PROXIMITY_LINES` lines of each other — close
    /// enough to be the same underlying model divergence, not two distinct
    /// occurrences of the same error class (e.g. two unrelated `E0308`
    /// mismatches at different call sites in one file, one caught only
    /// natively and one only by flycheck). Diagnostics with no `code` fall
    /// back to full-field equality, since there is no cheaper stable
    /// identity available for them.
    ///
    /// Output is sorted by `(start.line, start.character)` so merged
    /// cache-only entries don't land out of document order after the
    /// pull-model ones.
    #[must_use]
    pub async fn merge_diagnostics(
        pull: DiagnosticsResult,
        diag_info: Option<&DiagnosticInfo>,
        encoding: PositionEncoding,
        tracker: &Arc<DocumentTracker>,
    ) -> DiagnosticsResult {
        Self::merge_diagnostics_enriched(
            pull,
            diag_info,
            encoding,
            tracker,
            &[],
            &RedactionPolicy::default(),
            &mut SourceBudget::default(),
        )
        .await
    }

    pub(super) async fn merge_diagnostics_enriched(
        mut pull: DiagnosticsResult,
        diag_info: Option<&DiagnosticInfo>,
        encoding: PositionEncoding,
        tracker: &Arc<DocumentTracker>,
        workspace_roots: &[PathBuf],
        redaction_policy: &RedactionPolicy,
        source_budget: &mut SourceBudget,
    ) -> DiagnosticsResult {
        /// Start-line distance within which same-code, same-severity
        /// diagnostics from the two models are still considered the same
        /// underlying problem. Derived from the captured E0046 case above
        /// (1 line apart); wide enough to absorb span drift between
        /// rust-analyzer's own spans and rustc's, narrow enough that two
        /// genuinely distinct same-code errors elsewhere in a file are not
        /// collapsed into one.
        const DUPLICATE_RANGE_PROXIMITY_LINES: u32 = 3;

        fn position_le(a: &Position2D, b: &Position2D) -> bool {
            (a.line, a.character) <= (b.line, b.character)
        }

        fn ranges_close(a: &Range, b: &Range) -> bool {
            let overlaps = position_le(&a.start, &b.end) && position_le(&b.start, &a.end);
            overlaps || a.start.line.abs_diff(b.start.line) <= DUPLICATE_RANGE_PROXIMITY_LINES
        }

        fn is_duplicate(pull: &[Diagnostic], candidate: &Diagnostic) -> bool {
            pull.iter().any(|p| match (&candidate.code, &p.code) {
                (Some(c), Some(pc)) if c == pc && p.severity == candidate.severity => {
                    ranges_close(&p.range, &candidate.range)
                }
                _ => p == candidate,
            })
        }

        let cached = Self::diagnostics_from_cache_entry_enriched(
            diag_info,
            encoding,
            tracker,
            workspace_roots,
            redaction_policy,
            source_budget,
        )
        .await
        .diagnostics;
        let new_diagnostics: Vec<_> = cached
            .into_iter()
            .filter(|c| !is_duplicate(&pull.diagnostics, c))
            .collect();
        pull.diagnostics.extend(new_diagnostics);
        pull.diagnostics
            .sort_by_key(|d| (d.range.start.line, d.range.start.character));
        pull
    }

    /// Apply stable filtering, grouping, and item limits to enriched diagnostics.
    #[must_use]
    pub fn finish_diagnostics(
        diagnostics: Vec<Diagnostic>,
        options: DiagnosticOptions,
    ) -> DiagnosticsResult {
        let total_diagnostics = diagnostics.len();
        let byte_truncated = diagnostics.iter().any(source_budget_exhausted);
        let mut diagnostics: Vec<_> = diagnostics
            .into_iter()
            .filter(|diagnostic| diagnostic_matches(diagnostic, &options))
            .collect();
        diagnostics.sort_by(|left, right| {
            diagnostic_group_key(left)
                .cmp(&diagnostic_group_key(right))
                .then_with(|| {
                    (left.range.start.line, left.range.start.character)
                        .cmp(&(right.range.start.line, right.range.start.character))
                })
        });

        let mut groups: Vec<Diagnostic> = Vec::new();
        for mut diagnostic in diagnostics {
            if let Some(group) = groups
                .last_mut()
                .filter(|group| diagnostic_group_key(group) == diagnostic_group_key(&diagnostic))
            {
                group.context.occurrence_count += 1;
                if options.preserve_locations {
                    group
                        .context
                        .occurrences
                        .push(diagnostic_location(&diagnostic));
                }
            } else {
                diagnostic.context.group_id = Some(diagnostic_group_id(&diagnostic));
                if options.preserve_locations {
                    let location = diagnostic_location(&diagnostic);
                    diagnostic.context.occurrences.push(location);
                }
                groups.push(diagnostic);
            }
        }

        let total_groups = groups.len();
        groups.truncate(options.item_limit);
        let returned_groups = groups.len();
        let returned_diagnostics = groups
            .iter()
            .map(|diagnostic| diagnostic.context.occurrence_count)
            .sum();
        DiagnosticsResult {
            diagnostics: groups,
            source_resource: None,
            total_diagnostics,
            returned_diagnostics,
            remaining_diagnostics: total_diagnostics.saturating_sub(returned_diagnostics),
            total_groups,
            returned_groups,
            omitted_groups: total_groups.saturating_sub(returned_groups),
            remaining_groups: total_groups.saturating_sub(returned_groups),
            next_cursor: None,
            snapshot_identity: None,
            max_bytes: Some(options.byte_limit),
            truncated: byte_truncated || returned_groups < total_groups,
            filters: options,
            cache: None,
        }
    }

    /// Handle server logs request.
    ///
    /// # Errors
    ///
    /// Returns an error if the `min_level` parameter is invalid.
    pub fn handle_server_logs(
        cache: &NotificationCache,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult> {
        Self::handle_server_logs_page(cache, limit, min_level, None)
    }

    /// Return one snapshot-bound page of server logs.
    pub fn handle_server_logs_page(
        cache: &NotificationCache,
        limit: usize,
        min_level: Option<String>,
        cursor: Option<&str>,
    ) -> Result<ServerLogsResult> {
        use crate::bridge::notifications::LogLevel;

        let min_level_filter = if let Some(level_str) = min_level {
            let level = match level_str.to_lowercase().as_str() {
                "error" => LogLevel::Error,
                "warning" => LogLevel::Warning,
                "info" => LogLevel::Info,
                "debug" => LogLevel::Debug,
                _ => {
                    return Err(Error::InvalidToolParams(format!(
                        "Invalid min_level: '{level_str}'. Valid values: error, warning, info, debug"
                    )));
                }
            };
            Some(level)
        } else {
            None
        };

        let all_logs = cache.logs();

        let filtered_logs: Vec<_> = all_logs
            .iter()
            .filter(|log| {
                min_level_filter.is_none_or(|min| match min {
                    LogLevel::Error => matches!(log.level, LogLevel::Error),
                    LogLevel::Warning => matches!(log.level, LogLevel::Error | LogLevel::Warning),
                    LogLevel::Info => !matches!(log.level, LogLevel::Debug),
                    LogLevel::Debug => true,
                })
            })
            .cloned()
            .collect();
        let snapshot_identity = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&filtered_logs).unwrap_or_default())
        );
        let (page, next_cursor) = notification_page_bounds(
            filtered_logs.len(),
            cursor,
            &snapshot_identity,
            "server_logs",
            limit,
        )?;
        let page_end = page.end;
        let logs = (limit > 0)
            .then(|| filtered_logs[page].to_vec())
            .unwrap_or_default();

        Ok(ServerLogsResult {
            returned: logs.len(),
            remaining: filtered_logs.len().saturating_sub(page_end),
            total: filtered_logs.len(),
            snapshot_identity,
            next_cursor,
            logs,
        })
    }

    /// Handle server messages request.
    ///
    /// # Errors
    ///
    /// This method does not return errors.
    pub fn handle_server_messages(
        cache: &NotificationCache,
        limit: usize,
    ) -> Result<ServerMessagesResult> {
        Self::handle_server_messages_page(cache, limit, None)
    }

    /// Return one snapshot-bound page of server messages.
    pub fn handle_server_messages_page(
        cache: &NotificationCache,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ServerMessagesResult> {
        let all_messages = cache.messages();
        let snapshot_identity = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&all_messages).unwrap_or_default())
        );
        let (page, next_cursor) = notification_page_bounds(
            all_messages.len(),
            cursor,
            &snapshot_identity,
            "server_messages",
            limit,
        )?;
        let page_end = page.end;
        let messages: Vec<_> = (limit > 0)
            .then(|| {
                all_messages
                    .iter()
                    .skip(page.start)
                    .take(page.end - page.start)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Ok(ServerMessagesResult {
            returned: messages.len(),
            remaining: all_messages.len().saturating_sub(page_end),
            total: all_messages.len(),
            snapshot_identity,
            next_cursor,
            messages,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::time::timeout;
    use url::Url;

    use super::*;
    use crate::bridge::translator::testing::*;
    use crate::config::{ServerId, ToolRouter};

    #[test]
    fn test_diagnostic_request_params_omit_optional_null_fields() {
        let uri = "file:///test.ts".parse().unwrap();
        let params = diagnostic_request_params(TextDocumentIdentifier { uri });
        let value = serde_json::to_value(params).unwrap();

        assert_eq!(value["textDocument"]["uri"], "file:///test.ts");
        assert!(value.get("identifier").is_none());
        assert!(value.get("previousResultId").is_none());
    }

    #[test]
    fn diagnostics_serialize_model_ready_metadata() {
        let diagnostic = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 2,
                    character: 3,
                },
                end: Position2D {
                    line: 2,
                    character: 8,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types".to_owned(),
            code: Some("E0308".to_owned()),
            context: DiagnosticContext {
                path: Some("/workspace/src/lib.rs".to_owned()),
                project_relative_path: Some("src/lib.rs".to_owned()),
                uri: "file:///workspace/src/lib.rs".to_owned(),
                source_frame: super::super::dto::SourceContext::Unavailable {
                    reason: super::super::dto::SourceUnavailableReason::NotFound,
                },
                occurrence_count: 1,
                fix_handles: Vec::new(),
                ..DiagnosticContext::default()
            },
        };

        let value = serde_json::to_value(diagnostic).unwrap();
        assert_eq!(value["project_relative_path"], "src/lib.rs");
        assert_eq!(value["source_frame"]["status"], "unavailable");
        assert_eq!(value["occurrence_count"], 1);
        assert_eq!(value["fix_handles"], serde_json::json!([]));
    }

    fn grouped_diagnostic(line: u32, code: &str, source: &str, path: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position2D { line, character: 1 },
                end: Position2D { line, character: 2 },
            },
            severity: DiagnosticSeverity::Hint,
            message: "inactive code".to_owned(),
            code: Some(code.to_owned()),
            context: DiagnosticContext {
                path: Some(path.to_owned()),
                project_relative_path: Some(path.to_owned()),
                uri: format!("file:///workspace/{path}"),
                diagnostic_source: Some(source.to_owned()),
                ..DiagnosticContext::default()
            },
        }
    }

    #[test]
    fn diagnostics_group_repeated_occurrences_and_preserve_locations() {
        let diagnostics = vec![
            grouped_diagnostic(8, "inactive-code", "rust-analyzer", "src/lib.rs"),
            grouped_diagnostic(3, "inactive-code", "rust-analyzer", "src/lib.rs"),
        ];
        let result = Translator::finish_diagnostics(
            diagnostics,
            DiagnosticOptions {
                preserve_locations: true,
                ..DiagnosticOptions::default()
            },
        );

        assert_eq!(result.total_diagnostics, 2);
        assert_eq!(result.total_groups, 1);
        assert_eq!(result.returned_diagnostics, 2);
        assert_eq!(result.diagnostics[0].context.occurrence_count, 2);
        assert_eq!(result.diagnostics[0].context.occurrences.len(), 2);
    }

    #[test]
    fn diagnostics_keep_occurrence_identities_compact() {
        let diagnostics = (0..249)
            .map(|line| {
                let mut diagnostic =
                    grouped_diagnostic(line, "macro-error", "rust-analyzer", "src/lib.rs");
                diagnostic.context.source_frame = super::super::dto::SourceContext::Deferred {
                    resource: super::super::dto::DeferredResourceReference {
                        uri: format!(
                            "mcpls-source:///workspace/src/lib.rs?start_line={line}&snapshot={}",
                            "a".repeat(64)
                        ),
                        kind: "source_context".to_owned(),
                        snapshot_hash: "a".repeat(64),
                        document_version: Some(1),
                        total_bytes: Some(512),
                    },
                };
                diagnostic
            })
            .collect();
        let result = Translator::finish_diagnostics(
            diagnostics,
            DiagnosticOptions {
                preserve_locations: true,
                item_limit: 20,
                byte_limit: 6_000,
                ..DiagnosticOptions::default()
            },
        );

        let encoded = serde_json::to_vec(&result).unwrap();
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(result.diagnostics[0].context.occurrences.len(), 249);
        assert!(
            value["diagnostics"][0]["occurrences"][0]
                .get("source")
                .is_none()
        );
        assert!(encoded.len() < 64 * 1024, "{} bytes", encoded.len());
    }

    #[test]
    fn diagnostics_filters_and_item_limit_report_omissions() {
        let diagnostics = vec![
            grouped_diagnostic(1, "inactive-code", "rust-analyzer", "src/lib.rs"),
            grouped_diagnostic(2, "dead_code", "clippy", "target/out.rs"),
            grouped_diagnostic(3, "unused", "rustc", "src/main.rs"),
        ];
        let result = Translator::finish_diagnostics(
            diagnostics,
            DiagnosticOptions {
                sources: vec!["rustc".to_owned()],
                item_limit: 0,
                ..DiagnosticOptions::default()
            },
        );

        assert_eq!(result.total_diagnostics, 3);
        assert_eq!(result.total_groups, 1);
        assert_eq!(result.returned_groups, 0);
        assert_eq!(result.omitted_groups, 1);
        assert!(result.truncated);
    }

    #[tokio::test]
    async fn diagnostic_conversion_preserves_metadata_and_source_safely() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() { missing(); }\n").unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&path)
            .unwrap()
            .to_string()
            .parse()
            .unwrap();
        let location = lsp_types::Location {
            uri: uri.clone(),
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, 12),
                lsp_types::Position::new(0, 19),
            ),
        };
        let diagnostic = lsp_types::Diagnostic {
            range: location.range,
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: Some(lsp_types::NumberOrString::String("E0425".to_owned())),
            code_description: Some(lsp_types::CodeDescription {
                href: "https://doc.rust-lang.org/error_codes/E0425.html"
                    .parse()
                    .unwrap(),
            }),
            source: Some("rustc".to_owned()),
            message: "cannot find function".to_owned(),
            related_information: Some(vec![lsp_types::DiagnosticRelatedInformation {
                location,
                message: "called here".to_owned(),
            }]),
            tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
            data: Some(serde_json::json!({"password": "secret", "kind": "quickfix"})),
        };
        let ctx = EncodingCtx {
            encoding: PositionEncoding::Utf16,
            tracker: test_tracker(),
            approved_source_paths: Arc::new(
                std::sync::Mutex::new(std::collections::HashSet::new()),
            ),
        };
        let mut budget = SourceBudget::default();
        let converted = diagnostic_to_mcp(
            &diagnostic,
            &ctx,
            &uri,
            &[dir.path().to_path_buf()],
            &RedactionPolicy::default(),
            &mut budget,
        )
        .await;

        assert!(matches!(
            converted.context.source_frame,
            super::super::dto::SourceContext::Available(_)
        ));
        assert!(matches!(
            converted.context.related_information[0].location.source,
            super::super::dto::SourceContext::Available(_)
        ));
        assert_eq!(
            converted.context.diagnostic_source.as_deref(),
            Some("rustc")
        );
        assert_eq!(converted.context.tags, ["unnecessary"]);
        assert_eq!(converted.context.data.unwrap()["password"], "[REDACTED]");
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_empty() {
        let cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 0);
    }

    #[test]
    fn test_handle_server_logs_with_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        // Add some logs
        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        // Test with error filter
        let result = Translator::handle_server_logs(&cache, 10, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 1);
        assert_eq!(logs.logs[0].message, "error msg");

        // Test with warning filter (includes error and warning)
        let result = Translator::handle_server_logs(&cache, 10, Some("warning".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);

        // Test with info filter (excludes debug)
        let result = Translator::handle_server_logs(&cache, 10, Some("info".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 3);

        // Test with debug filter (includes all)
        let result = Translator::handle_server_logs(&cache, 10, Some("debug".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);

        // Test with invalid filter
        let result = Translator::handle_server_logs(&cache, 10, Some("invalid".to_string()));
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[test]
    fn test_handle_server_messages_limit() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        // Add some messages
        for i in 0..10 {
            cache.store_message(MessageType::Info, format!("message {i}"));
        }

        // Test limit
        let result = Translator::handle_server_messages(&cache, 5);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 5);
        assert_eq!(messages.messages[0].message, "message 0");
        assert_eq!(messages.messages[4].message, "message 4");

        // Test limit larger than available
        let result = Translator::handle_server_messages(&cache, 100);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 10);
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_with_data() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: Some(lsp_types::NumberOrString::String("E001".to_string())),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&ServerId::from("rust"), &uri, Some(1), vec![diagnostic]);

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 1);
        assert_eq!(diags.diagnostics[0].message, "test error");
        assert_eq!(diags.diagnostics[0].code, Some("E001".to_string()));
        assert!(matches!(
            diags.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert_eq!(diags.diagnostics[0].range.start.line, 1);
        assert_eq!(diags.diagnostics[0].range.start.character, 1);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn test_handle_cached_diagnostics_multiple_severities() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostics = vec![
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "error".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                message: "warning".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 2,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::INFORMATION),
                message: "info".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::HINT),
                message: "hint".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        cache.store_diagnostics(&ServerId::from("rust"), &uri, Some(1), diagnostics);

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 4);
        assert!(matches!(
            diags.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert!(matches!(
            diags.diagnostics[1].severity,
            DiagnosticSeverity::Warning
        ));
        assert!(matches!(
            diags.diagnostics[2].severity,
            DiagnosticSeverity::Information
        ));
        assert!(matches!(
            diags.diagnostics[3].severity,
            DiagnosticSeverity::Hint
        ));
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_with_numeric_code() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: Some(lsp_types::NumberOrString::Number(42)),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&ServerId::from("rust"), &uri, Some(1), vec![diagnostic]);

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 1);
        assert_eq!(diags.diagnostics[0].code, Some("42".to_string()));
    }

    #[test]
    fn test_handle_cached_diagnostics_invalid_path() {
        let result = Translator::cached_diagnostics_uri(&[], "/nonexistent/path/file.rs");
        assert!(matches!(result, Err(Error::FileIo { .. })));
    }

    #[tokio::test]
    async fn test_merge_diagnostics_cache_only_appends_to_empty_pull() {
        let pull = DiagnosticsResult::raw(vec![]);
        let cache = diag_info(vec![lsp_diag(
            0,
            10,
            lsp_types::DiagnosticSeverity::WARNING,
            "unused import: `std::fmt`",
            None,
        )]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 1);
        assert_eq!(merged.diagnostics[0].message, "unused import: `std::fmt`");
        assert!(matches!(
            merged.diagnostics[0].severity,
            DiagnosticSeverity::Warning
        ));
    }

    #[tokio::test]
    async fn test_merge_diagnostics_exact_duplicate_not_repeated() {
        // Same range/severity/message/code as the cache entry below, expressed
        // in the 1-based MCP shape `diagnostics_from_cache_entry` would produce.
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 11,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types".to_string(),
            code: Some("E0308".to_string()),
            context: DiagnosticContext::default(),
        };
        let pull = DiagnosticsResult::raw(vec![pull_diag.clone()]);
        let cache = diag_info(vec![lsp_diag(
            0,
            10,
            lsp_types::DiagnosticSeverity::ERROR,
            "mismatched types",
            Some("E0308"),
        )]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 1);
        assert_eq!(merged.diagnostics[0], pull_diag);
    }

    #[tokio::test]
    async fn test_merge_diagnostics_no_cache_entry_returns_pull_unchanged() {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 5,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "syntax error".to_string(),
            code: None,
            context: DiagnosticContext::default(),
        };
        let pull = DiagnosticsResult::raw(vec![pull_diag.clone()]);

        let merged =
            Translator::merge_diagnostics(pull, None, PositionEncoding::Utf16, &test_tracker())
                .await;

        assert_eq!(merged.diagnostics, vec![pull_diag]);
    }

    #[tokio::test]
    async fn test_merge_diagnostics_multiple_distinct_cache_entries_all_appear() {
        let pull = DiagnosticsResult::raw(vec![]);
        let cache = diag_info(vec![
            lsp_diag(
                0,
                10,
                lsp_types::DiagnosticSeverity::WARNING,
                "unused import: `std::fmt`",
                None,
            ),
            lsp_diag(
                5,
                8,
                lsp_types::DiagnosticSeverity::WARNING,
                "function `helper` is never used",
                None,
            ),
        ]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 2);
        assert!(
            merged
                .diagnostics
                .iter()
                .any(|d| d.message == "unused import: `std::fmt`")
        );
        assert!(
            merged
                .diagnostics
                .iter()
                .any(|d| d.message == "function `helper` is never used")
        );
    }

    #[tokio::test]
    async fn test_merge_diagnostics_same_range_different_message_not_deduped() {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 11,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types".to_string(),
            code: None,
            context: DiagnosticContext::default(),
        };
        let pull = DiagnosticsResult::raw(vec![pull_diag]);
        // Same range and severity as the pull diagnostic, but a different
        // message — must be treated as a distinct diagnostic, not a duplicate.
        let cache = diag_info(vec![lsp_diag(
            0,
            10,
            lsp_types::DiagnosticSeverity::ERROR,
            "expected `i32`, found `&str`",
            None,
        )]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 2);
    }

    /// Pins a cross-model duplicate shape verified empirically against a live
    /// rust-analyzer 1.97.1 session (#244): the pull and push diagnostics for
    /// the *same* "not all trait items implemented" (E0046) error had
    /// different ranges (trait name vs. impl block) and different messages
    /// (terse vs. rustc's full rendering), but shared `code` and `severity`.
    /// Exact-field dedup would report this twice; the `(severity, code)`
    /// fingerprint must collapse it to one entry.
    #[tokio::test]
    async fn test_merge_diagnostics_same_code_different_range_and_message_deduped() {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 96,
                    character: 7,
                },
                end: Position2D {
                    line: 96,
                    character: 12,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "not all trait items implemented, missing: `fn hello`".to_string(),
            code: Some("E0046".to_string()),
            context: DiagnosticContext::default(),
        };
        let pull = DiagnosticsResult::raw(vec![pull_diag.clone()]);
        // Same code and severity, but a different range and a longer,
        // differently-worded message -- the rustc-rendered push side of the
        // same underlying error.
        let cache = diag_info(vec![lsp_diag(
            94,
            31,
            lsp_types::DiagnosticSeverity::ERROR,
            "not all trait items implemented, missing: `hello`\nmissing `hello` in implementation",
            Some("E0046"),
        )]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 1);
        assert_eq!(merged.diagnostics[0], pull_diag);
    }

    /// Regression: `merge_diagnostics`'s `(severity, code)` fingerprint alone
    /// is coarser than full-field equality and cannot tell apart two
    /// genuinely distinct diagnostics that happen to share `code` and
    /// `severity` -- e.g. two separate `E0308` mismatched-type errors at
    /// different locations in the same file, one caught only by native
    /// (pull) analysis and a second, unrelated one caught only by
    /// flycheck/cargo check (cache), such as an error inside macro-expanded
    /// code the native pass did not evaluate. This previously caused the
    /// cache-only entry to be silently dropped -- reproducing #244's exact
    /// failure mode, just relocated from "no merge" to "over-eager dedup".
    ///
    /// The range-proximity check on `is_duplicate` (see `merge_diagnostics`)
    /// closes this: these two diagnostics are 45 lines apart, far outside
    /// `DUPLICATE_RANGE_PROXIMITY_LINES`, so both must survive the merge.
    #[tokio::test]
    async fn test_merge_diagnostics_same_code_distinct_diagnostics_at_different_locations_both_kept()
     {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 5,
                    character: 9,
                },
                end: Position2D {
                    line: 5,
                    character: 20,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types: expected `i32`, found `&str`".to_string(),
            code: Some("E0308".to_string()),
            context: DiagnosticContext::default(),
        };
        let pull = DiagnosticsResult::raw(vec![pull_diag.clone()]);
        // A second, unrelated E0308 at a completely different location with
        // a completely different message -- a real, distinct diagnostic,
        // not a duplicate of pull_diag.
        let cache = diag_info(vec![lsp_diag(
            49,
            22,
            lsp_types::DiagnosticSeverity::ERROR,
            "mismatched types: expected `String`, found `Vec<u8>`",
            Some("E0308"),
        )]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 2);
        assert_eq!(merged.diagnostics[0], pull_diag);
        assert_eq!(
            merged.diagnostics[1].message,
            "mismatched types: expected `String`, found `Vec<u8>`"
        );
    }

    #[test]
    fn test_handle_server_logs_no_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, None);
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_logs_error_filter_strict() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 1);
        assert_eq!(logs.logs[0].message, "error msg");
    }

    #[test]
    fn test_handle_server_logs_warning_filter_includes_errors() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("warning".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);
    }

    #[test]
    fn test_handle_server_logs_info_filter_excludes_debug() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("info".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);
    }

    #[test]
    fn test_handle_server_logs_debug_filter_includes_all() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("debug".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_logs_limit_applies_after_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        for i in 0..10 {
            cache.store_log(LogLevel::Error, format!("error {i}"));
        }

        let result = Translator::handle_server_logs(&cache, 5, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 5);
        assert_eq!(logs.logs[0].message, "error 0");
        assert_eq!(logs.logs[4].message, "error 4");
    }

    #[test]
    fn test_handle_server_logs_case_insensitive_level() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("ERROR".to_string()));
        assert!(result.is_ok());

        let result = Translator::handle_server_logs(&cache, 10, Some("Error".to_string()));
        assert!(result.is_ok());

        let result = Translator::handle_server_logs(&cache, 10, Some("eRrOr".to_string()));
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_server_messages_empty() {
        let cache = NotificationCache::new();

        let result = Translator::handle_server_messages(&cache, 10);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 0);
    }

    #[test]
    fn test_handle_server_messages_with_different_types() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Error, "error".to_string());
        cache.store_message(MessageType::Warning, "warning".to_string());
        cache.store_message(MessageType::Info, "info".to_string());
        cache.store_message(MessageType::Log, "log".to_string());

        let result = Translator::handle_server_messages(&cache, 10);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 4);
        assert_eq!(messages.messages[0].message, "error");
        assert_eq!(messages.messages[1].message, "warning");
        assert_eq!(messages.messages[2].message, "info");
        assert_eq!(messages.messages[3].message, "log");
    }

    #[test]
    fn notification_pages_are_complete_and_reject_mutated_cursors() {
        use crate::bridge::notifications::{LogLevel, MessageType};

        let mut cache = NotificationCache::new();
        for index in 0..101 {
            cache.store_log(LogLevel::Info, format!("log {index}"));
            cache.store_message(MessageType::Info, format!("message {index}"));
        }

        let first = Translator::handle_server_logs_page(&cache, 50, None, None).unwrap();
        assert_eq!(
            (first.total, first.returned, first.remaining),
            (100, 50, 50)
        );
        let cursor = first.next_cursor.clone().unwrap();
        let second = Translator::handle_server_logs_page(&cache, 50, None, Some(&cursor)).unwrap();
        assert_eq!(
            (second.total, second.returned, second.remaining),
            (100, 50, 0)
        );
        assert!(second.next_cursor.is_none());
        assert_eq!(first.logs[0].message, "log 1");
        assert_eq!(second.logs[0].message, "log 51");

        let first = Translator::handle_server_messages_page(&cache, 50, None).unwrap();
        assert_eq!((first.total, first.returned, first.remaining), (50, 50, 0));
        assert!(first.next_cursor.is_none());

        cache.store_log(LogLevel::Info, "new log".to_owned());
        assert!(Translator::handle_server_logs_page(&cache, 50, None, Some(&cursor)).is_err());
    }

    #[test]
    fn test_handle_server_messages_zero_limit() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Info, "test".to_string());

        let result = Translator::handle_server_messages(&cache, 0);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 0);
        assert_eq!(messages.total, 1);
        assert_eq!(messages.remaining, 1);
        assert!(messages.next_cursor.is_none());
    }

    #[test]
    fn test_handle_cached_diagnostics_path_outside_workspace() {
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        let workspace_roots = vec![temp_dir1.path().to_path_buf()];

        let test_file = temp_dir2.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result =
            Translator::cached_diagnostics_uri(&workspace_roots, test_file.to_str().unwrap());
        assert!(matches!(result, Err(Error::PathOutsideWorkspace(_))));
    }

    /// S1 regression (#244): a push-only server (or one that times out)
    /// answering `textDocument/diagnostic` with an LSP error must not
    /// discard diagnostics `handle_diagnostics` already knows about from the
    /// cache -- it should return the cache-only result instead of `Err`.
    #[tokio::test]
    async fn test_handle_diagnostics_pull_error_falls_back_to_nonempty_cache() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("rs".to_string(), "rust".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from("rust"),
                    "rust".to_string(),
                )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client, mut server) = fake_lsp_client();
        translator.register_client("rust".to_string(), client);

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        // Prime the cache under the exact URI handle_diagnostics will look
        // up (path_to_uri over the canonicalized path, same as
        // document_tracker uses to open the document).
        let canonical = path.canonicalize().unwrap();
        let uri = path_to_uri(&canonical).unwrap();
        let notification_cache = Mutex::new(NotificationCache::new());
        {
            let mut cache = notification_cache.lock().await;
            cache.store_diagnostics(
                &ServerId::from("rust"),
                &uri,
                Some(1),
                vec![lsp_diag(
                    0,
                    4,
                    lsp_types::DiagnosticSeverity::WARNING,
                    "unused import: `std::fmt`",
                    None,
                )],
            );
        }

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(path_str, &notification_cache)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_error_response(
            &mut server.read_half_stdin,
            &diag_request["id"],
            -32601,
            "method not found",
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        let diagnostics = result.expect("cache-only fallback should succeed despite pull error");
        assert_eq!(diagnostics.diagnostics.len(), 1);
        assert_eq!(
            diagnostics.diagnostics[0].message,
            "unused import: `std::fmt`"
        );
    }

    /// S1 counterpart: when the cache is also empty, the pull error must
    /// still propagate -- there is nothing to fall back to.
    #[tokio::test]
    async fn test_handle_diagnostics_pull_error_and_empty_cache_propagates_error() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("rs".to_string(), "rust".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from("rust"),
                    "rust".to_string(),
                )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client, mut server) = fake_lsp_client();
        translator.register_client("rust".to_string(), client);

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let notification_cache = Mutex::new(NotificationCache::new());

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(path_str, &notification_cache)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_error_response(
            &mut server.read_half_stdin,
            &diag_request["id"],
            -32601,
            "method not found",
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        assert!(
            result.is_err(),
            "pull error with no cache data must propagate, got {result:?}"
        );
    }
}
