use std::fs;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::io::BufReader;
use tokio::sync::Mutex as TokioMutex;

#[allow(clippy::wildcard_imports)]
use super::*;
#[allow(clippy::wildcard_imports)]
use super::{actor::*, identity::*, registry::*, runtime::*, state::*};

#[tokio::test]
async fn lexical_search_skips_non_utf8_files() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("binary.dat"), [0xff]).unwrap();
    fs::write(
        root.path().join("source.rs"),
        "fn status_chip() {}\nfn status_chip_again() {}\n",
    )
    .unwrap();
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let runtime = ProjectRuntime::new(translator);

    let scan = runtime
        .lexical_search(LexicalSearchRequest {
            query: "status_chip".to_owned(),
            mode: crate::bridge::LexicalMatchMode::Literal,
            case: crate::bridge::LexicalCaseMode::Sensitive,
            multiline: false,
            max_files: 10,
            max_matches: 1,
            include_generated: false,
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            context_lines: 0,
            page_token: None,
        })
        .await
        .unwrap();

    assert_eq!(scan.matches.len(), 1);
    assert_eq!(scan.total_matches, 2);
    assert_eq!(scan.scanned_files, 2);
    assert_eq!(scan.matches[0].project_relative_path, "source.rs");
    assert!(!scan.scan_truncated);
}

#[tokio::test]
async fn lexical_search_reports_file_scan_truncation_separately_from_match_pages() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a.rs", "b.rs", "c.rs"] {
        fs::write(root.path().join(name), "fn marker() {}\n").unwrap();
    }
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let runtime = ProjectRuntime::new(translator);

    let scan = runtime
        .lexical_search(LexicalSearchRequest {
            query: "absent".to_owned(),
            mode: crate::bridge::LexicalMatchMode::Literal,
            case: crate::bridge::LexicalCaseMode::Sensitive,
            multiline: false,
            max_files: 2,
            max_matches: 10,
            include_generated: false,
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            context_lines: 0,
            page_token: None,
        })
        .await
        .unwrap();

    assert_eq!(scan.scanned_files, 2);
    assert_eq!(scan.total_matches, 0);
    assert!(!scan.matches.iter().any(|entry| entry.source.is_some()));
    assert!(scan.scan_truncated);
}

#[tokio::test]
async fn workspace_symbol_snapshot_skips_non_utf8_files() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("binary.dat"), [0xff]).unwrap();
    fs::write(root.path().join("source.rs"), "fn status_chip() {}\n").unwrap();
    let mut translator = Translator::new()
        .with_extensions(HashMap::from([(String::from("rs"), String::from("rust"))]));
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let runtime = ProjectRuntime::new(translator);

    let result = runtime
        .workspace_symbol_page(WorkspaceSymbolPageRequest {
            query: "status_chip".to_owned(),
            kind_filter: None,
            match_mode: WorkspaceSymbolMatchMode::Fuzzy,
            scope: WorkspaceSymbolScope::Project,
            include_generated: false,
            max_items: 10,
            max_bytes: 16 * 1024,
            page_token: None,
        })
        .await;

    assert!(
        result.is_ok(),
        "binary files must not make workspace search fail"
    );
}

#[test]
fn invalid_utf8_file_errors_are_skipped_through_both_error_wrappers() {
    let source = std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid UTF-8");
    assert!(is_invalid_utf8_error(&crate::error::Error::Io(source)));

    let source = std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid UTF-8");
    assert!(is_invalid_utf8_error(&crate::error::Error::FileIo {
        path: "binary.dat".into(),
        source,
    }));

    let source = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
    assert!(!is_invalid_utf8_error(&crate::error::Error::Io(source)));
}

#[tokio::test]
async fn workspace_snapshot_identity_is_stable_and_changes_with_content() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..64 {
        fs::write(
            root.path().join(format!("source-{index:02}.rs")),
            format!("fn snapshot_marker_{index}() {{}}\n"),
        )
        .unwrap();
    }
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let runtime = ProjectRuntime::new(translator);

    let first = runtime.workspace_snapshot_identity().await.unwrap();
    let second = runtime.workspace_snapshot_identity().await.unwrap();
    assert_eq!(first, second, "snapshot hashing must be deterministic");

    fs::write(
        root.path().join("source-17.rs"),
        "fn changed_snapshot_marker() {}\n",
    )
    .unwrap();
    let changed = runtime.workspace_snapshot_identity().await.unwrap();
    assert_ne!(first, changed, "disk changes must invalidate the identity");
}

#[tokio::test]
async fn lexical_search_pages_replay_one_immutable_snapshot() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.rs"), "fn marker() {}\n").unwrap();
    fs::write(root.path().join("b.rs"), "fn marker() {}\n").unwrap();
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let runtime = ProjectRuntime::new(translator);
    let request = || LexicalSearchRequest {
        query: "marker".to_owned(),
        mode: crate::bridge::LexicalMatchMode::Literal,
        case: crate::bridge::LexicalCaseMode::Sensitive,
        multiline: false,
        max_files: 10,
        max_matches: 1,
        include_generated: false,
        include_paths: Vec::new(),
        exclude_paths: Vec::new(),
        context_lines: 0,
        page_token: None,
    };

    let first = runtime.lexical_search(request()).await.unwrap();
    assert_eq!(first.total_matches, 2);
    assert_eq!(first.matches.len(), 1);
    assert_eq!(first.offset, 0);
    fs::write(root.path().join("a.rs"), "fn changed() {}\n").unwrap();

    let second = runtime
        .lexical_search(LexicalSearchRequest {
            page_token: Some(crate::project::lexical_page_cursor(
                &first.page_token,
                first.offset + first.matches.len(),
            )),
            ..request()
        })
        .await
        .unwrap();
    assert_eq!(second.total_matches, 2);
    assert_eq!(second.matches.len(), 1);
    assert_eq!(second.offset, 1);
    assert_eq!(second.snapshot_identity, first.snapshot_identity);
    assert_ne!(second.matches[0].source_uri, first.matches[0].source_uri);
}

#[tokio::test]
async fn lexical_search_batch_shares_snapshot_scan_and_match_budget() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("source.rs"), "marker needle marker\n").unwrap();
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let runtime = ProjectRuntime::new(translator);

    let batch = runtime
        .lexical_search_batch(LexicalSearchBatchRequest {
            queries: vec![
                "marker".to_owned(),
                "needle".to_owned(),
                "marker".to_owned(),
            ],
            mode: crate::bridge::LexicalMatchMode::Literal,
            case: crate::bridge::LexicalCaseMode::Sensitive,
            multiline: false,
            max_files: 10,
            max_matches: 2,
            include_generated: false,
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            context_lines: 0,
            max_bytes: 16 * 1024,
        })
        .await
        .unwrap();

    assert_eq!(batch.unique_queries, 2);
    assert_eq!(batch.scanned_files, 1);
    assert_eq!(batch.entries.len(), 3);
    assert_eq!(batch.entries[0].result.as_ref().unwrap().returned, 2);
    assert!(batch.entries[1].skipped_by_budget);
    assert_eq!(batch.entries[2].reused_from, Some(0));
}

#[tokio::test]
async fn server_exit_forwarder_does_not_pin_after_intentional_shutdown() {
    let (request_sender, mut request_receiver) = mpsc::channel(1);
    let (notification_sender, notification_receiver) = mpsc::channel(1);
    let gate = ProjectRequestGate::new();

    let forwarder = tokio::spawn(forward_lsp_notifications(
        "rust".into(),
        notification_receiver,
        request_sender.downgrade(),
        gate,
        7,
    ));
    drop(notification_sender);

    let request = tokio::time::timeout(Duration::from_secs(1), request_receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        request,
        ProjectRequest::ServerExited { generation: 7 }
    ));
    forwarder.await.unwrap();
}

#[tokio::test]
async fn server_exit_forwarder_suppresses_exit_after_shutdown_begins() {
    let (request_sender, mut request_receiver) = mpsc::channel(1);
    let (notification_sender, notification_receiver) = mpsc::channel(1);
    let gate = ProjectRequestGate::new();

    let forwarder = tokio::spawn(forward_lsp_notifications(
        "rust".into(),
        notification_receiver,
        request_sender.downgrade(),
        gate.clone(),
        8,
    ));
    gate.reject_new_work();
    drop(notification_sender);

    assert!(
        tokio::time::timeout(Duration::from_millis(50), request_receiver.recv())
            .await
            .is_err()
    );
    forwarder.await.unwrap();
}

#[tokio::test]
async fn deferred_resource_reads_survive_actor_lifecycle_changes() {
    let registry = ProjectRegistry::new(2);
    let reference = registry.deferred_results.lock().unwrap().insert_scoped(
        serde_json::json!({"references": [1, 2]}),
        "snapshot".to_owned(),
        "",
    );

    let token = reference
        .uri
        .strip_prefix("mcpls-deferred:///")
        .unwrap()
        .to_owned();
    let payload = registry.read_deferred_resource(&token).unwrap();
    assert_eq!(payload.value, serde_json::json!({"references": [1, 2]}));
    assert_eq!(payload.snapshot_hash, "snapshot");
}

#[test]
fn deferred_resource_invalidation_is_scoped_to_a_project() {
    let mut store = DeferredResultStore::new();
    let first = store.insert_scoped(
        serde_json::json!({"project": "first"}),
        "snapshot-first".to_owned(),
        "first",
    );
    let second = store.insert_scoped(
        serde_json::json!({"project": "second"}),
        "snapshot-second".to_owned(),
        "second",
    );
    let first_token = first.uri.strip_prefix("mcpls-deferred:///").unwrap();
    let second_token = second.uri.strip_prefix("mcpls-deferred:///").unwrap();

    store.invalidate_scope("first");

    assert!(store.read(first_token).is_err());
    assert_eq!(
        store.read(second_token).unwrap().value,
        serde_json::json!({"project": "second"})
    );
}
#[test]
fn deferred_resource_reads_reject_wrong_scope() {
    let mut store = DeferredResultStore::new();
    let reference = store.insert_scoped(
        serde_json::json!({"project": "first"}),
        "snapshot-first".to_owned(),
        "first",
    );
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();

    assert!(store.read_scoped(token, "second").is_err());
    assert_eq!(
        store.read_scoped(token, "first").unwrap(),
        serde_json::json!({"project": "first"})
    );
}
#[tokio::test]
async fn call_hierarchy_cursor_pages_preserve_counts_and_identity() {
    let items: Vec<_> = (0..65)
        .map(|index| {
            serde_json::json!({
                "name": format!("item-{index}"),
                "kind": 12,
                "uri": format!("file:///item-{index}.rs"),
                "range": {
                    "start": {"line": 1, "character": 1},
                    "end": {"line": 1, "character": 4}
                },
                "selectionRange": {
                    "start": {"line": 1, "character": 1},
                    "end": {"line": 1, "character": 4}
                },
                "path": format!("/item-{index}.rs"),
                "source": {
                    "status": "deferred",
                    "resource": {
                        "uri": format!("mcpls-source:///item-{index}.rs"),
                        "kind": "source_context",
                        "snapshot_hash": "snapshot"
                    }
                },
                "symbol_handle": format!("handle-{index}")
            })
        })
        .collect();
    let deferred_results = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let runtime = ProjectRuntime::with_deferred_results_scoped(
        Translator::new(),
        None,
        deferred_results,
        Some("project".to_owned()),
    );
    let reference = runtime.deferred_results.lock().unwrap().insert_scoped(
        serde_json::json!({
            "provider": "standard_lsp",
            "kind": "call_hierarchy",
            "total_items": 65,
            "truncated": false,
            "snapshot_hash": "snapshot",
            "items": items,
        }),
        "snapshot".to_owned(),
        "project",
    );

    let first = runtime
        .prepare_call_hierarchy(String::new(), 0, 0, Some(reference.uri))
        .await
        .unwrap();
    assert_eq!(first.total_items, 65);
    assert_eq!(first.returned_items, 64);
    assert_eq!(first.items.first().unwrap().name, "item-0");
    assert_eq!(first.items.last().unwrap().name, "item-63");
    let next_cursor = first.next_cursor.clone().unwrap();

    let second = runtime
        .prepare_call_hierarchy(String::new(), 0, 0, Some(next_cursor))
        .await
        .unwrap();
    assert_eq!(second.total_items, 65);
    assert_eq!(second.returned_items, 1);
    assert_eq!(second.items[0].name, "item-64");
    assert_eq!(second.items[0].path.as_deref(), Some("/item-64.rs"));
    assert_eq!(
        second.items[0]
            .symbol_handle
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("handle-64")
    );
    assert!(matches!(
        second.items[0].source.as_ref(),
        Some(crate::bridge::SourceContext::Deferred { resource })
            if resource.snapshot_hash == "snapshot"
    ));
    assert!(second.next_cursor.is_none());
}

#[tokio::test]
async fn server_exit_recovery_acquires_residency_after_exit_is_authoritative() {
    let controller = RustResidencyController::new(1);
    let (second_sender, mut second_receiver) = mpsc::channel(1);
    controller.register(RustGroupId(2), second_sender.downgrade());
    let actor = spawn_project_actor_with_runtime(
        2,
        Translator::new(),
        None,
        Some(ProjectResidency {
            controller: controller.clone(),
            group: RustGroupId(1),
        }),
    );
    actor.set_status(ProjectStatus::Ready).await.unwrap();
    let mut events = actor.subscribe_events();
    let second_guard = controller.acquire(RustGroupId(2)).await;

    actor
        .sender
        .sender
        .send(ProjectRequest::ServerExited { generation: 0 })
        .await
        .unwrap();
    assert_eq!(
        events.recv().await.unwrap(),
        ProjectEvent::ServerExited { generation: 0 }
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );

    drop(second_guard);
    let Some(ProjectRequest::Suspend { reply, .. }) = second_receiver.recv().await else {
        panic!("expected the pinned resident group to be evicted");
    };
    reply.send(Ok(())).unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap(),
        ProjectEvent::StatusChanged {
            status: ProjectStatus::Restarting,
            ..
        }
    ));
    actor.shutdown().await.unwrap();
}

#[tokio::test]
async fn server_exit_recovery_does_not_wait_behind_eviction_transition() {
    let controller = RustResidencyController::with_idle_timeout(1, Duration::ZERO);
    let (victim_sender, mut victim_receiver) = mpsc::channel(1);
    let (replacement_sender, _replacement_receiver) = mpsc::channel(1);
    controller.register(RustGroupId(1), victim_sender.downgrade());
    controller.register(RustGroupId(2), replacement_sender.downgrade());
    drop(controller.acquire(RustGroupId(1)).await);

    let residency = ProjectResidency {
        controller: controller.clone(),
        group: RustGroupId(1),
    };
    let (actor_sender, _actor_receiver) = mpsc::channel(1);
    let (status_tx, _) = watch::channel(ProjectStatus::Ready);
    let (state_tx, _) = watch::channel(ProjectState::new(
        ProjectStatus::Ready,
        ProjectRuntimeSummary::default(),
    ));
    let (event_tx, _) = broadcast::channel(1);
    let channels = ProjectActorChannels {
        status_tx,
        state_tx,
        event_tx,
        event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
        gate: ProjectRequestGate::new(),
    };
    let mut state = ProjectState::new(ProjectStatus::Ready, ProjectRuntimeSummary::default());
    let mut runtime = ProjectRuntime::new(Translator::new());

    let replacement = tokio::spawn({
        let controller = controller.clone();
        async move { controller.acquire(RustGroupId(2)).await }
    });
    let Some(ProjectRequest::Suspend { reply, .. }) = victim_receiver.recv().await else {
        panic!("expected eviction to suspend the victim");
    };

    let recovery = Box::pin(tokio::time::timeout(
        Duration::from_secs(1),
        handle_server_exit(
            0,
            &actor_sender.downgrade(),
            &channels,
            &mut state,
            &mut runtime,
            Some(&residency),
        ),
    ))
    .await;
    assert!(recovery.is_ok(), "server-exit recovery deadlocked");

    reply.send(Ok(())).unwrap();
    drop(replacement.await.unwrap());
}

#[test]
fn code_action_store_references_are_bounded_and_single_use() {
    let mut store = CodeActionStore {
        entries: HashMap::new(),
        ttl: Duration::from_secs(60),
        max_entries: 1,
    };
    let first = store.insert(StoredCodeAction {
        file_path: "first.rs".to_string(),
        action: lsp_types::CodeActionOrCommand::Command(lsp_types::Command {
            title: "first".to_string(),
            command: "first".to_string(),
            arguments: None,
        }),
        created_at: Instant::now(),
    });
    let second = store.insert(StoredCodeAction {
        file_path: "second.rs".to_string(),
        action: lsp_types::CodeActionOrCommand::Command(lsp_types::Command {
            title: "second".to_string(),
            command: "second".to_string(),
            arguments: None,
        }),
        created_at: Instant::now(),
    });
    assert!(store.take(&first).is_err());
    assert!(store.take(&second).is_ok());
    assert!(store.take(&second).is_err());
}

#[test]
fn format_document_results_are_atomic_when_edits_do_not_fit() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let edits = (0..2)
        .map(|index| crate::bridge::translator::TextEdit {
            range: crate::bridge::translator::Range {
                start: crate::bridge::translator::Position2D {
                    line: index + 1,
                    character: 1,
                },
                end: crate::bridge::translator::Position2D {
                    line: index + 1,
                    character: 2,
                },
            },
            new_text: "λ".repeat(MAX_NOTIFICATION_RESULT_BYTES),
        })
        .collect::<Vec<_>>();
    let complete = serde_json::to_value(&edits).unwrap();
    let mut result = FormatDocumentResult {
        edits,
        total_edits: 0,
        returned_edits: 0,
        edit_bytes: 0,
        edit_digest: String::new(),
        edits_resource: None,
        deferred: false,
    };

    bound_format_document_result(&mut result, &shared, "project").unwrap();

    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_NOTIFICATION_RESULT_BYTES);
    assert!(result.deferred);
    assert_eq!(result.returned_edits, 0);
    assert_eq!(result.total_edits, 2);
    assert!(result.edits.is_empty());
    let reference = result.edits_resource.as_ref().unwrap();
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .read_scoped(token, "project")
            .unwrap(),
        complete
    );
}

#[test]
fn rename_results_are_atomic_and_preserve_workspace_operations() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let changes = vec![crate::bridge::translator::DocumentChanges {
        uri: "file:///one.rs".to_owned(),
        version: Some(7),
        edits: vec![crate::bridge::translator::TextEdit {
            range: crate::bridge::translator::Range {
                start: crate::bridge::translator::Position2D {
                    line: 1,
                    character: 1,
                },
                end: crate::bridge::translator::Position2D {
                    line: 1,
                    character: 2,
                },
            },
            new_text: "λ".repeat(MAX_NOTIFICATION_RESULT_BYTES),
        }],
    }];
    let operations = vec![serde_json::json!({
        "kind": "create",
        "uri": "file:///created.rs"
    })];
    let mut result = RenameResult {
        changes,
        operations,
        total_files: 0,
        total_edits: 0,
        total_operations: 0,
        returned_files: 0,
        returned_edits: 0,
        returned_operations: 0,
        edit_bytes: 0,
        edit_digest: String::new(),
        changes_resource: None,
        deferred: false,
    };
    let complete_changes = serde_json::to_value(&result.changes).unwrap();
    let complete_operations = serde_json::to_value(&result.operations).unwrap();

    bound_rename_result(&mut result, &shared, "project").unwrap();

    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_NOTIFICATION_RESULT_BYTES);
    assert!(result.deferred);
    assert_eq!(result.total_files, 1);
    assert_eq!(result.total_edits, 1);
    assert_eq!(result.total_operations, 1);
    assert!(result.changes.is_empty());
    assert!(result.operations.is_empty());
    let reference = result.changes_resource.as_ref().unwrap();
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    let complete = shared
        .lock()
        .unwrap()
        .read_scoped(token, "project")
        .unwrap();
    assert_eq!(complete["changes"], complete_changes);
    assert_eq!(complete["operations"], complete_operations);
}

#[test]
fn code_action_pages_are_snapshot_bound_and_gap_free() {
    let actions = (0..130)
        .map(|index| serde_json::json!({"index": index}))
        .collect::<Vec<_>>();
    let (first, snapshot, next) = code_action_page_bounds(&actions, None).unwrap();
    assert_eq!(first, 0..64);
    let token = next.unwrap();
    let (second, same_snapshot, next) = code_action_page_bounds(&actions, Some(&token)).unwrap();
    assert_eq!(same_snapshot, snapshot);
    assert_eq!(second, 64..128);
    let (last, _, next) = code_action_page_bounds(&actions, next.as_deref()).unwrap();
    assert_eq!(last, 128..130);
    assert!(next.is_none());
    assert!(code_action_page_bounds(&actions, Some("stale:64")).is_err());
}

#[test]
fn oversized_code_action_details_are_deferred_losslessly() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let action = crate::bridge::translator::CodeAction {
        action_id: Some("action-1".to_owned()),
        title: "Fix it".to_owned(),
        kind: Some("quickfix".to_owned()),
        diagnostics: Vec::new(),
        edit: None,
        workspace_edit: None,
        command: Some(crate::bridge::translator::CommandDescription {
            title: "Fix it".to_owned(),
            command: "fix".to_owned(),
            arguments: vec![serde_json::Value::String(
                "λ".repeat(MAX_NOTIFICATION_RESULT_BYTES),
            )],
        }),
        is_preferred: true,
        disabled: None,
        data: None,
    };
    let complete = serde_json::to_value(vec![action.clone()]).unwrap();
    let mut result = CodeActionsResult {
        actions: vec![action],
        actions_resource: None,
        total_actions: 1,
        returned_actions: 1,
        remaining_actions: 0,
        next_cursor: None,
        snapshot_identity: "snapshot".to_owned(),
    };

    defer_oversized_code_action_payloads(&mut result, &shared, "project").unwrap();

    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_NOTIFICATION_RESULT_BYTES);
    assert!(result.actions[0].command.is_none());
    let reference = result.actions_resource.as_ref().unwrap();
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .read_scoped(token, "project")
            .unwrap(),
        complete
    );
}

#[test]
fn inlay_hint_pages_are_snapshot_bound_and_gap_free() {
    let hints = (0..130)
        .map(|index| crate::bridge::translator::InlayHintEntry {
            hint_id: None,
            resolve_handle: None,
            position: crate::bridge::translator::Position2D {
                line: u32::try_from(index + 1).unwrap(),
                character: 1,
            },
            label: format!("hint-{index}"),
            label_parts: None,
            kind: Some(1),
            padding_left: None,
            padding_right: None,
            tooltip: None,
            text_edit: None,
            data: None,
        })
        .collect::<Vec<_>>();
    let (first, snapshot, next) = inlay_hint_page_bounds(&hints, None).unwrap();
    assert_eq!(first, 0..8);
    let token = next.unwrap();
    let (second, same_snapshot, _) = inlay_hint_page_bounds(&hints, Some(&token)).unwrap();
    assert_eq!(same_snapshot, snapshot);
    assert_eq!(second, 8..16);
    let last_token = format!("{snapshot}:128");
    let (last, _, next) = inlay_hint_page_bounds(&hints, Some(&last_token)).unwrap();
    assert_eq!(last, 128..130);
    assert!(next.is_none());
    assert!(inlay_hint_page_bounds(&hints, Some("stale:8")).is_err());
}

#[test]
fn oversized_inlay_hint_details_are_deferred_losslessly() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let hint = crate::bridge::translator::InlayHintEntry {
        hint_id: Some("hint-1".to_owned()),
        resolve_handle: Some("hint-1".to_owned()),
        position: crate::bridge::translator::Position2D {
            line: 1,
            character: 1,
        },
        label: "type".to_owned(),
        label_parts: Some(vec![serde_json::json!({
            "value": "type",
            "command": {"command": "resolve"}
        })]),
        kind: Some(1),
        padding_left: Some(true),
        padding_right: Some(false),
        tooltip: Some("λ".repeat(MAX_NOTIFICATION_RESULT_BYTES)),
        text_edit: Some(serde_json::json!({"newText": "replacement"})),
        data: Some(serde_json::json!({"opaque": "provider"})),
    };
    let complete = serde_json::to_value(vec![hint.clone()]).unwrap();
    let mut result = InlayHintsResult {
        hints: vec![hint],
        provider_incomplete: false,
        total_hints: 1,
        returned_hints: 1,
        remaining_hints: 0,
        next_cursor: None,
        snapshot_identity: "snapshot".to_owned(),
        hints_resource: None,
        truncated: false,
    };

    defer_oversized_inlay_hint_payloads(&mut result, &shared, "project").unwrap();

    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_NOTIFICATION_RESULT_BYTES);
    assert!(result.truncated);
    assert!(result.hints[0].tooltip.is_none());
    assert!(result.hints[0].text_edit.is_none());
    assert!(result.hints[0].data.is_none());
    let reference = result.hints_resource.as_ref().unwrap();
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .read_scoped(token, "project")
            .unwrap(),
        complete
    );
}

#[test]
fn completion_pages_are_snapshot_bound_and_gap_free() {
    let items = (0..130)
        .map(|index| serde_json::json!({"label": index}))
        .collect::<Vec<_>>();
    let (first, snapshot, next) = completion_page_bounds(&items, None).unwrap();
    assert_eq!(first, 0..64);
    let token = next.unwrap();
    let (second, same_snapshot, next) = completion_page_bounds(&items, Some(&token)).unwrap();
    assert_eq!(same_snapshot, snapshot);
    assert_eq!(second, 64..128);
    let (last, _, next) = completion_page_bounds(&items, next.as_deref()).unwrap();
    assert_eq!(last, 128..130);
    assert!(next.is_none());
    assert!(completion_page_bounds(&items, Some("stale:64")).is_err());
}

#[test]
fn oversized_completion_details_are_deferred_losslessly() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let item = crate::bridge::Completion {
        completion_id: Some("completion-1".to_owned()),
        label: "println!".to_owned(),
        kind: Some("Function".to_owned()),
        detail: Some("λ".repeat(MAX_NOTIFICATION_RESULT_BYTES)),
        documentation: Some("docs".to_owned()),
        sort_text: Some("01".to_owned()),
        filter_text: Some("println".to_owned()),
        insert_text: Some("println!(\"$0\")".to_owned()),
        text_edit: None,
        insertion_handle: Some("completion-1".to_owned()),
    };
    let complete = serde_json::to_value(vec![item.clone()]).unwrap();
    let mut result = CompletionsResult {
        items: vec![item],
        provider_incomplete: true,
        total_items: 1,
        returned_items: 1,
        remaining_items: 0,
        next_cursor: None,
        snapshot_identity: "snapshot".to_owned(),
        items_resource: None,
    };

    defer_oversized_completion_payloads(&mut result, &shared, "project").unwrap();

    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_NOTIFICATION_RESULT_BYTES);
    assert!(result.items[0].detail.is_none());
    let reference = result.items_resource.as_ref().unwrap();
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .read_scoped(token, "project")
            .unwrap(),
        complete
    );
}

#[test]
fn signature_pages_are_snapshot_bound_and_gap_free() {
    let signatures = (0..70)
        .map(|index| crate::bridge::translator::SignatureInfo {
            signature_id: None,
            label: format!("f{index}()"),
            documentation: None,
            parameters: Vec::new(),
        })
        .collect::<Vec<_>>();
    let (first, snapshot, next) = signature_page_bounds(&signatures, None).unwrap();
    assert_eq!(first, 0..32);
    let token = next.unwrap();
    let (second, same_snapshot, next) = signature_page_bounds(&signatures, Some(&token)).unwrap();
    assert_eq!(same_snapshot, snapshot);
    assert_eq!(second, 32..64);
    let (last, _, next) = signature_page_bounds(&signatures, next.as_deref()).unwrap();
    assert_eq!(last, 64..70);
    assert!(next.is_none());
    assert!(signature_page_bounds(&signatures, Some("stale:32")).is_err());
}

#[test]
fn oversized_signature_documentation_is_deferred_losslessly() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let signature = crate::bridge::translator::SignatureInfo {
        signature_id: Some("signature-1".to_owned()),
        label: "f(value)".to_owned(),
        documentation: Some("λ".repeat(MAX_NOTIFICATION_RESULT_BYTES)),
        parameters: vec![crate::bridge::translator::SignatureParameter {
            label: "value".to_owned(),
            documentation: Some("parameter docs".to_owned()),
        }],
    };
    let complete = serde_json::to_value(vec![signature.clone()]).unwrap();
    let mut result = SignatureHelpResult {
        signatures: vec![signature],
        active_signature: Some(0),
        active_parameter: Some(0),
        total_signatures: 1,
        returned_signatures: 1,
        remaining_signatures: 0,
        next_cursor: None,
        snapshot_identity: "snapshot".to_owned(),
        signatures_resource: None,
    };

    defer_oversized_signature_payloads(&mut result, &shared, "project").unwrap();

    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_NOTIFICATION_RESULT_BYTES);
    assert!(result.signatures[0].documentation.is_none());
    assert!(result.signatures[0].parameters[0].documentation.is_none());
    let reference = result.signatures_resource.as_ref().unwrap();
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .read_scoped(token, "project")
            .unwrap(),
        complete
    );
}

#[test]
fn symbol_handle_store_is_bounded_and_rejects_forged_handles() {
    let mut store = SymbolHandleStore {
        entries: HashMap::new(),
        ttl: Duration::from_secs(60),
        max_entries: 1,
    };
    let first = store.insert(StoredSymbolTarget::new(
        PathBuf::from("first.rs"),
        1,
        2,
        SourceSnapshot::Version(1),
    ));
    let second = store.insert(StoredSymbolTarget::new(
        PathBuf::from("second.rs"),
        3,
        4,
        SourceSnapshot::Hash("abc".to_owned()),
    ));

    assert!(store.resolve(&first).is_err());
    assert_eq!(store.resolve(&second).unwrap().line, 3);
    assert!(store.resolve(&SymbolHandle::new()).is_err());
}

#[test]
fn project_actor_replacements_start_without_semantic_handles_or_edit_plans() {
    let mut old_runtime = ProjectRuntime::new(Translator::new());
    let handle = old_runtime
        .symbol_handles
        .lock()
        .unwrap()
        .insert(StoredSymbolTarget::new(
            PathBuf::from("src/lib.rs"),
            1,
            2,
            SourceSnapshot::Version(1),
        ));
    let plan = EditPlan::new(
        "project".to_owned(),
        vec![crate::edit_plan::FileSnapshot::from_contents(
            PathBuf::from("src/lib.rs"),
            crate::edit_plan::SnapshotSource::Disk,
            None,
            "before\n",
            "after\n",
        )],
        vec!["replace".to_owned()],
        true,
        Duration::from_secs(60),
    );
    let plan_id = plan.id().clone();
    old_runtime.edit_plans.insert(plan).unwrap();

    let replacement_runtime = ProjectRuntime::new(Translator::new());
    assert!(
        replacement_runtime
            .symbol_handles
            .lock()
            .unwrap()
            .resolve(&handle)
            .is_err()
    );
    assert!(replacement_runtime.edit_plans.get(&plan_id).is_none());
}

#[test]
fn cancelled_inspect_symbol_request_is_discarded_before_actor_work() {
    let (reply, response) = oneshot::channel();
    drop(response);
    let request = ProjectRequest::InspectSymbol {
        request: InspectSymbolRequest {
            symbol_handle: None,
            query: Some("cancelled".to_owned()),
            kind: None,
            path: None,
            container: None,
            candidate_limit: 10,
            sections: Vec::new(),
            budget: crate::bridge::InspectSymbolBudget::default(),
        },
        reply,
    };

    assert!(request.is_cancelled());
}

#[test]
fn inspect_symbol_batch_resumes_a_dormant_rust_runtime() {
    let (reply, _response) = oneshot::channel();
    let request = ProjectRequest::InspectSymbolBatch {
        request: Box::new(crate::bridge::InspectSymbolBatchRequest {
            targets: vec![crate::bridge::InspectSymbolTarget {
                symbol_handle: None,
                query: Some("target".to_owned()),
                kind: None,
                path: None,
                container: None,
            }],
            candidate_limit: 10,
            sections: Vec::new(),
            budget: crate::bridge::InspectSymbolBudget::default(),
            page_token: None,
        }),
        reply,
    };

    assert!(request.resumes_rust_runtime());
}

#[tokio::test]
async fn workspace_symbol_handle_survives_handle_clones_and_rejects_stale_source() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("lib.rs");
    fs::write(&source, "fn handle_target() {}\n").unwrap();
    let mut translator =
        Translator::new().with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let actor = spawn_project_actor_with_translator(4, translator);

    let result = actor
        .workspace_symbol(WorkspaceSymbolPageRequest {
            query: "handle_target".to_owned(),
            kind_filter: None,
            match_mode: WorkspaceSymbolMatchMode::default(),
            scope: WorkspaceSymbolScope::default(),
            include_generated: false,
            max_items: 10,
            max_bytes: 16 * 1024,
            page_token: None,
        })
        .await
        .unwrap();
    let Some(handle) = result.symbols[0].location.symbol_handle.clone() else {
        panic!("workspace symbol should carry a handle");
    };
    let target = actor
        .clone()
        .resolve_symbol_handle(handle.clone())
        .await
        .unwrap();
    assert_eq!(target.file_path, source.display().to_string());
    assert_eq!(target.character, 4);
    let other_actor = spawn_project_actor_with_translator(4, Translator::new());
    let isolation_error = other_actor
        .resolve_symbol_handle(handle.clone())
        .await
        .unwrap_err();
    assert!(isolation_error.to_string().contains("forged"));

    fs::write(&source, "fn moved_target() {}\n").unwrap();
    let error = actor.resolve_symbol_handle(handle).await.unwrap_err();
    assert!(error.to_string().contains("stale_symbol_handle"));
}

#[tokio::test]
async fn deferred_workspace_symbol_handle_targets_the_identifier() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("lib.rs");
    fs::write(&source, "pub fn add(a: i32, b: i32) -> i32 { a + b }\n").unwrap();
    let mut translator =
        Translator::new().with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let (_, _, content_hash, _) = translator.source_snapshot(&source).await.unwrap();
    let runtime = ProjectRuntime::new(translator);
    let mut symbol = WorkspaceSymbol {
        name: "add".to_owned(),
        kind: "Function".to_owned(),
        location: crate::bridge::Location {
            path: Some(source.display().to_string()),
            uri: crate::bridge::path_to_uri(&source).unwrap().to_string(),
            range: crate::bridge::Range {
                start: crate::bridge::Position2D {
                    line: 1,
                    character: 1,
                },
                end: crate::bridge::Position2D {
                    line: 1,
                    character: 4,
                },
            },
            source: SourceContext::Deferred {
                resource: crate::bridge::translator::DeferredResourceReference {
                    uri: "mcpls-source://test".to_owned(),
                    kind: "source_context".to_owned(),
                    snapshot_hash: content_hash,
                    document_version: None,
                    total_bytes: None,
                },
            },
            symbol_handle: None,
        },
        container_name: None,
        match_class: crate::bridge::translator::WorkspaceSymbolMatch::Exact,
        score: 100,
        project_relative_path: Some("lib.rs".to_owned()),
        origin: crate::bridge::translator::WorkspaceSymbolOrigin::ProjectLocal,
        is_generated: false,
    };

    runtime
        .attach_workspace_symbol_handle(&mut symbol, &mut HashMap::new())
        .await;
    let target = runtime
        .resolve_symbol_target(symbol.location.symbol_handle.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!((target.line, target.character), (1, 8));
}

#[tokio::test]
async fn workspace_symbol_batch_deduplicates_queries_inside_one_actor_request() {
    use crate::bridge::translator::testing::{
        FakeServer, read_framed_message, translator_with_capabilities, write_response,
    };

    let root = TempDir::new().unwrap();
    let alpha = root.path().join("alpha.rs");
    let beta = root.path().join("beta.rs");
    fs::write(&alpha, "fn alpha() {}\n").unwrap();
    fs::write(&beta, "fn beta() {}\n").unwrap();
    let capabilities = lsp_types::ServerCapabilities {
        workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, server) =
        translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
    let (release_responder, keep_responder_alive) = tokio::sync::oneshot::channel();
    let FakeServer {
        _write_half,
        _read_half,
        mut read_half_stdin,
        mut write_stdout,
    } = server;
    let responder_alpha = alpha.clone();
    let responder = tokio::spawn(async move {
        let _processes = (_write_half, _read_half);
        let mut reader = BufReader::new(&mut write_stdout);
        let mut queries = Vec::new();
        while queries.len() < 3 {
            let message = read_framed_message(&mut reader).await;
            let Some(id) = message.get("id") else {
                continue;
            };
            let query = message["params"]["query"].as_str().unwrap().to_owned();
            let uri = if query == "alpha" {
                &responder_alpha
            } else {
                &beta
            };
            write_response(
                &mut read_half_stdin,
                id,
                serde_json::json!([{
                    "name": query,
                    "kind": 12,
                    "location": {
                        "uri": path_to_uri(uri).unwrap(),
                        "range": {
                            "start": {"line": 0, "character": 3},
                            "end": {"line": 0, "character": 8}
                        }
                    }
                }]),
            )
            .await;
            queries.push(query);
        }
        let _ = keep_responder_alive.await;
        queries
    });
    let actor = spawn_project_actor_with_translator(4, translator);

    let result = actor
        .workspace_symbol_batch(WorkspaceSymbolBatchRequest {
            queries: vec!["alpha".to_owned(), "alpha".to_owned(), "beta".to_owned()],
            kind_filter: None,
            match_mode: WorkspaceSymbolMatchMode::Exact,
            scope: WorkspaceSymbolScope::Project,
            include_generated: false,
            max_items: 10,
            max_bytes: 16 * 1024,
            page_token: None,
        })
        .await
        .unwrap();

    assert_eq!((result.unique_queries, result.provider_requests), (2, 2));
    assert_eq!(result.entries.len(), 3);
    assert_eq!(result.entries[1].reused_from, Some(0));
    assert!(result.entries[1].result.is_none());
    assert_eq!(result.returned, 2);
    assert!(!result.truncated);
    assert!(serde_json::to_vec(&result).unwrap().len() <= result.max_bytes);

    let repeated = actor
        .workspace_symbol_batch(WorkspaceSymbolBatchRequest {
            queries: vec!["alpha".to_owned(), "beta".to_owned()],
            kind_filter: None,
            match_mode: WorkspaceSymbolMatchMode::Exact,
            scope: WorkspaceSymbolScope::Project,
            include_generated: false,
            max_items: 10,
            max_bytes: 16 * 1024,
            page_token: None,
        })
        .await
        .unwrap();
    assert_eq!(repeated.provider_requests, 0);
    assert_eq!(repeated.returned, 2);
    assert!(repeated.cache_hit);
    assert_eq!(repeated.snapshot_identity, result.snapshot_identity);

    fs::write(&alpha, "fn alpha_changed() {}\n").unwrap();
    let refreshed = actor
        .workspace_symbol_batch(WorkspaceSymbolBatchRequest {
            queries: vec!["alpha".to_owned()],
            kind_filter: None,
            match_mode: WorkspaceSymbolMatchMode::Exact,
            scope: WorkspaceSymbolScope::Project,
            include_generated: false,
            max_items: 10,
            max_bytes: 16 * 1024,
            page_token: None,
        })
        .await
        .unwrap();
    assert_eq!(refreshed.provider_requests, 1);
    assert!(!refreshed.cache_hit);
    assert_ne!(result.snapshot_identity, refreshed.snapshot_identity);
    release_responder.send(()).unwrap();
    assert_eq!(responder.await.unwrap(), ["alpha", "beta", "alpha"]);
}

#[tokio::test]
async fn workspace_symbol_batch_overlaps_provider_requests() {
    use crate::bridge::translator::testing::{
        FakeServer, read_framed_message, translator_with_capabilities, write_response,
    };

    let root = TempDir::new().unwrap();
    fs::write(root.path().join("symbols.rs"), "fn symbol() {}\n").unwrap();
    let capabilities = lsp_types::ServerCapabilities {
        workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, server) =
        translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
    let FakeServer {
        _write_half,
        _read_half,
        mut read_half_stdin,
        mut write_stdout,
    } = server;
    let (release_responder, keep_responder_alive) = tokio::sync::oneshot::channel();
    let responder = tokio::spawn(async move {
        let _processes = (_write_half, _read_half);
        let mut reader = BufReader::new(&mut write_stdout);
        let mut requests = Vec::new();
        while requests.len() < 2 {
            let message = read_framed_message(&mut reader).await;
            if message.get("method").and_then(serde_json::Value::as_str) == Some("workspace/symbol")
            {
                requests.push(message);
            }
        }
        for request in &requests {
            write_response(&mut read_half_stdin, &request["id"], serde_json::json!([])).await;
        }
        while requests.len() < 4 {
            let message = read_framed_message(&mut reader).await;
            if message.get("method").and_then(serde_json::Value::as_str) == Some("workspace/symbol")
            {
                requests.push(message);
            }
        }
        for request in &requests[2..] {
            write_response(&mut read_half_stdin, &request["id"], serde_json::json!([])).await;
        }
        let _ = keep_responder_alive.await;
        requests.len()
    });
    let actor = spawn_project_actor_with_translator(4, translator);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        actor.workspace_symbol_batch(WorkspaceSymbolBatchRequest {
            queries: ["one", "two", "three", "four"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            kind_filter: None,
            match_mode: WorkspaceSymbolMatchMode::Exact,
            scope: WorkspaceSymbolScope::Project,
            include_generated: false,
            max_items: 10,
            max_bytes: 16 * 1024,
            page_token: None,
        }),
    )
    .await
    .expect("provider requests should overlap")
    .unwrap();

    assert_eq!(result.provider_requests, 4);
    assert_eq!(result.entries.len(), 4);
    release_responder.send(()).unwrap();
    assert_eq!(responder.await.unwrap(), 4);
}

#[tokio::test]
async fn concurrent_workspace_symbol_requests_do_not_serialize_in_the_actor() {
    use crate::bridge::translator::testing::{
        FakeServer, read_framed_message, translator_with_capabilities, write_response,
    };

    let root = TempDir::new().unwrap();
    let source = root.path().join("symbols.rs");
    fs::write(&source, "fn symbol() {}\n").unwrap();
    let capabilities = lsp_types::ServerCapabilities {
        workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, server) =
        translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
    let FakeServer {
        _write_half,
        _read_half,
        mut read_half_stdin,
        mut write_stdout,
    } = server;
    let (release_responder, keep_responder_alive) = tokio::sync::oneshot::channel();
    let responder = tokio::spawn(async move {
        let _processes = (_write_half, _read_half);
        let mut reader = BufReader::new(&mut write_stdout);
        let mut requests = Vec::new();
        while requests.len() < 2 {
            let message = read_framed_message(&mut reader).await;
            if message.get("method").and_then(serde_json::Value::as_str) == Some("workspace/symbol")
            {
                requests.push(message);
            }
        }
        for request in requests {
            write_response(
                &mut read_half_stdin,
                &request["id"],
                serde_json::json!([{
                    "name": request["params"]["query"],
                    "kind": 12,
                    "location": {
                        "uri": path_to_uri(&source).unwrap(),
                        "range": {
                            "start": {"line": 0, "character": 3},
                            "end": {"line": 0, "character": 9}
                        }
                    }
                }]),
            )
            .await;
        }
        let _ = keep_responder_alive.await;
    });
    let actor = spawn_project_actor_with_translator(4, translator);
    let request = |query: &str| {
        actor.workspace_symbol(WorkspaceSymbolPageRequest {
            query: query.to_owned(),
            kind_filter: None,
            match_mode: WorkspaceSymbolMatchMode::Exact,
            scope: WorkspaceSymbolScope::Project,
            include_generated: false,
            max_items: 10,
            max_bytes: 16 * 1024,
            page_token: None,
        })
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        futures::future::try_join(request("one"), request("two")),
    )
    .await
    .expect("independent workspace-symbol requests must overlap")
    .unwrap();

    assert_eq!(result.0.returned, 1);
    assert_eq!(result.1.returned, 1);
    release_responder.send(()).unwrap();
    responder.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workspace_symbol_search_is_bounded_and_pageable() {
    use crate::bridge::translator::testing::{
        FakeServer, read_framed_message, translator_with_capabilities, write_response,
    };

    let root = TempDir::new().unwrap();
    let source = root.path().join("symbols.rs");
    fs::write(&source, "fn get_symbol() {}\n".repeat(100)).unwrap();
    let capabilities = lsp_types::ServerCapabilities {
        workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, server) =
        translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
    let FakeServer {
        _write_half: write_half,
        _read_half: read_half,
        mut read_half_stdin,
        mut write_stdout,
    } = server;
    let responder_source = source.clone();
    let responder = tokio::spawn(async move {
        let _processes = (write_half, read_half);
        let mut reader = BufReader::new(&mut write_stdout);
        let message = read_framed_message(&mut reader).await;
        let id = message.get("id").unwrap();
        let symbols = (0..100)
            .map(|index| {
                serde_json::json!({
                    "name": format!("get_symbol_{index:03}"),
                    "kind": 12,
                    "location": {
                        "uri": path_to_uri(&responder_source).unwrap(),
                        "range": {
                            "start": {"line": index, "character": 3},
                            "end": {"line": index, "character": 17}
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        write_response(&mut read_half_stdin, id, serde_json::json!(symbols)).await;
    });
    let actor = spawn_project_actor_with_translator(4, translator);

    let mut page_token = None;
    let mut snapshot_identity = None;
    let mut source_resource = None;
    let mut names = Vec::new();
    let mut encoded_bytes = 0;
    loop {
        let result = actor
            .workspace_symbol(WorkspaceSymbolPageRequest {
                query: "get".to_owned(),
                kind_filter: None,
                match_mode: WorkspaceSymbolMatchMode::Fuzzy,
                scope: WorkspaceSymbolScope::Project,
                include_generated: false,
                max_items: 100,
                max_bytes: 16 * 1024,
                page_token,
            })
            .await
            .unwrap();
        let encoded = serde_json::to_vec(&result).unwrap();
        encoded_bytes += encoded.len();
        assert!(
            encoded.len() <= 16 * 1024,
            "single workspace-symbol page used {} bytes",
            encoded.len()
        );
        assert_eq!(result.total, 100);
        assert!(
            result
                .symbols
                .iter()
                .all(|symbol| matches!(&symbol.location.source, SourceContext::Deferred { .. })),
            "workspace-symbol pages must defer source context"
        );
        if source_resource.is_none() {
            let SourceContext::Deferred { resource } = &result.symbols[0].location.source else {
                unreachable!("source contexts were checked above")
            };
            source_resource = Some(resource.uri.clone());
        }
        if let Some(identity) = &snapshot_identity {
            assert_eq!(result.snapshot_identity.as_ref(), Some(identity));
        } else {
            snapshot_identity = result.snapshot_identity.clone();
        }
        names.extend(result.symbols.into_iter().map(|symbol| symbol.name));
        assert_eq!(result.remaining, 100 - names.len());
        let Some(cursor) = result.next_cursor else {
            assert!(!result.truncated);
            break;
        };
        assert!(result.truncated);
        page_token = Some(cursor);
    }

    assert_eq!(
        names,
        (0..100)
            .map(|index| format!("get_symbol_{index:03}"))
            .collect::<Vec<_>>()
    );
    assert!(
        encoded_bytes < 96 * 1024,
        "workspace-symbol pages used {encoded_bytes} bytes total"
    );
    let source = actor
        .read_source_resource(
            crate::bridge::resources::parse_source_uri(&source_resource.unwrap()).unwrap(),
            16 * 1024,
        )
        .await
        .unwrap();
    assert!(source.text.contains("fn get_symbol()"));
    responder.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn document_symbols_default_page_is_bounded() {
    use std::fmt::Write as _;

    use crate::bridge::translator::testing::{
        FakeServer, read_framed_message, translator_with_capabilities, write_response,
    };

    let root = TempDir::new().unwrap();
    let source = root.path().join("outline.rs");
    let content = (0..100).fold(String::new(), |mut content, index| {
        writeln!(content, "pub fn caf\u{e9}_{index:03}() {{}}").unwrap();
        content
    });
    fs::write(&source, content).unwrap();
    let capabilities = lsp_types::ServerCapabilities {
        document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, server) =
        translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
    let FakeServer {
        _write_half: write_half,
        _read_half: read_half,
        mut read_half_stdin,
        mut write_stdout,
    } = server;
    let responder_source = source.clone();
    let responder = tokio::spawn(async move {
        let _processes = (write_half, read_half);
        let mut reader = BufReader::new(&mut write_stdout);
        let message = loop {
            let message = read_framed_message(&mut reader).await;
            if !message["id"].is_null() {
                break message;
            }
        };
        let id = message.get("id").unwrap();
        let symbols = (0..100)
            .map(|index| {
                serde_json::json!({
                    "name": format!("caf\u{e9}_{index:03}"),
                    "kind": 12,
                    "location": {
                        "uri": path_to_uri(&responder_source).unwrap(),
                        "range": {
                            "start": {"line": index, "character": 7},
                            "end": {"line": index, "character": 15}
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        write_response(&mut read_half_stdin, id, serde_json::json!(symbols)).await;
    });
    let actor = spawn_project_actor_with_translator(4, translator);

    let mut page_token = None;
    let mut snapshot_identity = None;
    let mut source_resource = None;
    let mut names = Vec::new();
    let mut encoded_bytes = 0;
    loop {
        let result = actor
            .document_symbol_page(DocumentSymbolPageRequest {
                file_path: source.display().to_string(),
                options: DocumentSymbolOptions::default(),
                max_bytes: 16 * 1024,
                page_token,
            })
            .await
            .unwrap();
        let encoded = serde_json::to_vec(&result).unwrap();
        encoded_bytes += encoded.len();
        assert!(
            encoded.len() <= 16 * 1024,
            "document-symbol page used {} bytes",
            encoded.len()
        );
        assert_eq!(result.total, 100);
        assert!(
            result
                .symbols
                .iter()
                .all(|symbol| symbol.children.is_none() && symbol.source.is_none())
        );
        if source_resource.is_none() {
            source_resource = result
                .source_resource
                .as_ref()
                .map(|resource| resource.uri.clone());
        }
        if let Some(identity) = &snapshot_identity {
            assert_eq!(result.snapshot_identity.as_ref(), Some(identity));
        } else {
            snapshot_identity = result.snapshot_identity.clone();
        }
        names.extend(result.symbols.into_iter().map(|symbol| symbol.name));
        assert_eq!(result.remaining, 100 - names.len());
        let Some(cursor) = result.next_cursor else {
            assert!(!result.truncated);
            break;
        };
        assert!(result.truncated);
        page_token = Some(cursor);
    }

    assert_eq!(
        names,
        (0..100)
            .map(|index| format!("caf\u{e9}_{index:03}"))
            .collect::<Vec<_>>()
    );
    assert!(
        encoded_bytes < 32 * 1024,
        "document-symbol pages used {encoded_bytes} bytes"
    );
    let source_page = actor
        .read_source_resource(
            crate::bridge::resources::parse_source_uri(&source_resource.unwrap()).unwrap(),
            16 * 1024,
        )
        .await
        .unwrap();
    assert!(source_page.text.contains("pub fn caf\u{e9}_000"));
    assert!(source_page.text.contains("pub fn caf\u{e9}_099"));
    responder.await.unwrap();
}

#[test]
fn oversized_diagnostic_payloads_are_scoped_and_readable() {
    let range = crate::bridge::Range {
        start: crate::bridge::Position2D {
            line: 1,
            character: 1,
        },
        end: crate::bridge::Position2D {
            line: 1,
            character: 2,
        },
    };
    let location = crate::bridge::Location {
        path: Some("/workspace/src/lib.rs".to_owned()),
        uri: "file:///workspace/src/lib.rs".to_owned(),
        range: range.clone(),
        source: SourceContext::Unavailable {
            reason: SourceUnavailableReason::NotFound,
        },
        symbol_handle: None,
    };
    let mut diagnostic = crate::bridge::Diagnostic {
        range,
        severity: DiagnosticSeverity::Error,
        message: "error ".repeat(500),
        code: Some("E0001".to_owned()),
        context: crate::bridge::translator::DiagnosticContext::default(),
    };
    diagnostic.context.related_information =
        vec![crate::bridge::translator::DiagnosticRelatedInformation {
            location,
            message: "related context ".repeat(200),
        }];
    diagnostic.context.data = Some(serde_json::Value::String("data ".repeat(500)));
    diagnostic.context.fix_handles = (0..200).map(|index| format!("fix-{index}")).collect();
    let mut result = DiagnosticsResult::raw(vec![diagnostic]);
    let store = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    defer_oversized_diagnostic_payloads(&mut result, 100, &store, "project").unwrap();
    let reference = result.diagnostics[0]
        .context
        .related_information_resource
        .as_ref()
        .unwrap();
    assert!(result.diagnostics[0].context.related_information.is_empty());
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    assert!(store.lock().unwrap().read_scoped(token, "other").is_err());
    let value = store.lock().unwrap().read_scoped(token, "project").unwrap();
    assert_eq!(value.as_array().unwrap().len(), 1);
    assert!(result.diagnostics[0].context.message_resource.is_some());
    assert!(result.diagnostics[0].context.data_resource.is_some());
    assert!(result.diagnostics[0].context.fix_handles_resource.is_some());
}

#[test]
fn diagnostic_pages_bound_the_transcript_shape_without_losing_occurrences() {
    let diagnostics = (0..249)
        .map(|line| crate::bridge::Diagnostic {
            range: crate::bridge::Range {
                start: crate::bridge::Position2D { line, character: 1 },
                end: crate::bridge::Position2D { line, character: 2 },
            },
            severity: DiagnosticSeverity::Hint,
            message: "uniffi::constructor: internal café 🚗 proc-macro error".to_owned(),
            code: Some("macro-error".to_owned()),
            context: crate::bridge::translator::DiagnosticContext {
                path: Some("/workspace/src/lib.rs".to_owned()),
                project_relative_path: Some("src/lib.rs".to_owned()),
                uri: "file:///workspace/src/lib.rs".to_owned(),
                source_frame: SourceContext::Deferred {
                    resource: DeferredResourceReference {
                        uri: format!(
                            "mcpls-source:///workspace/src/lib.rs?start_line={line}&snapshot={}",
                            "a".repeat(64)
                        ),
                        kind: "source_context".to_owned(),
                        snapshot_hash: "a".repeat(64),
                        document_version: Some(1),
                        total_bytes: Some(512),
                    },
                },
                diagnostic_source: Some("rust-analyzer".to_owned()),
                ..crate::bridge::translator::DiagnosticContext::default()
            },
        })
        .collect();
    let options = DiagnosticOptions {
        preserve_locations: true,
        item_limit: 20,
        byte_limit: 6_000,
        ..DiagnosticOptions::default()
    };
    let mut result = Translator::finish_diagnostics(diagnostics, options);
    result.snapshot_identity = Some("diagnostics-snapshot".to_owned());
    let mut state = DiagnosticsPageState {
        file_path: "/workspace/src/lib.rs".to_owned(),
        fresh: false,
        result,
    };

    let mut lines = Vec::new();
    let mut group_id = None;
    let mut encoded_bytes = 0;
    loop {
        let (page, continuation) = bounded_diagnostics_page(state, 20, 6_000).unwrap();
        let encoded = serde_json::to_vec(&page).unwrap();
        encoded_bytes += encoded.len();
        assert!(encoded.len() <= 6_000, "page used {} bytes", encoded.len());
        assert_eq!(page.total_diagnostics, 249);
        assert_eq!(page.total_groups, 1);
        assert_eq!(
            page.snapshot_identity.as_deref(),
            Some("diagnostics-snapshot")
        );
        assert_eq!(page.diagnostics.len(), 1);
        let group = &page.diagnostics[0];
        if let Some(group_id) = &group_id {
            assert_eq!(group.context.group_id.as_ref(), Some(group_id));
        } else {
            group_id = group.context.group_id.clone();
        }
        assert_eq!(group.context.occurrence_offset, lines.len());
        lines.extend(
            group
                .context
                .occurrences
                .iter()
                .map(|occurrence| occurrence.range.start.line),
        );
        assert_eq!(page.returned_diagnostics, group.context.occurrences.len());
        assert_eq!(page.remaining_diagnostics, 249 - lines.len());

        let Some(continuation) = continuation else {
            assert!(page.next_cursor.is_none());
            break;
        };
        assert!(page.next_cursor.is_some());
        state = continuation;
    }

    assert_eq!(lines, (0..249).collect::<Vec<_>>());
    assert!(
        encoded_bytes < 64 * 1024,
        "pages used {encoded_bytes} bytes"
    );
}

#[test]
fn measured_swift_outline_fits_one_default_page() {
    let range = crate::bridge::Range {
        start: crate::bridge::Position2D {
            line: 1,
            character: 1,
        },
        end: crate::bridge::Position2D {
            line: 1,
            character: 2,
        },
    };
    let parent_handle = SymbolHandle::new();
    let symbols = (0..33)
        .map(|index| crate::bridge::Symbol {
            name: format!("RideMapLiveContentView_caf\u{e9}_{index:02}"),
            kind: if index == 0 { "Struct" } else { "Property" }.to_owned(),
            range: range.clone(),
            selection_range: range.clone(),
            symbol_handle: Some(if index == 0 {
                parent_handle.clone()
            } else {
                SymbolHandle::new()
            }),
            parent_symbol_handle: (index != 0).then(|| parent_handle.clone()),
            container_name: (index != 0).then(|| "RideMapLiveContentView".to_owned()),
            match_class: None,
            score: None,
            source: None,
            is_private: false,
            is_test: false,
            children: None,
        })
        .collect();
    let state = DocumentSymbolPageState {
            total: 33,
            snapshot_identity: "e0f3f3e91aa47c772291d7106b5000a1d487f071bf0eb8d6f8d495f0246f8c06"
                .to_owned(),
            document_version: Some(2),
            project_relative_path: Some(
                "swift/CutoutMobile/Apps/CutoutApp/RideMapLiveContentView.swift".to_owned(),
            ),
            source_resource: DeferredResourceReference {
                uri: "mcpls-source:///Users/mjc/projects/libcutout/swift/CutoutMobile/Apps/CutoutApp/RideMapLiveContentView.swift?start_line=1&start_character=1&end_line=270&end_character=1&snapshot=e0f3f3e91aa47c772291d7106b5000a1d487f071bf0eb8d6f8d495f0246f8c06&version=2".to_owned(),
                kind: "source_context".to_owned(),
                snapshot_hash:
                    "e0f3f3e91aa47c772291d7106b5000a1d487f071bf0eb8d6f8d495f0246f8c06"
                        .to_owned(),
                document_version: Some(2),
                total_bytes: Some(10_346),
            },
            filters: DocumentSymbolOptions {
                include_private: true,
                limit: 50,
                max_depth: Some(2),
                ..DocumentSymbolOptions::default()
            },
            symbols,
        };

    let (result, continuation) = bounded_document_symbol_page(state, 50, 16 * 1024).unwrap();

    assert!(continuation.is_none());
    assert_eq!(result.returned, 33);
    assert_eq!(result.remaining, 0);
    assert!(!result.truncated);
    assert!(serde_json::to_vec(&result).unwrap().len() <= 16 * 1024);
    assert_eq!(result.symbols[0].symbol_handle, Some(parent_handle.clone()));
    assert!(
        result.symbols[1..]
            .iter()
            .all(|symbol| symbol.parent_symbol_handle.as_ref() == Some(&parent_handle))
    );
    assert!(
        result
            .symbols
            .iter()
            .any(|symbol| symbol.name.contains('é'))
    );
}

#[test]
fn flattened_document_symbols_keep_exact_parent_identity() {
    let range = crate::bridge::Range {
        start: crate::bridge::Position2D {
            line: 1,
            character: 1,
        },
        end: crate::bridge::Position2D {
            line: 1,
            character: 2,
        },
    };
    let symbol = |name: &str, children| crate::bridge::Symbol {
        name: name.to_owned(),
        kind: "Function".to_owned(),
        range: range.clone(),
        selection_range: range.clone(),
        symbol_handle: None,
        parent_symbol_handle: None,
        container_name: None,
        match_class: None,
        score: None,
        source: None,
        is_private: false,
        is_test: false,
        children,
    };
    let mut symbols = vec![symbol("parent", Some(vec![symbol("child", None)]))];
    attach_document_symbol_handles(
        &mut SymbolHandleStore::new(),
        &mut symbols,
        Path::new("/tmp/outline.rs"),
        &SourceSnapshot::Hash("snapshot".to_owned()),
        None,
    );

    let flat = flatten_document_symbols(symbols);

    assert_eq!(flat.len(), 2);
    assert_eq!(flat[1].parent_symbol_handle, flat[0].symbol_handle);
    assert!(flat.iter().all(|symbol| symbol.children.is_none()));
}

#[tokio::test]
async fn workspace_symbol_batches_reuse_143_query_provider_results_across_calls() {
    use crate::bridge::translator::testing::{
        FakeServer, read_framed_message, translator_with_capabilities, write_response,
    };

    let root = TempDir::new().unwrap();
    let source = root.path().join("symbols.rs");
    fs::write(&source, "fn symbol() {}\n").unwrap();
    let capabilities = lsp_types::ServerCapabilities {
        workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, server) =
        translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
    let FakeServer {
        _write_half,
        _read_half,
        mut read_half_stdin,
        mut write_stdout,
    } = server;
    let (release_responder, keep_responder_alive) = tokio::sync::oneshot::channel();
    let responder = tokio::spawn(async move {
        let _processes = (_write_half, _read_half);
        let mut reader = BufReader::new(&mut write_stdout);
        let mut queries = Vec::new();
        while queries.len() < 117 {
            let message = read_framed_message(&mut reader).await;
            let Some(id) = message.get("id") else {
                continue;
            };
            let Some(query) = message["params"]["query"].as_str() else {
                panic!("unexpected fake-LSP request: {message}");
            };
            let query = query.to_owned();
            write_response(
                &mut read_half_stdin,
                id,
                serde_json::json!([{
                    "name": query,
                    "kind": 12,
                    "location": {
                        "uri": path_to_uri(&source).unwrap(),
                        "range": {
                            "start": {"line": 0, "character": 3},
                            "end": {"line": 0, "character": 9}
                        }
                    }
                }]),
            )
            .await;
            queries.push(query);
        }
        let _ = keep_responder_alive.await;
        queries
    });
    let actor = spawn_project_actor_with_translator(8, translator);
    let unique = (0..117)
        .map(|index| format!("symbol_{index}"))
        .collect::<Vec<_>>();
    let mut queries = unique.clone();
    queries.extend(unique.iter().take(26).cloned());

    let mut client_calls = 0;
    let mut provider_requests = 0;
    for chunk in queries.chunks(32) {
        let result = actor
            .workspace_symbol_batch(WorkspaceSymbolBatchRequest {
                queries: chunk.to_vec(),
                kind_filter: None,
                match_mode: WorkspaceSymbolMatchMode::Exact,
                scope: WorkspaceSymbolScope::Project,
                include_generated: false,
                max_items: 1_000,
                max_bytes: 64 * 1024,
                page_token: None,
            })
            .await
            .unwrap();
        client_calls += 1;
        provider_requests += result.provider_requests;
        assert_eq!(result.entries.len(), chunk.len());
    }

    assert_eq!(client_calls, 5);
    assert_eq!(provider_requests, 117);
    release_responder.send(()).unwrap();
    assert_eq!(responder.await.unwrap().len(), 117);
}

#[tokio::test]
async fn workspace_symbol_batches_cache_truncated_results_for_the_same_limit() {
    use crate::bridge::translator::testing::{
        FakeServer, read_framed_message, translator_with_capabilities, write_response,
    };

    let root = TempDir::new().unwrap();
    let source = root.path().join("symbols.rs");
    fs::write(&source, "fn alpha() {}\nfn alpha_two() {}\n").unwrap();
    let capabilities = lsp_types::ServerCapabilities {
        workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, server) =
        translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
    let (release_responder, keep_responder_alive) = tokio::sync::oneshot::channel();
    let FakeServer {
        _write_half,
        _read_half,
        mut read_half_stdin,
        mut write_stdout,
    } = server;
    let responder = tokio::spawn(async move {
        let _processes = (_write_half, _read_half);
        let mut reader = BufReader::new(&mut write_stdout);
        let mut queries = Vec::new();
        while queries.is_empty() {
            let message = read_framed_message(&mut reader).await;
            let Some(id) = message.get("id") else {
                continue;
            };
            write_response(
                &mut read_half_stdin,
                id,
                serde_json::json!([
                    {
                        "name": "alpha",
                        "kind": 12,
                        "location": {
                            "uri": path_to_uri(&source).unwrap(),
                            "range": {
                                "start": {"line": 0, "character": 3},
                                "end": {"line": 0, "character": 8}
                            }
                        }
                    },
                    {
                        "name": "alpha",
                        "kind": 12,
                        "location": {
                            "uri": path_to_uri(&source).unwrap(),
                            "range": {
                                "start": {"line": 1, "character": 3},
                                "end": {"line": 1, "character": 11}
                            }
                        }
                    }
                ]),
            )
            .await;
            queries.push(message["params"]["query"].as_str().unwrap().to_owned());
        }
        let _ = keep_responder_alive.await;
        queries
    });
    let actor = spawn_project_actor_with_translator(8, translator);
    let request = |max_items| WorkspaceSymbolBatchRequest {
        queries: vec!["alpha".to_owned()],
        kind_filter: None,
        match_mode: WorkspaceSymbolMatchMode::Exact,
        scope: WorkspaceSymbolScope::Project,
        include_generated: false,
        max_items,
        max_bytes: 16 * 1024,
        page_token: None,
    };

    let first = actor.workspace_symbol_batch(request(1)).await.unwrap();
    assert_eq!(first.provider_requests, 1);
    assert!(first.truncated);
    let second = actor
        .workspace_symbol_batch(WorkspaceSymbolBatchRequest {
            queries: Vec::new(),
            page_token: first.next_cursor.clone(),
            ..request(1)
        })
        .await
        .unwrap();
    assert_eq!(second.provider_requests, first.provider_requests);
    assert_eq!(second.returned, 1);
    assert!(second.next_cursor.is_none());
    assert_eq!(second.entries[0].result.as_ref().unwrap().symbols.len(), 1);

    let repeated = actor.workspace_symbol_batch(request(1)).await.unwrap();
    assert_eq!(repeated.provider_requests, 0);
    assert!(repeated.cache_hit);
    assert!(repeated.truncated);

    let larger = actor.workspace_symbol_batch(request(2)).await.unwrap();
    assert_eq!(larger.provider_requests, 0);
    assert_eq!(larger.returned, 2);
    assert!(!larger.truncated);
    release_responder.send(()).unwrap();
    assert_eq!(responder.await.unwrap().len(), 1);
}

#[tokio::test]
async fn inspect_symbol_batch_fetches_targets_concurrently_under_one_global_budget() {
    use crate::bridge::translator::testing::{
        FakeServer, read_framed_message, translator_with_capabilities, write_response,
    };

    let root = TempDir::new().unwrap();
    fs::write(root.path().join("alpha.rs"), "fn alpha() {}\n").unwrap();
    fs::write(root.path().join("beta.rs"), "fn beta() {}\n").unwrap();
    let capabilities = lsp_types::ServerCapabilities {
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, server) =
        translator_with_capabilities(&root, &ServerId::from("rust"), capabilities);
    let FakeServer {
        _write_half,
        _read_half,
        mut read_half_stdin,
        mut write_stdout,
    } = server;
    let (release_server, hold_server) = oneshot::channel();
    let responder = tokio::spawn(async move {
        let _processes = (_write_half, _read_half);
        let mut reader = BufReader::new(&mut write_stdout);
        let mut request_ids = Vec::new();
        while request_ids.len() < 2 {
            let message = read_framed_message(&mut reader).await;
            if let Some(id) = message.get("id").cloned() {
                assert_eq!(message["method"], "textDocument/hover");
                request_ids.push(id);
            }
        }
        for id in &request_ids {
            write_response(
                &mut read_half_stdin,
                id,
                serde_json::json!({
                    "contents": {"kind": "plaintext", "value": "inspected"}
                }),
            )
            .await;
        }
        let _ = hold_server.await;
        request_ids.len()
    });
    let actor = spawn_project_actor_with_translator(4, translator);
    let mut targets = Vec::new();
    for query in ["alpha", "beta"] {
        let symbol = actor
            .workspace_symbol(WorkspaceSymbolPageRequest {
                query: query.to_owned(),
                kind_filter: None,
                match_mode: WorkspaceSymbolMatchMode::Exact,
                scope: WorkspaceSymbolScope::Project,
                include_generated: false,
                max_items: 1,
                max_bytes: 16 * 1024,
                page_token: None,
            })
            .await
            .unwrap()
            .symbols
            .remove(0);
        targets.push(crate::bridge::InspectSymbolTarget {
            symbol_handle: symbol.location.symbol_handle,
            query: None,
            kind: None,
            path: None,
            container: None,
        });
    }
    targets.push(crate::bridge::InspectSymbolTarget {
        symbol_handle: Some(SymbolHandle::new()),
        query: None,
        kind: None,
        path: None,
        container: None,
    });
    targets.push(targets[0].clone());

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        actor.inspect_symbol_batch(crate::bridge::InspectSymbolBatchRequest {
            targets,
            candidate_limit: 10,
            sections: vec![crate::bridge::InspectSymbolSectionKind::Declaration],
            budget: crate::bridge::InspectSymbolBudget {
                max_bytes: 24 * 1024,
                max_items: 4,
            },
            page_token: None,
        }),
    )
    .await
    .expect("batch serialized target inspections")
    .unwrap();

    assert_eq!(result.entries.len(), 4);
    assert_eq!(result.inspections_started, 3);
    assert_eq!(result.total_targets, 4);
    assert_eq!(result.returned_targets, 4);
    assert_eq!(result.remaining_targets, 0);
    assert!(result.next_cursor.is_none());
    assert_eq!(result.budget.max_bytes, 16 * 1024);
    assert_eq!(result.returned_items, 3, "{result:#?}");
    assert!(result.entries[..2].iter().all(|entry| matches!(
        entry.result.as_ref().unwrap().resolution,
        crate::bridge::InspectSymbolResolution::Selected { .. }
    )));
    assert!(
        result.entries[..2]
            .iter()
            .all(|entry| entry.result.as_ref().unwrap().budget.max_items == 1)
    );
    assert!(result.entries[2].error.is_none());
    assert!(matches!(
        &result.entries[2].result.as_ref().unwrap().resolution,
        crate::bridge::InspectSymbolResolution::Stale { retryable: true, reason, .. }
            if reason.starts_with("invalid_symbol_handle:")
    ));
    assert_eq!(
        result.entries[0].result.as_ref().unwrap().returned_bytes,
        result.entries[3].result.as_ref().unwrap().returned_bytes
    );
    assert!(result.returned_bytes <= result.budget.max_bytes);
    release_server.send(()).unwrap();
    assert_eq!(responder.await.unwrap(), 2);
}

#[test]
fn inspect_symbol_batch_pages_are_bounded_replayable_and_lossless() {
    let entries = (0..4)
        .map(|index| InspectSymbolBatchEntry {
            target: crate::bridge::InspectSymbolTarget {
                symbol_handle: None,
                query: Some(format!("target-{index}")),
                kind: None,
                path: None,
                container: None,
            },
            result: None,
            error: Some("x".repeat(5_000)),
            resource: None,
        })
        .collect::<Vec<_>>();
    let snapshot = InspectSymbolBatchSnapshot {
        inspections_started: entries.len(),
        entries,
        snapshot_identity: "snapshot".to_owned(),
        truncated: false,
        max_items: 40,
    };
    let mut store = InspectSymbolBatchPageStore::new();
    let token = store.insert(snapshot.clone(), "session");
    assert!(store.read(&token, "different-session").is_err());

    let first = bounded_inspect_symbol_batch_page(&snapshot, &token, 0).unwrap();
    let first_json = serde_json::to_value(&first).unwrap();
    assert!(first.next_cursor.is_some());
    let mut cursor = Some(inspect_symbol_batch_cursor(&token, 0));
    let mut queries = Vec::new();
    let mut pages = 0;
    while let Some(page_cursor) = cursor {
        let (page_token, offset) = parse_inspect_symbol_batch_cursor(&page_cursor).unwrap();
        let retained = store.read(page_token, "session").unwrap();
        let page = bounded_inspect_symbol_batch_page(&retained, page_token, offset).unwrap();
        let encoded_len = serde_json::to_vec(&page).unwrap().len();
        assert!(encoded_len <= 16 * 1024);
        assert_eq!(page.returned_bytes, encoded_len);
        queries.extend(
            page.entries
                .iter()
                .map(|entry| entry.target.query.clone().unwrap()),
        );
        cursor = page.next_cursor;
        pages += 1;
    }

    assert!(pages > 1);
    assert_eq!(queries, ["target-0", "target-1", "target-2", "target-3"]);
    assert_eq!(
        serde_json::to_value(bounded_inspect_symbol_batch_page(&snapshot, &token, 0).unwrap())
            .unwrap(),
        first_json,
        "replaying a page must return the same cursor and content"
    );
}

#[tokio::test]
async fn inspect_symbol_runs_as_one_actor_request_and_honors_section_selection() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("lib.rs");
    fs::write(&source, "fn inspected() {}\n").unwrap();
    let mut translator =
        Translator::new().with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let actor = spawn_project_actor_with_translator(4, translator);
    let mut symbols = actor
        .workspace_symbol(WorkspaceSymbolPageRequest {
            query: "inspected".to_owned(),
            kind_filter: None,
            match_mode: WorkspaceSymbolMatchMode::Exact,
            scope: WorkspaceSymbolScope::Project,
            include_generated: false,
            max_items: 10,
            max_bytes: 16 * 1024,
            page_token: None,
        })
        .await
        .unwrap()
        .symbols;
    let symbol = symbols.remove(0);
    let result = actor
        .inspect_symbol(InspectSymbolRequest {
            symbol_handle: symbol.location.symbol_handle,
            query: None,
            kind: None,
            path: None,
            container: None,
            candidate_limit: 10,
            sections: vec![crate::bridge::InspectSymbolSectionKind::References],
            budget: crate::bridge::InspectSymbolBudget {
                max_bytes: 4_096,
                max_items: 3,
            },
        })
        .await
        .unwrap();

    assert!(matches!(
        result.resolution,
        crate::bridge::InspectSymbolResolution::Selected { .. }
    ));
    assert_eq!(
        result.sections.declaration.completeness,
        crate::bridge::InspectSectionCompleteness::NotRequested
    );
    assert_eq!(result.sections.references.returned, 0);
    assert!(result.returned_bytes <= result.budget.max_bytes);
}

#[tokio::test]
async fn inspect_symbol_fetches_independent_sections_concurrently() {
    use crate::bridge::translator::testing::{
        FakeServer, read_framed_message, translator_with_capabilities, write_response,
    };

    let root = TempDir::new().unwrap();
    let source = root.path().join("lib.rs");
    fs::write(&source, "fn inspected() {}\n").unwrap();
    let uri = crate::bridge::path_to_uri(&source).unwrap();
    let server_id = ServerId::from("rust");
    let capabilities = lsp_types::ServerCapabilities {
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        implementation_provider: Some(lsp_types::ImplementationProviderCapability::Simple(true)),
        references_provider: Some(lsp_types::OneOf::Left(true)),
        definition_provider: Some(lsp_types::OneOf::Left(true)),
        call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, server) = translator_with_capabilities(&root, &server_id, capabilities);
    let FakeServer {
        _write_half,
        _read_half,
        read_half_stdin,
        mut write_stdout,
    } = server;
    let (release_server, hold_server) = oneshot::channel();
    let responder = tokio::spawn(async move {
        let _processes = (_write_half, _read_half);
        let writer = Arc::new(TokioMutex::new(read_half_stdin));
        let mut reader = BufReader::new(&mut write_stdout);
        let hover = loop {
            let message = read_framed_message(&mut reader).await;
            if message.get("id").is_some() {
                break message;
            }
        };
        assert_eq!(hover["method"], "textDocument/hover");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), read_framed_message(&mut reader))
                .await
                .is_err(),
            "sections must wait for the declaration preflight"
        );
        {
            let mut writer = writer.lock().await;
            write_response(
                &mut *writer,
                &hover["id"],
                serde_json::json!({"contents": {"kind": "plaintext", "value": "inspected"}}),
            )
            .await;
        }

        let mut requests = 1;
        let mut responses = Vec::new();
        while requests < 7 {
            let message = read_framed_message(&mut reader).await;
            let Some(id) = message.get("id").cloned() else {
                continue;
            };
            requests += 1;
            let method = message["method"].as_str().unwrap().to_owned();
            let uri = uri.clone();
            let writer = Arc::clone(&writer);
            responses.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let result = match method.as_str() {
                    "textDocument/hover" => serde_json::json!({
                        "contents": {"kind": "plaintext", "value": "inspected"}
                    }),
                    "textDocument/prepareCallHierarchy" => serde_json::json!([{
                        "name": "inspected",
                        "kind": 12,
                        "uri": uri,
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 17}
                        },
                        "selectionRange": {
                            "start": {"line": 0, "character": 3},
                            "end": {"line": 0, "character": 12}
                        }
                    }]),
                    "textDocument/definition" => serde_json::Value::Null,
                    _ => serde_json::json!([]),
                };
                let mut writer = writer.lock().await;
                write_response(&mut *writer, &id, result).await;
            }));
        }
        for response in responses {
            response.await.unwrap();
        }
        let _ = hold_server.await;
    });

    let runtime = ProjectRuntime::new(translator);
    let (_, _, source_hash, _) = runtime.translator.source_snapshot(&source).await.unwrap();
    let handle = runtime
        .symbol_handles
        .lock()
        .unwrap()
        .insert(StoredSymbolTarget::new(
            source,
            1,
            4,
            SourceSnapshot::Hash(source_hash),
        ));
    let started = Instant::now();
    let result = runtime
        .inspect_symbol(InspectSymbolRequest {
            symbol_handle: Some(handle),
            query: None,
            kind: None,
            path: None,
            container: None,
            candidate_limit: 5,
            sections: vec![
                crate::bridge::InspectSymbolSectionKind::Declaration,
                crate::bridge::InspectSymbolSectionKind::Implementations,
                crate::bridge::InspectSymbolSectionKind::References,
                crate::bridge::InspectSymbolSectionKind::Calls,
            ],
            budget: crate::bridge::InspectSymbolBudget {
                max_bytes: 30_000,
                max_items: 80,
            },
        })
        .await
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_millis(450),
        "independent sections ran serially in {:?}",
        started.elapsed()
    );
    assert_eq!(
        result.sections.calls.completeness,
        crate::bridge::InspectSectionCompleteness::Complete,
        "{:?}",
        result.sections.calls.reason
    );
    release_server.send(()).unwrap();
    responder.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn inspect_symbol_path_resolution_does_not_request_document_outline() {
    use crate::bridge::translator::testing::{
        FakeServer, read_framed_message, translator_with_capabilities, write_response,
    };

    let root = TempDir::new().unwrap();
    let source = root.path().join("lib.rs");
    let other = root.path().join("other.rs");
    fs::write(&source, "fn inspected() {}\n").unwrap();
    fs::write(&other, "fn inspected() {}\n").unwrap();
    let uri = crate::bridge::path_to_uri(&source).unwrap();
    let other_uri = crate::bridge::path_to_uri(&other).unwrap();
    let server_id = ServerId::from("rust");
    let capabilities = lsp_types::ServerCapabilities {
        document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, server) = translator_with_capabilities(&root, &server_id, capabilities);
    let FakeServer {
        _write_half: write_half,
        _read_half: read_half,
        mut read_half_stdin,
        mut write_stdout,
    } = server;
    let (method_tx, method_rx) = oneshot::channel();
    let (release_server, hold_server) = oneshot::channel();
    let responder = tokio::spawn(async move {
        let processes = (write_half, read_half);
        let mut reader = BufReader::new(&mut write_stdout);
        loop {
            let message = read_framed_message(&mut reader).await;
            let Some(id) = message.get("id") else {
                continue;
            };
            let method = message["method"].as_str().unwrap().to_owned();
            method_tx.send(method.clone()).unwrap();
            let response = if method == "workspace/symbol" {
                serde_json::json!([
                    {
                        "name": "inspected",
                        "kind": 12,
                        "location": {
                            "uri": other_uri,
                            "range": {
                                "start": {"line": 0, "character": 3},
                                "end": {"line": 0, "character": 12}
                            }
                        }
                    },
                    {
                        "name": "inspected",
                        "kind": 12,
                        "location": {
                            "uri": uri,
                            "range": {
                                "start": {"line": 0, "character": 3},
                                "end": {"line": 0, "character": 12}
                            }
                        }
                    }
                ])
            } else {
                serde_json::json!([])
            };
            write_response(&mut read_half_stdin, id, response).await;
            break;
        }
        let _ = hold_server.await;
        drop(processes);
    });

    let runtime = ProjectRuntime::new(translator);
    let result = Box::pin(runtime.inspect_symbol(InspectSymbolRequest {
        symbol_handle: None,
        query: Some("inspected".to_owned()),
        kind: None,
        path: Some("lib.rs".to_owned()),
        container: None,
        candidate_limit: 1,
        sections: Vec::new(),
        budget: crate::bridge::InspectSymbolBudget::default(),
    }))
    .await;

    assert_eq!(method_rx.await.unwrap(), "workspace/symbol");
    let result = result.unwrap();
    let crate::bridge::InspectSymbolResolution::Selected {
        symbol: Some(symbol),
        ..
    } = result.resolution
    else {
        panic!("requested path must select its symbol before applying the candidate limit");
    };
    assert_eq!(
        symbol.location.path.as_deref(),
        Some(source.to_str().unwrap())
    );
    release_server.send(()).unwrap();
    responder.await.unwrap();
}

#[test]
fn empty_call_hierarchy_is_not_misclassified_as_non_callable() {
    let section = missing_call_hierarchy_item();

    assert_eq!(
        section.completeness,
        crate::bridge::InspectSectionCompleteness::Unavailable
    );
    assert_eq!(
        section.reason.as_deref(),
        Some("call hierarchy provider returned no item at the symbol selection")
    );
}

#[tokio::test]
async fn inspect_symbol_returns_source_bearing_candidates_for_duplicate_names() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("one.rs"), "fn duplicate() -> u8 { 1 }\n").unwrap();
    fs::write(root.path().join("two.rs"), "fn duplicate() -> u8 { 2 }\n").unwrap();
    let mut translator =
        Translator::new().with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let actor = spawn_project_actor_with_translator(4, translator);
    let result = actor
        .inspect_symbol(InspectSymbolRequest {
            symbol_handle: None,
            query: Some("duplicate".to_owned()),
            kind: Some("function".to_owned()),
            path: None,
            container: None,
            candidate_limit: 10,
            sections: Vec::new(),
            budget: crate::bridge::InspectSymbolBudget::default(),
        })
        .await
        .unwrap();

    let crate::bridge::InspectSymbolResolution::Ambiguous { candidates } = result.resolution else {
        panic!("duplicate symbols must remain ambiguous");
    };
    assert_eq!(candidates.len(), 2);
    assert!(candidates.iter().all(|candidate| matches!(
        candidate.location.source,
        crate::bridge::SourceContext::Available(_)
    )));
}

#[tokio::test]
async fn inspect_symbol_treats_a_large_requested_budget_as_an_upper_bound() {
    const PAGE_LIMIT: usize = 16 * 1024;

    let root = TempDir::new().unwrap();
    for index in 0..40 {
        fs::write(
            root.path().join(format!("duplicate_{index}.rs")),
            format!(
                "// {}\nfn duplicate() -> u8 {{ {index} }}\n",
                "x".repeat(1_024)
            ),
        )
        .unwrap();
    }
    let mut translator =
        Translator::new().with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let actor = spawn_project_actor_with_translator(4, translator);

    let result = actor
        .inspect_symbol(InspectSymbolRequest {
            symbol_handle: None,
            query: Some("duplicate".to_owned()),
            kind: Some("function".to_owned()),
            path: None,
            container: None,
            candidate_limit: 100,
            sections: Vec::new(),
            budget: crate::bridge::InspectSymbolBudget {
                max_bytes: 45_000,
                max_items: 80,
            },
        })
        .await
        .unwrap();

    assert_eq!(result.budget.max_bytes, PAGE_LIMIT);
    let serialized = serde_json::to_vec(&result).unwrap();
    assert!(serialized.len() <= PAGE_LIMIT);
    assert_eq!(result.returned_bytes, serialized.len());
    assert!(result.truncated);
}

#[test]
fn symbol_handle_store_expires_entries() {
    let mut store = SymbolHandleStore {
        entries: HashMap::new(),
        ttl: Duration::ZERO,
        max_entries: 1,
    };
    let handle = store.insert(StoredSymbolTarget::new(
        PathBuf::from("lib.rs"),
        1,
        1,
        SourceSnapshot::Version(1),
    ));
    assert!(store.resolve(&handle).is_err());
}

#[tokio::test]
async fn symbol_handle_rejects_a_new_dirty_document_version() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("dirty.rs");
    fs::write(&source, "fn before() {}\n").unwrap();
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator
        .document_tracker()
        .open(source.clone(), "fn before() {}\n".to_owned())
        .unwrap();
    let runtime = ProjectRuntime::new(translator);
    let handle = runtime
        .symbol_handles
        .lock()
        .unwrap()
        .insert(StoredSymbolTarget::new(
            source.clone(),
            1,
            4,
            SourceSnapshot::Version(1),
        ));
    runtime
        .translator
        .document_tracker()
        .update(&source, "fn after() {}\n".to_owned());

    let error = runtime.resolve_symbol_target(&handle).await.unwrap_err();
    assert!(error.contains("stale_symbol_handle"));
}

#[tokio::test]
async fn inspect_symbol_returns_refresh_result_for_unknown_or_expired_handles() {
    let runtime = ProjectRuntime::new(Translator::new());
    let foreign_runtime = ProjectRuntime::new(Translator::new());
    let target = || {
        StoredSymbolTarget::new(
            PathBuf::from("private-other-project.rs"),
            1,
            1,
            SourceSnapshot::Version(1),
        )
    };
    let foreign = foreign_runtime
        .symbol_handles
        .lock()
        .unwrap()
        .insert(target());
    let expired = {
        let mut store = runtime.symbol_handles.lock().unwrap();
        store.ttl = Duration::ZERO;
        store.insert(target())
    };

    for handle in [SymbolHandle::new(), foreign, expired] {
        let result = runtime
            .inspect_symbol(InspectSymbolRequest {
                symbol_handle: Some(handle.clone()),
                query: None,
                kind: None,
                path: None,
                container: None,
                candidate_limit: 5,
                sections: Vec::new(),
                budget: crate::bridge::InspectSymbolBudget::default(),
            })
            .await
            .expect("unresolvable handles must return a refresh result, not an actor error");
        let crate::bridge::InspectSymbolResolution::Stale {
            symbol_handle,
            reason,
            retryable,
        } = result.resolution
        else {
            panic!("unresolvable handles must produce a structured refresh result");
        };
        assert_eq!(symbol_handle, handle);
        assert!(retryable);
        assert!(reason.starts_with("invalid_symbol_handle:"));
        assert!(!reason.contains("private-other-project.rs"));
        assert_eq!(
            serde_json::to_value(result.sections).unwrap(),
            serde_json::json!({})
        );
    }
}

#[tokio::test]
async fn inspect_symbol_returns_retryable_result_for_stale_handle() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("stale.rs");
    fs::write(&source, "fn before() {}\n").unwrap();
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let runtime = ProjectRuntime::new(translator);
    let (_, _, source_hash, _) = runtime.translator.source_snapshot(&source).await.unwrap();
    let handle = runtime
        .symbol_handles
        .lock()
        .unwrap()
        .insert(StoredSymbolTarget::new(
            source.clone(),
            1,
            4,
            SourceSnapshot::Hash(source_hash),
        ));
    fs::write(&source, "fn after() {}\n").unwrap();

    let result = runtime
        .inspect_symbol(InspectSymbolRequest {
            symbol_handle: Some(handle.clone()),
            query: None,
            kind: None,
            path: None,
            container: None,
            candidate_limit: 5,
            sections: Vec::new(),
            budget: crate::bridge::InspectSymbolBudget::default(),
        })
        .await
        .unwrap();
    let crate::bridge::InspectSymbolResolution::Stale {
        symbol_handle,
        reason,
        retryable,
    } = result.resolution
    else {
        panic!("stale handles must produce a structured refresh result");
    };
    assert_eq!(symbol_handle, handle);
    assert!(retryable);
    assert!(reason.starts_with("stale_symbol_handle:"));
}

#[tokio::test]
async fn inspect_symbol_accepts_a_handle_bound_to_the_current_dirty_version() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("dirty.rs");
    fs::write(&source, "fn disk_name() {}\n").unwrap();
    let mut translator =
        Translator::new().with_extensions(HashMap::from([("rs".to_owned(), "rust".to_owned())]));
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator
        .document_tracker()
        .open(source, "fn dirty_name() {}\n".to_owned())
        .unwrap();
    let runtime = ProjectRuntime::new(translator);
    let handle = runtime
        .symbol_handles
        .lock()
        .unwrap()
        .insert(StoredSymbolTarget::new(
            root.path().join("dirty.rs"),
            1,
            4,
            SourceSnapshot::Version(1),
        ));

    let result = Box::pin(runtime.inspect_symbol(InspectSymbolRequest {
        symbol_handle: Some(handle),
        query: None,
        kind: None,
        path: None,
        container: None,
        candidate_limit: 10,
        sections: vec![crate::bridge::InspectSymbolSectionKind::Diagnostics],
        budget: crate::bridge::InspectSymbolBudget::default(),
    }))
    .await
    .unwrap();

    assert!(matches!(
        result.resolution,
        crate::bridge::InspectSymbolResolution::Selected { .. }
    ));
}

#[test]
fn git_identity_resolves_main_checkout_and_linked_worktree() {
    let repository = TempDir::new().unwrap();
    let git_dir = repository.path().join(".git");
    let worktree_git_dir = git_dir.join("worktrees").join("feature");
    fs::create_dir_all(&worktree_git_dir).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();
    fs::create_dir(git_dir.join("objects")).unwrap();
    fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

    let worktree = TempDir::new().unwrap();
    fs::write(
        worktree.path().join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .unwrap();

    let main_identity = GitRepositoryIdentity::discover(repository.path())
        .unwrap()
        .unwrap();
    let worktree_identity = GitRepositoryIdentity::discover(worktree.path())
        .unwrap()
        .unwrap();

    assert_eq!(main_identity.common_dir(), git_dir.canonicalize().unwrap());
    assert_eq!(worktree_identity, main_identity);
}

#[test]
fn git_identity_distinguishes_non_git_and_stale_metadata() {
    let plain = TempDir::new().unwrap();
    assert!(
        GitRepositoryIdentity::discover(plain.path())
            .unwrap()
            .is_none()
    );

    let stale = TempDir::new().unwrap();
    fs::write(stale.path().join(".git"), "gitdir: /missing/worktree\n").unwrap();
    assert!(matches!(
        GitRepositoryIdentity::discover(stale.path()),
        Err(GitRepositoryIdentityError::MissingGitDirectory { .. })
    ));
}

#[tokio::test]
async fn rust_compatibility_key_changes_with_server_configuration() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();

    let mut first = Translator::new();
    first.set_lsp_configs(
        vec![crate::config::LspServerConfig::rust_analyzer()],
        Some(10),
    );
    let mut changed_config = crate::config::LspServerConfig::rust_analyzer();
    changed_config.args.push("--log-file=ra.log".to_string());
    let mut second = Translator::new();
    second.set_lsp_configs(vec![changed_config], Some(10));

    assert_ne!(
        rust_project_compatibility_key(root.path(), Some(&first.configuration_template())).await,
        rust_project_compatibility_key(root.path(), Some(&second.configuration_template())).await,
    );
}

#[tokio::test]
async fn rust_compatibility_key_changes_with_edit_safety_policy() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();

    let mut translator = Translator::new();
    translator.set_lsp_configs(
        vec![crate::config::LspServerConfig::rust_analyzer()],
        Some(10),
    );
    let first = translator.configuration_template();
    let second = first.clone().with_project_config(&ProjectConfig {
        edit_safety: Some(EditSafetyConfig {
            audit_log: Some(crate::config::AuditLogConfig {
                path: PathBuf::from("audit.jsonl"),
                max_bytes: 4_096,
                failure_mode: crate::edit_plan::AuditFailureMode::FailClosed,
            }),
            backup: None,
        }),
        ..ProjectConfig::default()
    });

    assert_ne!(
        rust_project_compatibility_key(root.path(), Some(&first)).await,
        rust_project_compatibility_key(root.path(), Some(&second)).await
    );
}

#[tokio::test]
async fn rust_compatibility_key_changes_with_file_patterns() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();

    let mut first_config = crate::config::LspServerConfig::rust_analyzer();
    first_config.file_patterns = vec!["**/*.rs".to_string()];
    let mut second_config = first_config.clone();
    second_config.file_patterns = vec!["**/*.rs", "**/*.toml"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut first = Translator::new();
    first.set_lsp_configs(vec![first_config], Some(10));
    let mut second = Translator::new();
    second.set_lsp_configs(vec![second_config], Some(10));

    assert_ne!(
        rust_project_compatibility_key(root.path(), Some(&first.configuration_template())).await,
        rust_project_compatibility_key(root.path(), Some(&second.configuration_template())).await,
    );
}

#[tokio::test]
async fn rust_compatibility_key_ignores_manifest_and_lockfile_contents() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    for root in [first.path(), second.path()] {
        fs::write(
            root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
    }
    fs::write(
        first.path().join("Cargo.toml"),
        "[package]\nname = \"first\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        second.path().join("Cargo.toml"),
        "[package]\nname = \"second\"\nversion = \"0.2.0\"\n",
    )
    .unwrap();
    fs::write(first.path().join("Cargo.lock"), "version = 3\n").unwrap();
    fs::write(
        second.path().join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"dependency\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    let mut translator = Translator::new();
    translator.set_lsp_configs(
        vec![crate::config::LspServerConfig::rust_analyzer()],
        Some(10),
    );
    let template = translator.configuration_template();

    assert_eq!(
        rust_project_compatibility_key(first.path(), Some(&template)).await,
        rust_project_compatibility_key(second.path(), Some(&template)).await
    );
}

#[tokio::test]
async fn rust_compatibility_key_rejects_dynamic_project_environment() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join(".envrc"),
        "export RUSTFLAGS=-Ctarget-cpu=native\n",
    )
    .unwrap();

    let mut translator = Translator::new();
    translator.set_lsp_configs(
        vec![crate::config::LspServerConfig::rust_analyzer()],
        Some(10),
    );

    assert_eq!(
        rust_project_compatibility_key(root.path(), Some(&translator.configuration_template()))
            .await,
        None
    );

    fs::remove_file(root.path().join(".envrc")).unwrap();
    fs::write(root.path().join("flake.nix"), "{}\n").unwrap();
    assert_eq!(
        rust_project_compatibility_key(root.path(), Some(&translator.configuration_template()))
            .await,
        None
    );
}

#[test]
fn rust_compatibility_environment_ignores_ephemeral_values() {
    let mut first = HashMap::from([
        ("DIRENV_DIFF".to_string(), Some("first".to_string())),
        ("PWD".to_string(), Some("/first".to_string())),
        (
            "RUSTFLAGS".to_string(),
            Some("-Ctarget-cpu=native".to_string()),
        ),
    ]);
    let mut second = first.clone();
    second.insert("DIRENV_DIFF".to_string(), Some("second".to_string()));
    second.insert("PWD".to_string(), Some("/second".to_string()));

    let fingerprint = |environment: &HashMap<String, Option<String>>| {
        let mut hasher = Sha256::new();
        hash_project_environment(&mut hasher, Some(environment));
        hasher.finalize()
    };

    assert_eq!(fingerprint(&first), fingerprint(&second));
    first.insert(
        "RUSTFLAGS".to_string(),
        Some("-Ctarget-cpu=generic".to_string()),
    );
    assert_ne!(fingerprint(&first), fingerprint(&second));
}

#[tokio::test]
async fn rust_compatibility_key_rejects_unavailable_toolchains() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"mcpls-definitely-missing\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();

    let mut translator = Translator::new();
    translator.set_lsp_configs(
        vec![crate::config::LspServerConfig::rust_analyzer()],
        Some(10),
    );

    assert_eq!(
        rust_project_compatibility_key(root.path(), Some(&translator.configuration_template()))
            .await,
        None
    );
}

#[test]
fn resolve_path_selects_longest_registered_root() {
    let workspace = TempDir::new().unwrap();
    let nested = workspace.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let file = nested.join("src.rs");
    fs::write(&file, "fn main() {}").unwrap();

    let outer = ProjectIdentity::new(
        ProjectId::new("outer").unwrap(),
        CanonicalRoot::new(workspace.path()).unwrap(),
    );
    let inner = ProjectIdentity::new(
        ProjectId::new("inner").unwrap(),
        CanonicalRoot::new(&nested).unwrap(),
    );
    let project_resolver = ProjectResolver::new([outer, inner]).unwrap();

    let resolved = project_resolver.resolve_path(&file).unwrap();

    assert_eq!(resolved.id().as_str(), "inner");
}

#[test]
fn new_rejects_duplicate_project_ids() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let projects = [
        ProjectIdentity::new(
            ProjectId::new("same").unwrap(),
            CanonicalRoot::new(first.path()).unwrap(),
        ),
        ProjectIdentity::new(
            ProjectId::new("same").unwrap(),
            CanonicalRoot::new(second.path()).unwrap(),
        ),
    ];

    assert!(matches!(
        ProjectResolver::new(projects),
        Err(ProjectIdentityError::DuplicateId(id)) if id.as_str() == "same"
    ));
}

#[test]
fn resolve_rejects_explicit_id_and_path_mismatch() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let file = second.path().join("src.rs");
    fs::write(&file, "fn main() {}").unwrap();
    let first_id = ProjectId::new("first").unwrap();
    let projects = [
        ProjectIdentity::new(first_id.clone(), CanonicalRoot::new(first.path()).unwrap()),
        ProjectIdentity::new(
            ProjectId::new("second").unwrap(),
            CanonicalRoot::new(second.path()).unwrap(),
        ),
    ];
    let project_resolver = ProjectResolver::new(projects).unwrap();

    assert!(matches!(
        project_resolver.resolve(Some(&first_id), Some(&file)),
        Err(ProjectIdentityError::ProjectPathMismatch { id, .. }) if id == first_id
    ));
}

#[test]
fn longest_matching_root_uses_path_components() {
    let roots = vec![
        PathBuf::from("/workspace/project"),
        PathBuf::from("/workspace/project/nested"),
        PathBuf::from("/workspace/project-other"),
    ];

    let root = longest_matching_root(Path::new("/workspace/project/nested/src.rs"), &roots);

    assert_eq!(root, Some(Path::new("/workspace/project/nested")));
}

#[test]
fn resolve_path_reports_deleted_project_root() {
    let workspace = TempDir::new().unwrap();
    let root = workspace.path().to_path_buf();
    let file = root.join("src.rs");
    fs::write(&file, "fn main() {}").unwrap();
    let project = ProjectIdentity::new(
        ProjectId::new("deleted").unwrap(),
        CanonicalRoot::new(&root).unwrap(),
    );
    let project_resolver = ProjectResolver::new([project]).unwrap();
    fs::remove_dir_all(&root).unwrap();

    assert!(matches!(
        project_resolver.resolve_path(&file),
        Err(ProjectIdentityError::ProjectRootUnavailable(id)) if id.as_str() == "deleted"
    ));
}

#[tokio::test]
async fn project_actor_reports_status_transitions() {
    let handle = spawn_project_actor(4);

    assert_eq!(handle.status().borrow().clone(), ProjectStatus::Starting);
    handle.set_status(ProjectStatus::Ready).await.unwrap();

    assert_eq!(handle.status().borrow().clone(), ProjectStatus::Ready);
}

#[tokio::test]
async fn project_actor_skips_cancelled_queued_mutation() {
    let actor = spawn_project_actor(2);
    let (reply, response) = oneshot::channel();
    drop(response);
    actor
        .sender
        .send(ProjectRequest::SetStatus {
            status: ProjectStatus::Ready,
            reply,
        })
        .await
        .unwrap();

    assert_eq!(
        actor.query().await.unwrap().status(),
        ProjectStatus::Starting
    );
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn project_actor_delivers_active_mutation_after_response_cancellation() {
    let (status_tx, _) = watch::channel(ProjectStatus::Starting);
    let (state_tx, _) = watch::channel(ProjectState::new(
        ProjectStatus::Starting,
        ProjectRuntimeSummary::default(),
    ));
    let (event_tx, _) = broadcast::channel(1);
    let channels = ProjectActorChannels {
        status_tx,
        state_tx,
        event_tx,
        event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
        gate: ProjectRequestGate::new(),
    };
    let (sender, _receiver) = mpsc::channel(1);
    let actor_sender = sender.downgrade();
    let mut runtime = ProjectRuntime::new(Translator::new());
    let mut state = ProjectState::new(ProjectStatus::Starting, runtime.summary());
    let (reply, response) = oneshot::channel();
    drop(response);

    assert!(
        !handle_project_request(
            ProjectRequest::SetStatus {
                status: ProjectStatus::Ready,
                reply,
            },
            &actor_sender,
            &channels,
            &mut state,
            &mut runtime,
            None,
        )
        .await
    );
    assert_eq!(state.status(), ProjectStatus::Ready);
}

#[tokio::test]
async fn project_request_waiting_on_full_queue_is_rejected_when_work_closes() {
    let (sender, mut receiver) = mpsc::channel(1);
    let sender = ProjectRequestSender::new(sender);
    sender
        .send(ProjectRequest::ServerExited { generation: 0 })
        .await
        .unwrap();

    let (reply, _response) = oneshot::channel();
    let mut pending = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .send(ProjectRequest::SetStatus {
                    status: ProjectStatus::Ready,
                    reply,
                })
                .await
        }
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut pending)
            .await
            .is_err()
    );
    sender.reject_new_work();
    let _ = receiver.recv().await;

    let result = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
}

#[tokio::test]
async fn project_request_sender_attaches_queue_timing_before_enqueue() {
    let (channel, mut receiver) = mpsc::channel(1);
    let sender = ProjectRequestSender::new(channel);

    sender
        .send(ProjectRequest::ServerExited { generation: 0 })
        .await
        .unwrap();

    let request = receiver.recv().await.unwrap();
    let (request, timing) = request.into_timed();
    assert!(matches!(
        request,
        ProjectRequest::ServerExited { generation: 0 }
    ));
    assert!(timing.queued_at.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn queued_resident_request_pins_group_before_actor_dequeues_it() {
    let controller = RustResidencyController::new(1);
    let (first_channel, mut first_receiver) = mpsc::channel(4);
    let (second_channel, mut second_receiver) = mpsc::channel(4);
    let first_residency = ProjectResidency {
        controller: controller.clone(),
        group: RustGroupId(1),
    };
    let second_residency = ProjectResidency {
        controller: controller.clone(),
        group: RustGroupId(2),
    };
    controller.register(RustGroupId(1), first_channel.downgrade());
    controller.register(RustGroupId(2), second_channel.downgrade());
    let first_sender = ProjectRequestSender::with_residency(first_channel, first_residency);
    let second_sender = ProjectRequestSender::with_residency(second_channel, second_residency);

    let (first_reply, _first_response) = oneshot::channel();
    first_sender
        .send(ProjectRequest::Activate {
            root: PathBuf::from("first"),
            reply: first_reply,
        })
        .await
        .unwrap();

    let (second_reply, _second_response) = oneshot::channel();
    let mut second_send = tokio::spawn(async move {
        second_sender
            .send(ProjectRequest::Activate {
                root: PathBuf::from("second"),
                reply: second_reply,
            })
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second_send)
            .await
            .is_err()
    );

    let first_request = first_receiver.recv().await.unwrap();
    assert!(matches!(first_request, ProjectRequest::Resident { .. }));
    drop(first_request);
    let suspend = tokio::time::timeout(Duration::from_secs(1), first_receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let ProjectRequest::Suspend { reply, .. } = suspend else {
        panic!("expected eviction only after the queued request completed");
    };
    reply.send(Ok(())).unwrap();

    second_send.await.unwrap().unwrap();
    assert!(matches!(
        second_receiver.recv().await.unwrap(),
        ProjectRequest::Resident { .. }
    ));
}

#[test]
fn project_request_modes_distinguish_activation_from_activity() {
    let (query_reply, _query_response) = oneshot::channel();
    assert_eq!(
        ProjectRequest::Query { reply: query_reply }.rust_residency_mode(),
        Some(RustResidencyMode::Touch)
    );

    let (activate_reply, _activate_response) = oneshot::channel();
    assert_eq!(
        ProjectRequest::Activate {
            root: PathBuf::from("project"),
            reply: activate_reply,
        }
        .rust_residency_mode(),
        Some(RustResidencyMode::Activate)
    );
}

#[tokio::test]
async fn touching_a_resident_project_request_does_not_wait_for_capacity() {
    let controller = RustResidencyController::with_idle_timeout(1, Duration::from_secs(60 * 60));
    let (channel, _receiver) = mpsc::channel(1);
    let residency = ProjectResidency {
        controller: controller.clone(),
        group: RustGroupId(1),
    };
    controller.register(RustGroupId(1), channel.downgrade());
    drop(controller.acquire(RustGroupId(1)).await);

    let (reply, _response) = oneshot::channel();
    let touched = residency.touch_request(ProjectRequest::Query { reply });
    assert!(matches!(touched, ProjectRequest::Resident { .. }));
}

#[tokio::test]
async fn project_actor_publishes_typed_status_events() {
    let handle = spawn_project_actor(4);
    let mut events = handle.subscribe_events();

    handle.set_status(ProjectStatus::Ready).await.unwrap();

    assert_eq!(
        events.recv().await.unwrap(),
        ProjectEvent::StatusChanged {
            status: ProjectStatus::Ready,
            last_error: None,
        }
    );
}

#[test]
fn project_event_history_bounds_records_and_reports_cursor_resync() {
    let mut history = ProjectEventHistory::new(2);
    history.record(ProjectEvent::StatusChanged {
        status: ProjectStatus::Starting,
        last_error: None,
    });
    history.record(ProjectEvent::StatusChanged {
        status: ProjectStatus::Ready,
        last_error: None,
    });
    history.record(ProjectEvent::ServerExited { generation: 1 });

    let snapshot = history.snapshot_since(Some(0), 2);
    assert!(snapshot.resync_required());
    assert_eq!(snapshot.events().len(), 2);
    assert_eq!(snapshot.events()[0].sequence(), 2);
    assert_eq!(snapshot.events()[1].sequence(), 3);
    assert_eq!(snapshot.next_sequence(), 3);

    history.record(ProjectEvent::ServerExited { generation: 2 });
    let resumed = history.snapshot_since(Some(snapshot.next_sequence()), 2);
    assert_eq!(resumed.events().len(), 1);
    assert_eq!(resumed.events()[0].sequence(), 4);
}

#[test]
fn project_event_history_pages_an_exclusive_cursor_without_gaps() {
    let mut history = ProjectEventHistory::new(4);
    for generation in 1..=4 {
        history.record(ProjectEvent::ServerExited { generation });
    }

    let first = history.snapshot_since(None, 2);
    assert!(first.truncated());
    assert_eq!(first.next_sequence(), 2);
    assert_eq!(
        first
            .events()
            .iter()
            .map(ProjectEventRecord::sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );

    let second = history.snapshot_since(Some(first.next_sequence()), 2);
    assert!(!second.truncated());
    assert_eq!(second.next_sequence(), 4);
    assert_eq!(
        second
            .events()
            .iter()
            .map(ProjectEventRecord::sequence)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
}

#[test]
fn project_event_history_retains_edit_completion_and_file_change_payloads() {
    let mut history = ProjectEventHistory::new(4);
    let plan_id = PlanId::parse("plan-1").unwrap();
    history.record(ProjectEvent::FilesChanged {
        paths: vec![PathBuf::from("/workspace/main.rs")],
    });
    history.record(ProjectEvent::EditApplied {
        plan_id: plan_id.clone(),
        committed_files: vec![PathBuf::from("/workspace/main.rs")],
        operation_count: 1,
    });

    let snapshot = history.snapshot_since(None, 4);
    assert!(matches!(
        snapshot.events()[0].event(),
        ProjectEvent::FilesChanged { paths } if paths.len() == 1
    ));
    assert!(matches!(
        snapshot.events()[1].event(),
        ProjectEvent::EditApplied {
            plan_id: actual,
            operation_count: 1,
            ..
        } if actual == &plan_id
    ));
    assert_eq!(
        snapshot.events()[0].event().json_value(),
        serde_json::json!({
            "kind": "files_changed",
            "paths": ["/workspace/main.rs"],
        })
    );
    assert_eq!(
        snapshot.events()[1].event().json_value(),
        serde_json::json!({
            "kind": "edit_applied",
            "plan_id": "plan-1",
            "committed_files": ["/workspace/main.rs"],
            "operation_count": 1,
        })
    );
}

#[tokio::test]
async fn project_actor_publishes_server_exit_events_before_recovery_status() {
    let handle = spawn_project_actor(4);
    let mut events = handle.subscribe_events();
    handle.set_status(ProjectStatus::Ready).await.unwrap();
    let _ = events.recv().await.unwrap();

    handle
        .sender
        .send(ProjectRequest::ServerExited { generation: 0 })
        .await
        .unwrap();

    assert_eq!(
        events.recv().await.unwrap(),
        ProjectEvent::ServerExited { generation: 0 }
    );
    assert_eq!(
        events.recv().await.unwrap(),
        ProjectEvent::StatusChanged {
            status: ProjectStatus::Restarting,
            last_error: Some("language server exited; restarting (attempt 1/3)".to_string(),),
        }
    );
    assert_eq!(
        events.recv().await.unwrap(),
        ProjectEvent::StatusChanged {
            status: ProjectStatus::Ready,
            last_error: None,
        }
    );
}

#[tokio::test]
async fn project_actor_shutdown_publishes_stopped_and_closes_requests() {
    let handle = spawn_project_actor(1);

    handle.shutdown().await.unwrap();

    assert_eq!(handle.status().borrow().clone(), ProjectStatus::Stopped);
    assert!(matches!(
        handle.set_status(ProjectStatus::Ready).await,
        Err(ProjectActorError::Closed)
    ));
}

#[tokio::test]
async fn dropping_last_project_handle_stops_actor() {
    let handle = spawn_project_actor(1);
    let mut status = handle.status();

    drop(handle);

    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while *status.borrow() != ProjectStatus::Stopped {
                status.changed().await.unwrap();
            }
        })
        .await
        .is_ok(),
        "actor did not stop after its last handle was dropped"
    );
}

#[tokio::test]
async fn project_actor_exposes_typed_query_refresh_restart_and_failure() {
    let handle = spawn_project_actor(4);

    assert_eq!(
        handle.query().await.unwrap().status(),
        ProjectStatus::Starting
    );
    assert_eq!(
        handle.refresh().await.unwrap().status(),
        ProjectStatus::Starting
    );
    assert_eq!(
        handle.restart().await.unwrap().status(),
        ProjectStatus::Ready
    );

    handle.fail("rust-analyzer exited").await.unwrap();
    let state = handle.query().await.unwrap();
    assert_eq!(state.status(), ProjectStatus::Failed);
    assert_eq!(state.last_error(), Some("rust-analyzer exited"));
}

#[tokio::test]
async fn project_actor_owns_project_workspace_state() {
    let root = TempDir::new().unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let state = handle.query().await.unwrap();

    assert_eq!(
        state.workspace_roots(),
        &[root.path().canonicalize().unwrap()]
    );
    assert_eq!(state.open_document_count(), 0);
}

#[tokio::test]
async fn project_actor_routes_semantic_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("outside.rs"), "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .hover(
            outside.path().join("outside.rs").display().to_string(),
            0,
            0,
        )
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_definition_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle.definition(file.display().to_string(), 0, 0).await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_references_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .references(
            file.display().to_string(),
            0,
            0,
            false,
            SemanticResultLimits::default(),
        )
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_diagnostics_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle.diagnostics(file.display().to_string()).await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_rename_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .rename(file.display().to_string(), 0, 0, "renamed".to_string())
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_raw_rename_edits_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .rename_workspace_edit(file.display().to_string(), 0, 0, "renamed".to_string())
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_completion_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .completions(file.display().to_string(), 0, 0, None, None)
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_document_symbol_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .document_symbols(file.display().to_string(), DocumentSymbolOptions::default())
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_format_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .format_document(file.display().to_string(), 4, true)
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_raw_format_edits_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .format_workspace_edit(file.display().to_string(), 4, true)
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn generated_preview_keeps_lsp_generation_and_snapshotting_in_one_actor_request() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .preview_generated_edit(
            "project".to_owned(),
            GeneratedEditRequest::Format {
                file_path: file.display().to_string(),
                tab_size: 4,
                insert_spaces: true,
            },
            PositionEncoding::Utf8,
            root.path().to_path_buf(),
        )
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_code_action_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .code_actions(file.display().to_string(), 1, 5, 1, 15, None, None)
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_call_hierarchy_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .prepare_call_hierarchy(file.display().to_string(), 1, 5, None)
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_signature_help_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .signature_help(file.display().to_string(), 1, 5, None)
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_inlay_hints_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .inlay_hints(file.display().to_string(), 1, 5, 1, 15, None)
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_implementation_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .go_to_implementation(file.display().to_string(), 1, 5)
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_type_definition_requests_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle
        .go_to_type_definition(file.display().to_string(), 1, 5)
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_routes_cached_diagnostics_through_owned_translator() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = outside.path().join("outside.rs");
    fs::write(&file, "fn outside() {}\n").unwrap();
    let canonical_root = CanonicalRoot::new(root.path()).unwrap();
    let handle = spawn_project_actor_for_root(2, &canonical_root);

    let result = handle.cached_diagnostics(file.display().to_string()).await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message.contains("outside workspace")
    ));
}

#[tokio::test]
async fn project_actor_owns_notification_cache_for_server_logs() {
    let actor = spawn_project_actor(2);
    let notification = LspNotification::parse(
        "window/logMessage",
        Some(serde_json::json!({"type": 3, "message": "project log"})),
    );
    actor
        .sender
        .send(ProjectRequest::Notification {
            generation: 0,
            server_id: ServerId::from("rust"),
            notification,
        })
        .await
        .unwrap();

    let result = actor.server_logs(10, None).await.unwrap();
    assert_eq!(result.logs.len(), 1);
    assert_eq!(result.logs[0].message, "project log");
    assert_eq!(result.logs[0].generation, 0);
}

#[tokio::test]
async fn project_actor_exposes_semantic_notification_overflow_as_retry_state() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("src.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    let actor = spawn_project_actor_for_root(8, &CanonicalRoot::new(root.path()).unwrap());
    let mut events = actor.subscribe_events();

    actor
        .notify(
            0,
            ServerId::from("rust"),
            LspNotification::DeliveryOverflow {
                notification_kind: "publish_diagnostics",
                dropped_count: 7,
                semantic: true,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap(),
        ProjectEvent::NotificationOverflowed {
            server_id: "rust".to_owned(),
            notification_kind: "publish_diagnostics".to_owned(),
            dropped_count: 7,
            semantic: true,
        }
    );
    let diagnostics = actor
        .cached_diagnostics(file.display().to_string())
        .await
        .unwrap();
    assert!(
        diagnostics
            .cache
            .as_ref()
            .is_some_and(|cache| cache.resync_required)
    );
}

#[tokio::test]
async fn project_actor_publishes_diagnostics_events_for_notifications() {
    let actor = spawn_project_actor(2);
    let mut events = actor.subscribe_events();
    let uri = "file:///project/src/main.rs";
    let notification = LspNotification::parse(
        "textDocument/publishDiagnostics",
        Some(serde_json::json!({
            "uri": uri,
            "version": 7,
            "diagnostics": []
        })),
    );

    actor
        .sender
        .send(ProjectRequest::Notification {
            generation: 99,
            server_id: ServerId::from("rust"),
            notification: LspNotification::parse(
                "textDocument/publishDiagnostics",
                Some(serde_json::json!({
                    "uri": "file:///project/src/stale.rs",
                    "diagnostics": []
                })),
            ),
        })
        .await
        .unwrap();
    actor
        .sender
        .send(ProjectRequest::Notification {
            generation: 0,
            server_id: ServerId::from("rust"),
            notification,
        })
        .await
        .unwrap();

    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap(),
        ProjectEvent::DiagnosticsUpdated {
            uri: uri.to_string(),
            version: Some(7),
            diagnostic_count: 0,
        }
    );
}

#[tokio::test]
async fn project_actor_reports_cached_diagnostics_presence_after_notification() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("src.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    let actor = spawn_project_actor_for_root(2, &CanonicalRoot::new(root.path()).unwrap());
    let uri = crate::bridge::path_to_uri(&file).unwrap();
    actor
        .sender
        .send(ProjectRequest::Notification {
            generation: 0,
            server_id: ServerId::from("rust"),
            notification: LspNotification::parse(
                "textDocument/publishDiagnostics",
                Some(serde_json::json!({
                    "uri": uri,
                    "diagnostics": []
                })),
            ),
        })
        .await
        .unwrap();

    assert!(
        actor
            .has_cached_diagnostics(file.display().to_string())
            .await
            .unwrap()
    );
    let diagnostics = actor
        .cached_diagnostics(file.display().to_string())
        .await
        .unwrap();
    assert_eq!(
        diagnostics
            .cache
            .as_ref()
            .and_then(|cache| cache.document_version),
        None
    );
    assert!(diagnostics.cache.as_ref().is_some_and(|cache| cache.hit));
    assert_eq!(
        diagnostics
            .cache
            .as_ref()
            .and_then(|cache| cache.snapshot_identity.as_ref())
            .map(String::len),
        Some(64)
    );
}

#[tokio::test]
async fn project_actor_pages_cached_diagnostics_with_snapshot_owned_cursors() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("src.rs");
    fs::write(&file, "fn item() {}\n".repeat(300)).unwrap();
    let actor = spawn_project_actor_for_root(2, &CanonicalRoot::new(root.path()).unwrap());
    let uri = crate::bridge::path_to_uri(&file).unwrap();
    let diagnostics = (0..249)
        .map(|line| {
            serde_json::json!({
                "range": {
                    "start": {"line": line, "character": 0},
                    "end": {"line": line, "character": 2}
                },
                "severity": 4,
                "code": "macro-error",
                "source": "rust-analyzer",
                "message": "uniffi::constructor: internal café 🚗 proc-macro error"
            })
        })
        .collect::<Vec<_>>();
    actor
        .sender
        .send(ProjectRequest::Notification {
            generation: 0,
            server_id: ServerId::from("rust"),
            notification: LspNotification::parse(
                "textDocument/publishDiagnostics",
                Some(serde_json::json!({
                    "uri": uri,
                    "version": 7,
                    "diagnostics": diagnostics
                })),
            ),
        })
        .await
        .unwrap();

    let mut options = DiagnosticOptions {
        preserve_locations: true,
        item_limit: 20,
        byte_limit: 6_000,
        ..DiagnosticOptions::default()
    };
    let mut lines = Vec::new();
    let mut snapshot_identity = None;
    let mut first_cursor = None;
    loop {
        let result = actor
            .cached_diagnostics_with_options(file.display().to_string(), options)
            .await
            .unwrap();
        assert!(serde_json::to_vec(&result).unwrap().len() <= 6_000);
        assert_eq!(result.total_diagnostics, 249);
        assert!(result.source_resource.is_some());
        if let Some(identity) = &snapshot_identity {
            assert_eq!(result.snapshot_identity.as_ref(), Some(identity));
        } else {
            snapshot_identity = result.snapshot_identity.clone();
        }
        lines.extend(
            result
                .diagnostics
                .iter()
                .flat_map(|group| &group.context.occurrences)
                .map(|occurrence| occurrence.range.start.line),
        );
        assert_eq!(result.remaining_diagnostics, 249 - lines.len());

        let Some(cursor) = result.next_cursor else {
            break;
        };
        first_cursor.get_or_insert_with(|| cursor.clone());
        options = DiagnosticOptions {
            page_token: Some(cursor),
            ..DiagnosticOptions::default()
        };
    }

    assert_eq!(lines, (1..=249).collect::<Vec<_>>());
    let mismatched = actor
        .diagnostics_with_options(
            file.display().to_string(),
            DiagnosticOptions {
                page_token: first_cursor,
                ..DiagnosticOptions::default()
            },
        )
        .await;
    assert!(matches!(
        mismatched,
        Err(ProjectActorError::Operation(message))
            if message.contains("different diagnostics request")
    ));
}

#[tokio::test]
async fn project_actor_ignores_server_quiescence_until_initial_rust_indexing_finishes() {
    let translator = Translator::new();
    translator.set_expected_languages(HashSet::from(["rust".to_string()]));
    let actor = spawn_project_actor_with_translator(2, translator);

    actor
        .sender
        .send(ProjectRequest::Notification {
            generation: 0,
            server_id: ServerId::from("rust"),
            notification: LspNotification::parse(
                "experimental/serverStatus",
                Some(serde_json::json!({
                    "health": "ok",
                    "quiescent": false
                })),
            ),
        })
        .await
        .unwrap();
    assert_eq!(
        actor.query().await.unwrap().status(),
        ProjectStatus::Starting
    );

    actor
        .sender
        .send(ProjectRequest::Notification {
            generation: 0,
            server_id: ServerId::from("rust"),
            notification: LspNotification::parse(
                "experimental/serverStatus",
                Some(serde_json::json!({
                    "health": "ok",
                    "quiescent": true
                })),
            ),
        })
        .await
        .unwrap();
    assert_eq!(
        actor.query().await.unwrap().status(),
        ProjectStatus::Starting
    );

    actor
        .sender
        .send(ProjectRequest::Notification {
            generation: 0,
            server_id: ServerId::from("rust"),
            notification: LspNotification::parse(
                "$/progress",
                Some(serde_json::json!({
                    "token": "rustAnalyzer/Indexing",
                    "value": {"kind": "end"}
                })),
            ),
        })
        .await
        .unwrap();

    assert_eq!(actor.query().await.unwrap().status(), ProjectStatus::Ready);
}

#[tokio::test]
async fn repeated_activation_preserves_active_runtime_generation() {
    let root = TempDir::new().unwrap();
    let root = root.path().to_path_buf();
    let mut translator = Translator::new();
    let mut config = crate::config::LspServerConfig::rust_analyzer();
    config.heuristics = None;
    translator.set_workspace_roots(vec![root.clone()]);
    translator.set_lsp_configs(vec![config.clone()], None);
    translator.register_client(
        config.language_id.clone(),
        crate::lsp::LspClient::new(config),
    );
    translator.register_server_roots("rust".to_string(), vec![root.clone()]);

    let actor = spawn_project_actor_with_translator(2, translator);
    actor.set_status(ProjectStatus::Ready).await.unwrap();
    assert_eq!(actor.query().await.unwrap().runtime().generation(), 0);

    actor.activate(root).await.unwrap();

    assert_eq!(actor.query().await.unwrap().runtime().generation(), 0);
}

#[cfg(unix)]
const DUPLICATE_ACTIVATION_LSP: &str = r#"#!/usr/bin/env python3
import json
import os
import pathlib
import sys

counter = pathlib.Path(os.environ["MCPLS_SPAWN_COUNTER"])
value = int(counter.read_text()) if counter.exists() else 0
counter.write_text(str(value + 1))

def read_message():
    headers = b""
    while b"\r\n\r\n" not in headers:
        chunk = sys.stdin.buffer.read(1)
        if not chunk:
            return None
        headers += chunk
    length = next(
        int(line.split(b":", 1)[1].strip())
        for line in headers.split(b"\r\n")
        if line.lower().startswith(b"content-length:")
    )
    return json.loads(sys.stdin.buffer.read(length))

def send(message):
    body = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(
        b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body
    )
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    if message.get("method") == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "capabilities": {"positionEncoding": "utf-8"}
        }})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"health": "ok", "quiescent": True}})
        send({"jsonrpc": "2.0", "method": "$/progress",
              "params": {"token": "rustAnalyzer/Indexing", "value": {"kind": "end"}}})
    elif message.get("method") == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        break
"#;

#[cfg(unix)]
const CANCELLABLE_INITIALIZATION_LSP: &str = r#"#!/usr/bin/env python3
import os
import pathlib
import time

pathlib.Path(os.environ["MCPLS_PID_FILE"]).write_text(str(os.getpid()))
while True:
    time.sleep(1)
"#;

#[cfg(unix)]
const PROFILE_FAILURE_LSP: &str = r#"#!/usr/bin/env python3
import json
import sys

def read_message():
    headers = b""
    while b"\r\n\r\n" not in headers:
        chunk = sys.stdin.buffer.read(1)
        if not chunk:
            return None
        headers += chunk
    length = next(
        int(line.split(b":", 1)[1].strip())
        for line in headers.split(b"\r\n")
        if line.lower().startswith(b"content-length:")
    )
    return json.loads(sys.stdin.buffer.read(length))

def send(message):
    body = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(
        b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body
    )
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    if message.get("method") == "initialize":
        if "bad-feature" in json.dumps(message.get("params")):
            sys.exit(2)
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "capabilities": {"positionEncoding": "utf-8"}
        }})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"health": "ok", "quiescent": True}})
        send({"jsonrpc": "2.0", "method": "$/progress",
              "params": {"token": "rustAnalyzer/Indexing", "value": {"kind": "end"}}})
    elif message.get("method") == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        break
"#;

fn write_compatible_roots_with_changed_manifests(roots: &[&Path]) {
    for root in roots {
        fs::write(
            root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
    }
    let Some((first, linked)) = roots.split_first() else {
        return;
    };
    fs::write(
        first.join("Cargo.toml"),
        "[package]\nname = \"fixture-main\"\n",
    )
    .unwrap();
    fs::write(first.join("Cargo.lock"), "version = 3\n").unwrap();
    for root in linked {
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture-linked\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"changed\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
    }
}

fn compatible_worktree_fixture() -> (TempDir, Vec<TempDir>, Vec<PathBuf>) {
    let repository = TempDir::new().unwrap();
    let git_dir = repository.path().join(".git");
    fs::create_dir_all(git_dir.join("objects")).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();

    let worktrees: Vec<_> = (0..4)
        .map(|index| {
            let worktree_git_dir = git_dir.join("worktrees").join(format!("linked-{index}"));
            fs::create_dir_all(&worktree_git_dir).unwrap();
            fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
            let worktree = TempDir::new().unwrap();
            fs::write(
                worktree.path().join(".git"),
                format!("gitdir: {}\n", worktree_git_dir.display()),
            )
            .unwrap();
            worktree
        })
        .collect();
    let roots: Vec<_> = std::iter::once(repository.path().to_path_buf())
        .chain(
            worktrees
                .iter()
                .map(|worktree| worktree.path().to_path_buf()),
        )
        .collect();
    let root_refs: Vec<_> = roots.iter().map(PathBuf::as_path).collect();
    write_compatible_roots_with_changed_manifests(&root_refs);
    (repository, worktrees, roots)
}

async fn add_compatible_roots(
    registry: &ProjectRegistry,
    project_id: &ProjectId,
    roots: &[PathBuf],
) {
    for root in roots {
        let repository_identity = GitRepositoryIdentity::discover(root).unwrap().unwrap();
        registry
            .add(
                ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(root).unwrap())
                    .with_repository_identity(repository_identity),
            )
            .await
            .unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn repeated_activation_does_not_spawn_a_duplicate_lsp_process() {
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().unwrap();
    let counter = root.path().join("spawn-count");
    let lsp = root.path().join("counting-lsp.py");
    fs::write(&lsp, DUPLICATE_ACTIVATION_LSP).unwrap();
    let mut permissions = fs::metadata(&lsp).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&lsp, permissions).unwrap();

    let mut config = crate::config::LspServerConfig::rust_analyzer();
    config.command = lsp.display().to_string();
    config.heuristics = None;
    config.env = HashMap::from([(
        "MCPLS_SPAWN_COUNTER".to_string(),
        counter.display().to_string(),
    )]);
    let mut translator =
        Translator::new().with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator.set_lsp_configs(vec![config], Some(3));
    let actor = spawn_project_actor_with_translator(2, translator);

    let first = actor.activate(root.path().to_path_buf()).await.unwrap();
    assert!(matches!(
        first.status(),
        ProjectStatus::Starting | ProjectStatus::Ready
    ));
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");

    let second = actor.activate(root.path().to_path_buf()).await.unwrap();
    assert!(matches!(
        second.status(),
        ProjectStatus::Starting | ProjectStatus::Ready
    ));
    assert_eq!(second.runtime().generation(), first.runtime().generation());
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");

    let state = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let state = actor.query().await.unwrap();
            if state.status() == ProjectStatus::Ready {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(state.runtime().generation(), first.runtime().generation());
}

#[cfg(unix)]
#[tokio::test]
async fn degraded_activation_stays_degraded_after_initial_indexing() {
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().unwrap();
    let counter = root.path().join("spawn-count");
    let lsp = root.path().join("ready-lsp.py");
    fs::write(&lsp, DUPLICATE_ACTIVATION_LSP).unwrap();
    let mut permissions = fs::metadata(&lsp).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&lsp, permissions).unwrap();

    let mut ready = crate::config::LspServerConfig::rust_analyzer();
    ready.command = lsp.display().to_string();
    ready.heuristics = None;
    ready.env = HashMap::from([(
        "MCPLS_SPAWN_COUNTER".to_string(),
        counter.display().to_string(),
    )]);
    let mut unavailable = ready.clone();
    unavailable.language_id = "unavailable".to_string();
    unavailable.command = "/definitely/missing/mcpls-lsp".to_string();
    unavailable.env.clear();

    let mut translator =
        Translator::new().with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator.set_lsp_configs(vec![ready, unavailable], Some(3));
    let actor = spawn_project_actor_with_translator(4, translator);

    actor.activate(root.path().to_path_buf()).await.unwrap();
    let state = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let state = actor.query().await.unwrap();
            if state.status() != ProjectStatus::Starting {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert_eq!(state.status(), ProjectStatus::Degraded);
}

#[tokio::test]
async fn project_actor_marks_current_server_exit_failed_but_ignores_stale_exit() {
    let actor = spawn_project_actor(2);
    actor.set_status(ProjectStatus::Ready).await.unwrap();

    actor
        .sender
        .send(ProjectRequest::ServerExited { generation: 99 })
        .await
        .unwrap();
    assert_eq!(actor.query().await.unwrap().status(), ProjectStatus::Ready);

    actor.restart().await.unwrap();
    actor
        .sender
        .send(ProjectRequest::ServerExited { generation: 0 })
        .await
        .unwrap();
    assert_eq!(actor.query().await.unwrap().status(), ProjectStatus::Ready);

    actor
        .sender
        .send(ProjectRequest::ServerExited { generation: 1 })
        .await
        .unwrap();
    let state = actor.query().await.unwrap();
    assert_eq!(state.status(), ProjectStatus::Ready);
    assert_eq!(state.runtime().generation(), 2);
}

#[tokio::test]
async fn project_actor_fails_when_current_server_exits_during_starting() {
    let actor = spawn_project_actor(2);

    actor
        .sender
        .send(ProjectRequest::ServerExited { generation: 0 })
        .await
        .unwrap();

    let state = actor.query().await.unwrap();
    assert_eq!(state.status(), ProjectStatus::Failed);
    assert_eq!(state.last_error(), Some("language server exited"));
}

#[tokio::test]
async fn project_actor_restarts_after_current_server_exit() {
    let actor = spawn_project_actor(2);
    actor.set_status(ProjectStatus::Ready).await.unwrap();

    actor
        .sender
        .send(ProjectRequest::ServerExited { generation: 0 })
        .await
        .unwrap();

    let state = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let state = actor.query().await.unwrap();
            if state.status() != ProjectStatus::Restarting {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert_eq!(state.status(), ProjectStatus::Ready);
    assert_eq!(state.runtime().generation(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn project_actor_shutdown_cancels_pending_server_exit_recovery() {
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().unwrap();
    let counter = root.path().join("spawn-count");
    let lsp = root.path().join("counting-lsp.py");
    fs::write(&lsp, DUPLICATE_ACTIVATION_LSP).unwrap();
    let mut permissions = fs::metadata(&lsp).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&lsp, permissions).unwrap();

    let mut config = crate::config::LspServerConfig::rust_analyzer();
    config.command = lsp.display().to_string();
    config.heuristics = None;
    config.env = HashMap::from([(
        "MCPLS_SPAWN_COUNTER".to_string(),
        counter.display().to_string(),
    )]);
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator.set_lsp_configs(vec![config], Some(3));
    let actor = spawn_project_actor_with_translator(2, translator);

    actor.activate(root.path().to_path_buf()).await.unwrap();
    actor.set_status(ProjectStatus::Ready).await.unwrap();
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");

    actor
        .sender
        .send(ProjectRequest::ServerExited { generation: 1 })
        .await
        .unwrap();
    let shutdown = tokio::time::timeout(Duration::from_millis(50), actor.shutdown()).await;
    assert!(
        shutdown.is_ok(),
        "shutdown should cancel pending recovery promptly"
    );
    shutdown.unwrap().unwrap();

    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
    assert_eq!(actor.status().borrow().clone(), ProjectStatus::Stopped);
}

#[cfg(unix)]
#[tokio::test]
async fn project_actor_shutdown_cancels_initialization_and_reaps_lsp() {
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().unwrap();
    let pid_file = root.path().join("lsp.pid");
    let lsp = root.path().join("blocked-lsp.py");
    fs::write(&lsp, CANCELLABLE_INITIALIZATION_LSP).unwrap();
    let mut permissions = fs::metadata(&lsp).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&lsp, permissions).unwrap();

    let mut config = crate::config::LspServerConfig::rust_analyzer();
    config.command = lsp.display().to_string();
    config.heuristics = None;
    config.timeout_seconds = 30;
    config.env = HashMap::from([("MCPLS_PID_FILE".to_string(), pid_file.display().to_string())]);
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator.set_lsp_configs(vec![config], Some(3));
    let actor = spawn_project_actor_with_translator(2, translator);

    let activation = {
        let actor = actor.clone();
        let root = root.path().to_path_buf();
        tokio::spawn(async move { actor.activate(root).await })
    };
    let pid = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Ok(pid) = fs::read_to_string(&pid_file)
                && let Ok(pid) = pid.trim().parse::<u32>()
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let shutdown = tokio::time::timeout(Duration::from_secs(1), actor.shutdown()).await;
    assert!(
        shutdown.is_ok(),
        "shutdown should cancel blocked initialization"
    );
    shutdown.unwrap().unwrap();
    assert!(activation.await.unwrap().is_err());
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert_eq!(actor.status().borrow().clone(), ProjectStatus::Stopped);
}

#[test]
fn automatic_restart_policy_is_bounded_and_resettable() {
    let mut policy = AutomaticRestartPolicy::default();

    assert_eq!(
        policy.next().map(|attempt| (attempt.number, attempt.delay)),
        Some((1, Duration::from_millis(100)))
    );
    assert_eq!(
        policy.next().map(|attempt| (attempt.number, attempt.delay)),
        Some((2, Duration::from_millis(500)))
    );
    assert_eq!(
        policy.next().map(|attempt| (attempt.number, attempt.delay)),
        Some((3, Duration::from_secs(2)))
    );
    assert_eq!(policy.next(), None);

    policy.reset();
    assert_eq!(
        policy.next().map(|attempt| (attempt.number, attempt.delay)),
        Some((1, Duration::from_millis(100)))
    );
}

#[tokio::test]
async fn project_actor_retries_failed_restarts_until_the_policy_is_exhausted() {
    let root = TempDir::new().unwrap();
    let mut config = crate::config::LspServerConfig::rust_analyzer();
    config.command = "/definitely/missing/mcpls-language-server".to_string();
    config.heuristics = None;
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator.set_lsp_configs(vec![config], None);
    let actor = spawn_project_actor_with_translator(2, translator);
    actor.set_status(ProjectStatus::Ready).await.unwrap();

    actor
        .sender
        .send(ProjectRequest::ServerExited { generation: 0 })
        .await
        .unwrap();

    let state = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let state = actor.query().await.unwrap();
            if state.status() == ProjectStatus::Failed {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert_eq!(state.runtime().generation(), 3);
    assert!(
        state
            .last_error()
            .is_some_and(|error| error.contains("No such file") || error.contains("not found"))
    );
}

#[tokio::test]
async fn project_actor_rejects_queued_semantic_work_after_restart_exhaustion() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    let mut config = crate::config::LspServerConfig::rust_analyzer();
    config.command = "/definitely/missing/mcpls-language-server".to_string();
    config.heuristics = None;
    let mut extensions = HashMap::new();
    extensions.insert("rs".to_string(), "rust".to_string());
    let mut translator = Translator::new().with_extensions(extensions);
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator.set_lsp_configs(vec![config], None);
    let actor = spawn_project_actor_with_translator(2, translator);
    actor.set_status(ProjectStatus::Ready).await.unwrap();

    actor
        .sender
        .send(ProjectRequest::ServerExited { generation: 0 })
        .await
        .unwrap();
    let result = actor
        .document_symbols(
            root.path().join("src/main.rs").display().to_string(),
            DocumentSymbolOptions::default(),
        )
        .await;

    assert!(matches!(
        result,
        Err(ProjectActorError::Operation(message)) if message == "language server exited"
    ));
}

#[tokio::test]
async fn project_actor_can_add_a_linked_workspace_root() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let first_root = CanonicalRoot::new(first.path()).unwrap();
    let second_root = CanonicalRoot::new(second.path()).unwrap();
    let actor = spawn_project_actor_for_root(2, &first_root);

    let state = actor
        .add_workspace_root(second_root.as_path().to_path_buf())
        .await
        .unwrap();

    assert_eq!(state.workspace_roots().len(), 2);
    assert!(
        state
            .workspace_roots()
            .contains(&first_root.as_path().to_path_buf())
    );
    assert!(
        state
            .workspace_roots()
            .contains(&second_root.as_path().to_path_buf())
    );
    let restarted = actor.restart().await.unwrap();
    assert_eq!(restarted.workspace_roots(), state.workspace_roots());
}

#[tokio::test]
async fn structural_only_actor_reevaluates_lsp_when_linked_root_is_added() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    fs::write(second.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    let mut config = crate::config::LspServerConfig::rust_analyzer();
    config.command = "/definitely/missing/mcpls-language-server".to_string();
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![first.path().to_path_buf()]);
    translator.set_lsp_configs(vec![config], Some(1));
    let actor = spawn_project_actor_with_translator(2, translator);

    let initial = actor.activate(first.path().to_path_buf()).await.unwrap();
    let added = actor.add_workspace_root(second.path().to_path_buf()).await;

    assert_eq!(initial.status(), ProjectStatus::Degraded);
    assert!(matches!(added, Err(ProjectActorError::Operation(_))));
}

#[tokio::test]
async fn project_actor_owns_bounded_edit_plans() {
    let actor = spawn_project_actor(2);
    let plan = crate::edit_plan::EditPlan::new(
        "project".to_string(),
        Vec::new(),
        Vec::new(),
        true,
        std::time::Duration::from_secs(60),
    );
    let plan_id = plan.id().clone();

    actor.store_edit_plan(plan).await.unwrap();
    let taken = actor
        .take_edit_plan(plan_id.clone(), "project".to_string())
        .await
        .unwrap();
    assert_eq!(taken.project_id(), "project");
    assert!(matches!(
        actor
            .take_edit_plan(plan_id, "project".to_string())
            .await,
        Err(ProjectActorError::Operation(message)) if message.contains("not found")
    ));
}

#[tokio::test]
async fn project_runtime_refuses_to_replace_disk_with_dirty_open_document_content() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("lib.rs");
    fs::write(&source, "pub mod feature { fn disk() {} }\n").unwrap();
    let dirty = "// dirty\npub mod feature { fn open() {} }\n";
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator
        .document_tracker_mut()
        .open(source.clone(), dirty.to_string())
        .unwrap();
    let mut runtime = ProjectRuntime::new(translator);

    let artifact = runtime
        .move_inline_module_preview(
            "project",
            &source.display().to_string(),
            "feature",
            None,
            PositionEncoding::Utf8,
            root.path(),
        )
        .await
        .unwrap();
    assert_eq!(
        artifact.verification,
        Some(VerificationStatus::StructuralUnverified)
    );
    assert_eq!(artifact.producer, Some(EditProducer::StructuralAstGrep));
    assert!(!artifact.plan.safe_to_apply());
    assert!(
        artifact
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("open document differs from disk"))
    );
    let destination = root.path().join("feature.rs");
    assert!(
        artifact
            .plan
            .files()
            .iter()
            .any(|file| file.path() == &destination && file.was_created())
    );
    assert!(
        artifact
            .plan
            .files()
            .iter()
            .any(|file| file.path() == &source && file.original_content() == dirty)
    );

    let plan_id = artifact.plan.id().clone();
    let error = runtime
        .apply_edit_plan_with_context(&plan_id, "project", root.path(), None, None)
        .await
        .unwrap_err();
    assert_eq!(error, "edit plan is not safe to apply");
    assert_eq!(
        runtime
            .translator
            .document_tracker()
            .get(&source)
            .unwrap()
            .content(),
        dirty
    );
    assert_eq!(
        fs::read_to_string(source).unwrap(),
        "pub mod feature { fn disk() {} }\n"
    );
    assert!(!destination.exists());
}

#[tokio::test]
async fn dirty_documents_refuse_residency_suspension() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("lib.rs");
    fs::write(&source, "pub fn on_disk() {}\n").unwrap();

    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator
        .document_tracker_mut()
        .open(source, "pub fn unsaved() {}\n".to_string())
        .unwrap();
    let mut runtime = ProjectRuntime::new(translator);
    let (status_tx, _) = watch::channel(ProjectStatus::Ready);
    let (state_tx, _) = watch::channel(ProjectState::new(ProjectStatus::Ready, runtime.summary()));
    let (event_tx, _) = broadcast::channel(1);
    let channels = ProjectActorChannels {
        status_tx,
        state_tx,
        event_tx,
        event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
        gate: ProjectRequestGate::new(),
    };
    let mut state = ProjectState::new(ProjectStatus::Ready, runtime.summary());

    assert!(
        suspend_project_runtime(
            &channels,
            &mut state,
            &mut runtime,
            ProjectDormancy::new(ProjectDormancyReason::Restored, None),
        )
        .await
        .is_err()
    );
    assert_eq!(state.status(), ProjectStatus::Ready);
    assert_eq!(runtime.summary().open_document_count(), 1);
}

#[tokio::test]
async fn residency_suspension_records_dormancy_reason_and_idle_duration() {
    let mut runtime = ProjectRuntime::new(Translator::new());
    let (status_tx, _) = watch::channel(ProjectStatus::Ready);
    let (state_tx, _) = watch::channel(ProjectState::new(ProjectStatus::Ready, runtime.summary()));
    let (event_tx, _) = broadcast::channel(1);
    let channels = ProjectActorChannels {
        status_tx,
        state_tx,
        event_tx,
        event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
        gate: ProjectRequestGate::new(),
    };
    let mut state = ProjectState::new(ProjectStatus::Ready, runtime.summary());
    let idle_for = Duration::from_secs(60 * 60);

    suspend_project_runtime(
        &channels,
        &mut state,
        &mut runtime,
        ProjectDormancy::new(ProjectDormancyReason::ResidencyEviction, Some(idle_for)),
    )
    .await
    .unwrap();

    let Some(dormancy) = state.dormancy() else {
        panic!("residency suspension should report dormancy");
    };
    assert_eq!(dormancy.reason(), ProjectDormancyReason::ResidencyEviction);
    assert_eq!(dormancy.idle_for(), Some(idle_for));
}

#[tokio::test]
async fn path_rename_composition_uses_authoritative_open_document_content() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("old.rs");
    let destination = root.path().join("renamed.rs");
    let reference = root.path().join("reference.rs");
    fs::write(&source, "pub fn old() {}\n").unwrap();
    fs::write(&reference, "old_name();\n").unwrap();
    let dirty = "old_name(); // dirty\n";

    let reference_uri = path_to_uri(&reference).unwrap().to_string();
    let edit = serde_json::from_value(serde_json::json!({
        "changes": {
            (reference_uri): [{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 8}
                },
                "newText": "new_name"
            }]
        }
    }))
    .unwrap();
    let (edit, providers, semantic_edit_count) = compose_path_rename_edit(
        WillRenameFilesResult {
            providers: vec!["rust".to_string()],
            edits: vec![edit],
        },
        &source,
        &destination,
    )
    .unwrap();

    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator
        .document_tracker_mut()
        .open(reference.clone(), dirty.to_string())
        .unwrap();
    let mut runtime = ProjectRuntime::new(translator);
    let artifact = runtime
        .preview_edit("project", edit, PositionEncoding::Utf8, root.path())
        .await
        .unwrap();

    assert_eq!(providers, ["rust"]);
    assert_eq!(semantic_edit_count, 1);
    let snapshot = artifact
        .plan
        .files()
        .iter()
        .find(|snapshot| snapshot.path() == &reference)
        .unwrap();
    assert_eq!(
        snapshot.source(),
        crate::edit_plan::SnapshotSource::OpenDocument
    );
    assert_eq!(snapshot.original_content(), dirty);
    assert_eq!(snapshot.planned_content(), "new_name(); // dirty\n");
    assert_eq!(
        artifact
            .plan
            .operations()
            .iter()
            .filter(|operation| operation.starts_with("rename "))
            .count(),
        1
    );
}

#[tokio::test]
async fn preview_edit_refreshes_clean_tracked_document_after_external_rewrite() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("source.rs");
    fs::write(&file, "before\n").unwrap();
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let tracker = translator.document_tracker();
    tracker.open(file.clone(), "before\n".to_owned()).unwrap();
    tracker.reconciled_snapshot(&file).await.unwrap();

    fs::write(&file, "after!\n").unwrap();
    let edit = serde_json::from_value(serde_json::json!({
        "changes": {
            path_to_uri(&file).unwrap().to_string(): [{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 5}
                },
                "newText": "fresh"
            }]
        }
    }))
    .unwrap();

    let mut runtime = ProjectRuntime::new(translator);
    let artifact = runtime
        .preview_edit("project", edit, PositionEncoding::Utf8, root.path())
        .await
        .unwrap();

    assert!(artifact.plan.safe_to_apply(), "{:?}", artifact.conflicts);
    assert_eq!(artifact.plan.files()[0].original_content(), "after!\n");
    assert_eq!(artifact.plan.files()[0].planned_content(), "fresh!\n");
}

#[tokio::test]
async fn preview_edit_refreshes_after_mcpls_apply_and_external_formatter() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("source.rs");
    fs::write(&file, "before\n").unwrap();
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    let tracker = translator.document_tracker();
    tracker.open(file.clone(), "before\n".to_owned()).unwrap();
    tracker.reconciled_snapshot(&file).await.unwrap();

    let first_edit = serde_json::from_value(serde_json::json!({
        "changes": {
            path_to_uri(&file).unwrap().to_string(): [{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 6}
                },
                "newText": "changed"
            }]
        }
    }))
    .unwrap();
    let mut runtime = ProjectRuntime::new(translator);
    let first_artifact = runtime
        .preview_edit("project", first_edit, PositionEncoding::Utf8, root.path())
        .await
        .unwrap();
    runtime
        .apply_edit_plan_with_context(first_artifact.plan.id(), "project", root.path(), None, None)
        .await
        .unwrap();

    // Model cargo fmt (or another external formatter) rewriting the clean
    // file after MCPLS has successfully applied its own edit.
    fs::write(&file, "changed();\n").unwrap();
    let second_edit = serde_json::from_value(serde_json::json!({
        "changes": {
            path_to_uri(&file).unwrap().to_string(): [{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 7}
                },
                "newText": "formatted"
            }]
        }
    }))
    .unwrap();
    let second_artifact = runtime
        .preview_edit("project", second_edit, PositionEncoding::Utf8, root.path())
        .await
        .unwrap();

    assert!(
        second_artifact.plan.safe_to_apply(),
        "{:?}",
        second_artifact.conflicts
    );
    assert_eq!(
        second_artifact.plan.files()[0].original_content(),
        "changed();\n"
    );
    assert_eq!(
        second_artifact.plan.files()[0].planned_content(),
        "formatted();\n"
    );
}

#[tokio::test]
async fn preview_edit_preserves_dirty_document_on_external_rewrite() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("source.rs");
    fs::write(&file, "external\n").unwrap();
    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![root.path().to_path_buf()]);
    translator
        .document_tracker_mut()
        .open(file.clone(), "local\n".to_owned())
        .unwrap();

    fs::write(&file, "external rewrite\n").unwrap();
    let edit = serde_json::from_value(serde_json::json!({
        "changes": {
            path_to_uri(&file).unwrap().to_string(): [{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 5}
                },
                "newText": "fresh"
            }]
        }
    }))
    .unwrap();

    let mut runtime = ProjectRuntime::new(translator);
    let artifact = runtime
        .preview_edit("project", edit, PositionEncoding::Utf8, root.path())
        .await
        .unwrap();

    assert!(!artifact.plan.safe_to_apply());
    assert!(
        artifact
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("open document differs from disk"))
    );
    assert_eq!(
        runtime
            .translator
            .document_tracker()
            .get(&file)
            .unwrap()
            .content(),
        "local\n"
    );
    assert_eq!(fs::read_to_string(file).unwrap(), "external rewrite\n");
}

#[test]
fn identifies_rust_analyzer_assists_by_stable_data_id() {
    let matching = lsp_types::CodeAction {
        title: "localized or changed title".to_string(),
        data: Some(serde_json::json!({
            "id": "move_module_to_file:RefactorExtract:2:"
        })),
        ..lsp_types::CodeAction::default()
    };
    let similarly_named = lsp_types::CodeAction {
        title: "Extract module to file".to_string(),
        data: Some(serde_json::json!({
            "id": "move_module_to_file_elsewhere:RefactorExtract:2:"
        })),
        ..lsp_types::CodeAction::default()
    };

    assert!(code_action_has_assist_id(&matching, "move_module_to_file"));
    assert!(!code_action_has_assist_id(
        &similarly_named,
        "move_module_to_file"
    ));
    assert!(
        take_code_action_by_assist_id(
            vec![lsp_types::CodeActionOrCommand::CodeAction(similarly_named)],
            "move_module_to_file",
        )
        .is_none()
    );
    assert_eq!(
        take_code_action_by_assist_id(
            vec![lsp_types::CodeActionOrCommand::CodeAction(matching)],
            "move_module_to_file",
        )
        .map(|action| action.title),
        Some("localized or changed title".to_string())
    );
}

#[tokio::test]
async fn project_runtime_applies_configured_audit_and_backup_policies() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("configured.rs");
    fs::write(&file, "before\n").unwrap();
    let safety = EditSafetyConfig {
        audit_log: Some(crate::config::AuditLogConfig {
            path: PathBuf::from(".mcpls/audit.jsonl"),
            max_bytes: 4_096,
            failure_mode: crate::edit_plan::AuditFailureMode::FailClosed,
        }),
        backup: Some(crate::config::BackupConfig {
            root: PathBuf::from(".mcpls/backups"),
            max_archives: 2,
            max_bytes: 16_384,
            failure_mode: crate::edit_backup::BackupFailureMode::FailClosed,
        }),
    };
    let mut runtime = ProjectRuntime::with_edit_safety(Translator::new(), Some(safety));
    let boundary = WorkspaceBoundary::new(root.path()).unwrap();
    let configured_backup = runtime.configure_edit_safety(&boundary).unwrap().unwrap();
    assert_eq!(
        configured_backup.root(),
        root.path().join(".mcpls/backups").as_path()
    );
    let plan = EditPlan::new(
        "project".to_string(),
        vec![crate::edit_plan::FileSnapshot::from_contents(
            file.clone(),
            crate::edit_plan::SnapshotSource::Disk,
            None,
            "before\n",
            "after\n",
        )],
        Vec::new(),
        true,
        Duration::from_secs(60),
    );
    let plan_id = plan.id().clone();
    runtime.store_edit_plan(plan).unwrap();

    runtime
        .apply_edit_plan_with_context(
            &plan_id,
            "project",
            root.path(),
            Some("session-1".to_string()),
            Some("principal-1".to_string()),
        )
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(&file).unwrap(), "after\n");
    let audit = runtime.edit_plans.audit_records().next().unwrap();
    assert_eq!(audit.session_id(), Some("session-1"));
    assert_eq!(audit.principal(), Some("principal-1"));
    let audit_path = root.path().join(".mcpls/audit.jsonl");
    assert!(
        fs::read_to_string(audit_path)
            .unwrap()
            .contains("Committed")
    );
    assert!(
        root.path()
            .join(".mcpls/backups")
            .join(plan_id.as_str())
            .join("manifest.json")
            .is_file()
    );
}

#[tokio::test]
async fn committed_edit_plan_retry_returns_the_original_receipt() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("retry.rs");
    fs::write(&file, "before\n").unwrap();
    let mut runtime = ProjectRuntime::new(Translator::new());
    let plan = EditPlan::new(
        "project".to_string(),
        vec![crate::edit_plan::FileSnapshot::from_contents(
            file.clone(),
            crate::edit_plan::SnapshotSource::Disk,
            None,
            "before\n",
            "after\n",
        )],
        vec!["replace retry fixture".to_string()],
        true,
        Duration::from_secs(60),
    );
    let plan_id = plan.id().clone();
    runtime.store_edit_plan(plan).unwrap();

    let first = runtime
        .apply_edit_plan_with_context(&plan_id, "project", root.path(), None, None)
        .await
        .unwrap();
    let retry = runtime
        .apply_edit_plan_with_context(&plan_id, "project", root.path(), None, None)
        .await
        .unwrap();

    assert_eq!(retry, first);
    assert_eq!(fs::read_to_string(file).unwrap(), "after\n");
    assert_eq!(runtime.edit_plans.audit_records().count(), 1);
}

#[tokio::test]
async fn registry_overlaps_disjoint_same_project_commits() {
    let root = TempDir::new().unwrap();
    let first_path = root.path().join("first.rs");
    let second_path = root.path().join("second.rs");
    fs::write(&first_path, "before first\n").unwrap();
    fs::write(&second_path, "before second\n").unwrap();

    let registry = ProjectRegistry::new(4);
    let project_id = ProjectId::new("project").unwrap();
    let actor = registry
        .add(ProjectIdentity::new(
            project_id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();
    let first = EditPlan::new(
        project_id.to_string(),
        vec![crate::edit_plan::FileSnapshot::from_contents(
            first_path.clone(),
            crate::edit_plan::SnapshotSource::Disk,
            None,
            "before first\n",
            "after first\n",
        )],
        vec!["replace first".to_owned()],
        true,
        Duration::from_secs(60),
    )
    .with_workspace_root(root.path().to_path_buf());
    let second = EditPlan::new(
        project_id.to_string(),
        vec![crate::edit_plan::FileSnapshot::from_contents(
            second_path.clone(),
            crate::edit_plan::SnapshotSource::Disk,
            None,
            "before second\n",
            "after second\n",
        )],
        vec!["replace second".to_owned()],
        true,
        Duration::from_secs(60),
    )
    .with_workspace_root(root.path().to_path_buf());
    let first_id = first.id().clone();
    let second_id = second.id().clone();
    actor.store_edit_plan(first).await.unwrap();
    actor.store_edit_plan(second).await.unwrap();

    let _barrier =
        crate::edit_apply::install_test_apply_barrier([first_id.clone(), second_id.clone()], 2);
    let (first_result, second_result) = tokio::join!(
        registry.apply_edit_plan_with_context(
            &project_id,
            first_id,
            Some("first-session".to_owned()),
            None,
        ),
        registry.apply_edit_plan_with_context(
            &project_id,
            second_id,
            Some("second-session".to_owned()),
            None,
        ),
    );
    assert!(matches!(
        first_result.unwrap(),
        ApplyEditPlanOutcome::Applied(_)
    ));
    assert!(matches!(
        second_result.unwrap(),
        ApplyEditPlanOutcome::Applied(_)
    ));
    assert_eq!(fs::read_to_string(first_path).unwrap(), "after first\n");
    assert_eq!(fs::read_to_string(second_path).unwrap(), "after second\n");
}

#[tokio::test]
async fn registry_overlaps_linked_worktree_commits() {
    let (_repository, _worktrees, roots) = compatible_worktree_fixture();
    let registry = ProjectRegistry::new(4);
    let project_id = ProjectId::new("project").unwrap();
    add_compatible_roots(&registry, &project_id, &roots).await;
    let actor = registry.actor_for_project(&project_id).await.unwrap();
    let first_path = roots[0].join("src.rs");
    let second_path = roots[1].join("src.rs");
    fs::write(&first_path, "before first\n").unwrap();
    fs::write(&second_path, "before second\n").unwrap();
    let first = EditPlan::new(
        project_id.to_string(),
        vec![crate::edit_plan::FileSnapshot::from_contents(
            first_path.clone(),
            crate::edit_plan::SnapshotSource::Disk,
            None,
            "before first\n",
            "after first\n",
        )],
        vec!["replace first".to_owned()],
        true,
        Duration::from_secs(60),
    )
    .with_workspace_root(roots[0].clone());
    let second = EditPlan::new(
        project_id.to_string(),
        vec![crate::edit_plan::FileSnapshot::from_contents(
            second_path.clone(),
            crate::edit_plan::SnapshotSource::Disk,
            None,
            "before second\n",
            "after second\n",
        )],
        vec!["replace second".to_owned()],
        true,
        Duration::from_secs(60),
    )
    .with_workspace_root(roots[1].clone());
    let first_id = first.id().clone();
    let second_id = second.id().clone();
    actor.store_edit_plan(first).await.unwrap();
    actor.store_edit_plan(second).await.unwrap();

    let _barrier =
        crate::edit_apply::install_test_apply_barrier([first_id.clone(), second_id.clone()], 2);
    let (first_result, second_result) = tokio::join!(
        registry.apply_edit_plan_with_context(
            &project_id,
            first_id,
            Some("first-session".to_owned()),
            None,
        ),
        registry.apply_edit_plan_with_context(
            &project_id,
            second_id,
            Some("second-session".to_owned()),
            None,
        ),
    );
    assert!(matches!(
        first_result.unwrap(),
        ApplyEditPlanOutcome::Applied(_)
    ));
    assert!(matches!(
        second_result.unwrap(),
        ApplyEditPlanOutcome::Applied(_)
    ));
    assert_eq!(fs::read_to_string(first_path).unwrap(), "after first\n");
    assert_eq!(fs::read_to_string(second_path).unwrap(), "after second\n");
}

#[tokio::test]
async fn registry_reports_busy_without_consuming_a_plan() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("busy.rs");
    fs::write(&file, "before\n").unwrap();
    let registry = ProjectRegistry::new(2);
    let project_id = ProjectId::new("project").unwrap();
    let actor = registry
        .add(ProjectIdentity::new(
            project_id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();
    let plan = EditPlan::new(
        project_id.to_string(),
        vec![crate::edit_plan::FileSnapshot::from_contents(
            file.clone(),
            crate::edit_plan::SnapshotSource::Disk,
            None,
            "before\n",
            "after\n",
        )],
        vec!["replace busy".to_owned()],
        true,
        Duration::from_secs(60),
    )
    .with_workspace_root(root.path().to_path_buf());
    let plan_id = plan.id().clone();
    actor.store_edit_plan(plan).await.unwrap();
    let blocker = registry
        .edit_coordinator
        .try_acquire(
            "blocker",
            [crate::edit_coordinator::EditResource::exact(file)],
        )
        .unwrap();

    let busy = registry
        .apply_edit_plan_with_wait(&project_id, plan_id.clone(), None, None, Duration::ZERO)
        .await
        .unwrap();
    assert!(matches!(busy, ApplyEditPlanOutcome::NotReady(_)));
    assert!(
        actor
            .inspect_edit_plan(plan_id.clone(), project_id.to_string())
            .await
            .is_ok()
    );

    drop(blocker);
    assert!(matches!(
        registry
            .apply_edit_plan_with_context(&project_id, plan_id, None, None)
            .await
            .unwrap(),
        ApplyEditPlanOutcome::Applied(_)
    ));
}

#[tokio::test]
async fn registry_competing_same_file_is_retryable_then_conflicts() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("src.rs");
    fs::write(&file, "before\n").unwrap();
    let registry = ProjectRegistry::new(2);
    let project_id = ProjectId::new("project").unwrap();
    let actor = registry
        .add(ProjectIdentity::new(
            project_id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();
    let plan = |after| {
        EditPlan::new(
            project_id.to_string(),
            vec![crate::edit_plan::FileSnapshot::from_contents(
                file.clone(),
                crate::edit_plan::SnapshotSource::Disk,
                None,
                "before\n",
                after,
            )],
            vec!["replace src.rs".to_owned()],
            true,
            Duration::from_secs(60),
        )
        .with_workspace_root(root.path().to_path_buf())
    };
    let first = plan("first\n");
    let second = plan("second\n");
    let first_id = first.id().clone();
    let second_id = second.id().clone();
    actor.store_edit_plan(first).await.unwrap();
    actor.store_edit_plan(second).await.unwrap();

    let lease = registry
        .edit_coordinator
        .try_acquire(
            "first-session",
            [crate::edit_coordinator::EditResource::exact(file.clone())],
        )
        .unwrap();
    let busy = registry
        .apply_edit_plan_with_wait(
            &project_id,
            second_id.clone(),
            Some("second-session".to_owned()),
            None,
            Duration::ZERO,
        )
        .await
        .unwrap();
    assert!(matches!(busy, ApplyEditPlanOutcome::NotReady(_)));
    assert!(
        actor
            .inspect_edit_plan(second_id.clone(), project_id.to_string())
            .await
            .is_ok()
    );

    drop(lease);
    assert!(matches!(
        registry
            .apply_edit_plan_with_context(
                &project_id,
                first_id,
                Some("first-session".to_owned()),
                None,
            )
            .await
            .unwrap(),
        ApplyEditPlanOutcome::Applied(_)
    ));
    assert!(matches!(
        registry
            .apply_edit_plan_with_context(
                &project_id,
                second_id,
                Some("second-session".to_owned()),
                None,
            )
            .await
            .unwrap(),
        ApplyEditPlanOutcome::Conflict(_)
    ));
    assert_eq!(fs::read_to_string(file).unwrap(), "first\n");
}

#[tokio::test]
async fn registry_keeps_one_logical_project_for_linked_git_worktrees() {
    let repository = TempDir::new().unwrap();
    let git_dir = repository.path().join(".git");
    let worktree_git_dir = git_dir.join("worktrees").join("linked");
    fs::create_dir_all(&worktree_git_dir).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();
    fs::create_dir(git_dir.join("objects")).unwrap();
    fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

    let worktree = TempDir::new().unwrap();
    fs::write(
        worktree.path().join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .unwrap();
    write_compatible_roots_with_changed_manifests(&[repository.path(), worktree.path()]);
    let main_repository = GitRepositoryIdentity::discover(repository.path())
        .unwrap()
        .unwrap();
    let linked_repository = GitRepositoryIdentity::discover(worktree.path())
        .unwrap()
        .unwrap();
    let registry = ProjectRegistry::new(2);
    let main_id = ProjectId::new("main").unwrap();

    let main_actor = registry
        .add(
            ProjectIdentity::new(
                main_id.clone(),
                CanonicalRoot::new(repository.path()).unwrap(),
            )
            .with_repository_identity(main_repository),
        )
        .await
        .unwrap();
    let linked_actor = registry
        .add(
            ProjectIdentity::new(
                main_id.clone(),
                CanonicalRoot::new(worktree.path()).unwrap(),
            )
            .with_repository_identity(linked_repository),
        )
        .await
        .unwrap();

    let main_state = main_actor.query().await.unwrap();
    let linked_state = linked_actor.query().await.unwrap();
    assert_eq!(main_state.workspace_roots(), linked_state.workspace_roots());
    assert_eq!(main_state.workspace_roots().len(), 2);
    assert_eq!(registry.list().await.len(), 1);

    registry.remove(main_id).await.unwrap();
    assert_eq!(*linked_actor.status().borrow(), ProjectStatus::Stopped);
}

#[tokio::test]
async fn registry_keeps_linked_worktrees_with_different_toolchains_isolated() {
    let repository = TempDir::new().unwrap();
    let git_dir = repository.path().join(".git");
    let worktree_git_dir = git_dir.join("worktrees").join("linked");
    fs::create_dir_all(&worktree_git_dir).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();
    fs::create_dir(git_dir.join("objects")).unwrap();
    fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

    let worktree = TempDir::new().unwrap();
    fs::write(
        worktree.path().join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .unwrap();
    fs::write(
        repository.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    fs::write(
        worktree.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly\"\n",
    )
    .unwrap();
    for root in [repository.path(), worktree.path()] {
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
    }
    let main_repository = GitRepositoryIdentity::discover(repository.path())
        .unwrap()
        .unwrap();
    let linked_repository = GitRepositoryIdentity::discover(worktree.path())
        .unwrap()
        .unwrap();
    let registry = ProjectRegistry::new(2);

    let main_actor = registry
        .add(
            ProjectIdentity::new(
                ProjectId::new("main").unwrap(),
                CanonicalRoot::new(repository.path()).unwrap(),
            )
            .with_repository_identity(main_repository),
        )
        .await
        .unwrap();
    let linked_actor = registry
        .add(
            ProjectIdentity::new(
                ProjectId::new("main").unwrap(),
                CanonicalRoot::new(worktree.path()).unwrap(),
            )
            .with_repository_identity(linked_repository),
        )
        .await;
    let linked_actor = linked_actor.unwrap();

    assert_eq!(main_actor.query().await.unwrap().workspace_roots().len(), 1);
    assert_eq!(
        linked_actor.query().await.unwrap().workspace_roots().len(),
        1
    );
    assert!(!main_actor.sender.same_channel(&linked_actor.sender));
    assert_eq!(registry.list().await.len(), 1);
    assert_eq!(
        registry
            .actor_group_count(&ProjectId::new("main").unwrap())
            .await
            .unwrap(),
        2
    );
    registry
        .remove(ProjectId::new("main").unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn registry_resolves_semantic_paths_to_the_longest_project_actor() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let file = nested.join("src.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    let registry = ProjectRegistry::new(2);
    let outer = ProjectId::new("outer").unwrap();
    let inner = ProjectId::new("inner").unwrap();
    registry
        .add(ProjectIdentity::new(
            outer.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();
    let inner_actor = registry
        .add(ProjectIdentity::new(
            inner,
            CanonicalRoot::new(&nested).unwrap(),
        ))
        .await
        .unwrap();

    let resolved = registry.actor_for_path(&file).await.unwrap();

    assert_eq!(
        resolved.query().await.unwrap().workspace_roots(),
        inner_actor.query().await.unwrap().workspace_roots()
    );
}

#[tokio::test]
async fn project_actor_activation_owns_lsp_failure_state() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let mut config = crate::config::LspServerConfig::rust_analyzer();
    config.command = "/definitely/missing/custom-rust-lsp".to_string();
    let mut translator = Translator::new();
    translator.set_lsp_configs(vec![config], Some(1));
    let handle = spawn_project_actor_with_translator(2, translator);

    let result = handle.activate(root.path().to_path_buf()).await;

    assert!(matches!(result, Err(ProjectActorError::Operation(_))));
    let state = handle.query().await.unwrap();
    assert_eq!(state.status(), ProjectStatus::Failed);
    assert_eq!(
        state.runtime().configured_language_ids(),
        &["rust".to_string()]
    );
    assert!(state.last_error().is_some());
}

#[tokio::test]
async fn project_registry_adds_lists_and_removes_without_duplicate_actors() {
    let root = TempDir::new().unwrap();
    let identity = ProjectIdentity::new(
        ProjectId::new("demo").unwrap(),
        CanonicalRoot::new(root.path()).unwrap(),
    );
    let registry = ProjectRegistry::new(4);

    registry.add(identity.clone()).await.unwrap();
    let duplicate = registry.add(identity).await.unwrap();
    assert_eq!(registry.list().await.len(), 1);
    let state = duplicate.query().await.unwrap();
    assert_eq!(state.status(), ProjectStatus::Starting);
    assert_eq!(state.workspace_roots().len(), 1);

    registry
        .remove(ProjectId::new("demo").unwrap())
        .await
        .unwrap();
    assert!(
        duplicate
            .event_snapshot(None, 256)
            .events()
            .iter()
            .any(|record| {
                record.event()
                    == &ProjectEvent::ProjectRemoved {
                        project_id: ProjectId::new("demo").unwrap(),
                        root: root.path().canonicalize().unwrap(),
                    }
            })
    );
    assert!(registry.list().await.is_empty());
}

#[tokio::test]
async fn project_registry_retains_bounded_history_after_removal() {
    let root = TempDir::new().unwrap();
    let project_id = ProjectId::new("removed-history").unwrap();
    let registry = ProjectRegistry::new(2);
    let actor = registry
        .add(ProjectIdentity::new(
            project_id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();

    actor
        .sender
        .send(ProjectRequest::Notification {
            generation: 0,
            server_id: ServerId::from("rust"),
            notification: LspNotification::parse(
                "window/logMessage",
                Some(serde_json::json!({"type": 1, "message": "retained log"})),
            ),
        })
        .await
        .unwrap();
    actor
        .sender
        .send(ProjectRequest::Notification {
            generation: 0,
            server_id: ServerId::from("rust"),
            notification: LspNotification::parse(
                "window/showMessage",
                Some(serde_json::json!({"type": 2, "message": "retained message"})),
            ),
        })
        .await
        .unwrap();

    assert_eq!(actor.server_logs(10, None).await.unwrap().logs.len(), 1);

    registry.remove(project_id.clone()).await.unwrap();

    let logs = registry.server_logs(&project_id, 10, None).await.unwrap();
    assert_eq!(logs.logs[0].message, "retained log");
    let messages = registry.server_messages(&project_id, 10).await.unwrap();
    assert_eq!(messages.messages[0].message, "retained message");
}

#[tokio::test]
async fn removed_project_history_defers_oversized_notification_messages() {
    let root = TempDir::new().unwrap();
    let project_id = ProjectId::new("removed-oversized").unwrap();
    let registry = ProjectRegistry::new(2);
    let actor = registry
        .add(ProjectIdentity::new(
            project_id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();
    let log = "retained log ".repeat(500);
    let message = "retained message ".repeat(500);

    for (method, body) in [
        ("window/logMessage", &log),
        ("window/showMessage", &message),
    ] {
        actor
            .sender
            .send(ProjectRequest::Notification {
                generation: 0,
                server_id: ServerId::from("rust"),
                notification: LspNotification::parse(
                    method,
                    Some(serde_json::json!({"type": 1, "message": body})),
                ),
            })
            .await
            .unwrap();
    }
    actor.server_logs(10, None).await.unwrap();
    actor.server_messages(10).await.unwrap();
    registry.remove(project_id.clone()).await.unwrap();

    let logs = registry.server_logs(&project_id, 10, None).await.unwrap();
    let log_reference = logs.logs[0].message_resource.as_ref().unwrap();
    let log_token = log_reference
        .uri
        .strip_prefix("mcpls-deferred:///")
        .unwrap();
    assert_eq!(logs.logs[0].message, "[server message deferred]");
    assert_eq!(
        registry.read_deferred_resource(log_token).unwrap().value,
        serde_json::Value::String(log)
    );

    let messages = registry.server_messages(&project_id, 10).await.unwrap();
    let message_reference = messages.messages[0].message_resource.as_ref().unwrap();
    let message_token = message_reference
        .uri
        .strip_prefix("mcpls-deferred:///")
        .unwrap();
    assert_eq!(messages.messages[0].message, "[server message deferred]");
    assert_eq!(
        registry
            .read_deferred_resource(message_token)
            .unwrap()
            .value,
        serde_json::Value::String(message)
    );
}

#[tokio::test]
async fn oversized_server_notification_messages_are_deferred_losslessly() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let runtime =
        ProjectRuntime::with_deferred_results_scoped(Translator::new(), None, shared.clone(), None);
    let log = "log ".repeat(2_000);
    let message = "message ".repeat(2_000);
    let mut logs = vec![LogEntry {
        generation: 1,
        level: LogLevel::Error,
        message: log.clone(),
        message_resource: None,
        timestamp: chrono::Utc::now(),
    }];
    let mut messages = vec![ServerMessage {
        generation: 1,
        message_type: crate::bridge::MessageType::Warning,
        message: message.clone(),
        message_resource: None,
        timestamp: chrono::Utc::now(),
    }];
    runtime
        .defer_notification_messages(&mut logs, "diagnostic_log_message")
        .unwrap();
    let log_resource = logs[0].message_resource.as_ref().unwrap();
    assert_eq!(logs[0].message, "[server message deferred]");
    let log_token = log_resource.uri.strip_prefix("mcpls-deferred:///").unwrap();
    assert_eq!(
        shared.lock().unwrap().read_scoped(log_token, "").unwrap(),
        serde_json::Value::String(log)
    );

    runtime
        .defer_notification_messages(&mut messages, "server_message")
        .unwrap();
    let message_resource = messages[0].message_resource.as_ref().unwrap();
    assert_eq!(messages[0].message, "[server message deferred]");
    let message_token = message_resource
        .uri
        .strip_prefix("mcpls-deferred:///")
        .unwrap();
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .read_scoped(message_token, "")
            .unwrap(),
        serde_json::Value::String(message)
    );
}

#[test]
fn notification_pages_are_bounded_when_many_records_fit_inline() {
    let mut logs: Vec<_> = (0..400)
        .map(|index| LogEntry {
            generation: 1,
            level: LogLevel::Info,
            message: format!("log {index}"),
            message_resource: None,
            timestamp: chrono::Utc::now(),
        })
        .collect();
    let snapshot_identity = "snapshot".to_owned();
    let total = logs.len();
    let mut returned = 0;
    let mut remaining = total;
    let mut next_cursor = None;
    bound_notification_page(
        &mut logs,
        total,
        &snapshot_identity,
        None,
        |entries, page_returned, page_remaining, page_cursor| {
            returned = page_returned;
            remaining = page_remaining;
            next_cursor = page_cursor.clone();
            serde_json::to_vec(&serde_json::json!({
                "returned": returned,
                "remaining": remaining,
                "total": total,
                "snapshot_identity": snapshot_identity,
                "next_cursor": page_cursor,
                "logs": entries,
            }))
            .unwrap()
            .len()
        },
    )
    .unwrap();

    assert!(returned < total);
    assert_eq!(remaining, total - returned);
    assert!(next_cursor.is_some());
}

#[test]
fn oversized_macro_expansions_are_deferred_without_truncation() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let expansion = crate::bridge::translator::MacroExpansion {
        name: "macro".to_owned(),
        expansion: "λ".repeat(MAX_NOTIFICATION_RESULT_BYTES),
    };
    let complete = serde_json::to_value(&expansion).unwrap();
    let mut result = crate::bridge::SemanticDiscoveryResult {
        supported: true,
        provider: "rust_analyzer".to_owned(),
        kind: crate::bridge::SemanticDiscoveryKind::MacroExpansion,
        locations: Vec::new(),
        locations_total: 0,
        locations_returned: 0,
        remaining_locations: 0,
        locations_resource: None,
        selection_ranges: Vec::new(),
        selection_ranges_total: 0,
        selection_ranges_returned: 0,
        remaining_selection_ranges: 0,
        next_cursor: None,
        snapshot_identity: String::new(),
        selection_ranges_resource: None,
        macro_expansion: Some(expansion),
        macro_expansion_resource: None,
        runnables: Vec::new(),
        runnables_total: 0,
        runnables_returned: 0,
        remaining_runnables: 0,
        runnables_resource: None,
        truncated: false,
    };

    defer_semantic_discovery_payloads(&mut result, &shared, "project").unwrap();

    assert!(result.macro_expansion.is_none());
    let reference = result.macro_expansion_resource.as_ref().unwrap();
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .read_scoped(token, "project")
            .unwrap(),
        complete
    );
}

#[test]
fn oversized_runnable_payloads_are_deferred_with_exact_counts() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let runnables = (0..100)
        .map(|index| serde_json::json!({"label": "λ".repeat(512), "index": index}))
        .collect::<Vec<_>>();
    let complete = serde_json::Value::Array(runnables.clone());
    let mut result = crate::bridge::SemanticDiscoveryResult {
        supported: true,
        provider: "rust_analyzer".to_owned(),
        kind: crate::bridge::SemanticDiscoveryKind::Runnables,
        locations: Vec::new(),
        locations_total: 0,
        locations_returned: 0,
        remaining_locations: 0,
        locations_resource: None,
        selection_ranges: Vec::new(),
        selection_ranges_total: 0,
        selection_ranges_returned: 0,
        remaining_selection_ranges: 0,
        next_cursor: None,
        snapshot_identity: String::new(),
        selection_ranges_resource: None,
        macro_expansion: None,
        macro_expansion_resource: None,
        runnables,
        runnables_total: 100,
        runnables_returned: 100,
        remaining_runnables: 0,
        runnables_resource: None,
        truncated: false,
    };

    defer_semantic_discovery_payloads(&mut result, &shared, "project").unwrap();

    assert!(result.runnables.is_empty());
    assert_eq!(result.runnables_total, 100);
    assert_eq!(result.runnables_returned, 0);
    let reference = result.runnables_resource.as_ref().unwrap();
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .read_scoped(token, "project")
            .unwrap(),
        complete
    );
}

#[test]
fn oversized_selection_ranges_are_deferred_with_exact_counts() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let selection_ranges = (0..2_000)
        .map(|index| crate::bridge::Range {
            start: crate::bridge::Position2D {
                line: index,
                character: 1,
            },
            end: crate::bridge::Position2D {
                line: index,
                character: 2,
            },
        })
        .collect::<Vec<_>>();
    let complete = serde_json::to_value(&selection_ranges).unwrap();
    let mut result = crate::bridge::SemanticDiscoveryResult {
        supported: true,
        provider: "standard_lsp".to_owned(),
        kind: crate::bridge::SemanticDiscoveryKind::SelectionRanges,
        locations: Vec::new(),
        locations_total: 0,
        locations_returned: 0,
        remaining_locations: 0,
        locations_resource: None,
        selection_ranges,
        selection_ranges_total: 2_000,
        selection_ranges_returned: 2_000,
        remaining_selection_ranges: 0,
        next_cursor: None,
        snapshot_identity: String::new(),
        selection_ranges_resource: None,
        macro_expansion: None,
        macro_expansion_resource: None,
        runnables: Vec::new(),
        runnables_total: 0,
        runnables_returned: 0,
        remaining_runnables: 0,
        runnables_resource: None,
        truncated: false,
    };

    defer_semantic_discovery_payloads(&mut result, &shared, "project").unwrap();

    assert!(result.selection_ranges.is_empty());
    assert_eq!(result.selection_ranges_total, 2_000);
    assert_eq!(result.selection_ranges_returned, 0);
    let reference = result.selection_ranges_resource.as_ref().unwrap();
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .read_scoped(token, "project")
            .unwrap(),
        complete
    );
}

#[tokio::test]
async fn project_registry_evicts_old_removed_history() {
    let registry = ProjectRegistry::new(1);
    let roots = (0..=RETAINED_PROJECT_HISTORY_CAPACITY)
        .map(|_| TempDir::new().unwrap())
        .collect::<Vec<_>>();

    for (index, root) in roots.iter().enumerate() {
        let id = ProjectId::new(format!("removed-{index}")).unwrap();
        registry
            .add(ProjectIdentity::new(
                id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        registry.remove(id).await.unwrap();
    }

    let oldest = ProjectId::new("removed-0").unwrap();
    assert!(matches!(
        registry.server_logs(&oldest, 10, None).await,
        Err(ProjectRegistryError::ProjectNotFound(_))
    ));
    let newest = ProjectId::new(format!("removed-{RETAINED_PROJECT_HISTORY_CAPACITY}")).unwrap();
    assert!(registry.server_logs(&newest, 10, None).await.is_ok());
}

#[tokio::test]
async fn notification_history_eviction_reports_a_retention_gap() {
    let registry = ProjectRegistry::new(1);
    let target_root = TempDir::new().unwrap();
    let target = ProjectId::new("notification-retention-target").unwrap();
    let actor = registry
        .add(ProjectIdentity::new(
            target.clone(),
            CanonicalRoot::new(target_root.path()).unwrap(),
        ))
        .await
        .unwrap();
    actor
        .notify(
            0,
            ServerId::from("rust"),
            LspNotification::parse(
                "window/logMessage",
                Some(serde_json::json!({"type": 3, "message": "retained log"})),
            ),
        )
        .await
        .unwrap();
    actor
        .notify(
            0,
            ServerId::from("rust"),
            LspNotification::parse(
                "window/showMessage",
                Some(serde_json::json!({"type": 3, "message": "retained message"})),
            ),
        )
        .await
        .unwrap();
    actor.server_logs(10, None).await.unwrap();
    actor.server_messages(10).await.unwrap();
    registry.remove(target.clone()).await.unwrap();

    for index in 0..=RETAINED_PROJECT_HISTORY_CAPACITY {
        let root = TempDir::new().unwrap();
        let id = ProjectId::new(format!("notification-retention-{index}")).unwrap();
        registry
            .add(ProjectIdentity::new(
                id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ))
            .await
            .unwrap();
        registry.remove(id).await.unwrap();
    }

    assert!(matches!(
        registry.server_logs(&target, 10, None).await,
        Err(ProjectRegistryError::ProjectNotFound(id)) if id == target
    ));
    assert!(matches!(
        registry.server_messages(&target, 10).await,
        Err(ProjectRegistryError::ProjectNotFound(id)) if id == target
    ));
}

#[tokio::test]
async fn project_registry_persists_add_and_remove_mutations() {
    let root = TempDir::new().unwrap();
    let state_path = root.path().join("state/projects.json");
    let store = ProjectRegistrationStore::new(&state_path);
    let registry = ProjectRegistry::new(2).with_persistence(store.clone());
    let identity = ProjectIdentity::new(
        ProjectId::new("persisted").unwrap(),
        CanonicalRoot::new(root.path()).unwrap(),
    );

    registry.add(identity).await.unwrap();
    assert_eq!(store.load().unwrap().projects.len(), 1);

    registry
        .remove(ProjectId::new("persisted").unwrap())
        .await
        .unwrap();
    assert!(store.load().unwrap().projects.is_empty());
}

#[tokio::test]
async fn project_registry_updates_cargo_features_without_replacing_identity() {
    let root = TempDir::new().unwrap();
    let project_id = ProjectId::new("cargo-profile").unwrap();
    let identity =
        ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(root.path()).unwrap());
    let registry = ProjectRegistry::new(2);
    let actor = registry.add(identity.clone()).await.unwrap();
    let plan = EditPlan::new(
        project_id.to_string(),
        vec![crate::edit_plan::FileSnapshot::from_contents(
            root.path().join("lib.rs"),
            crate::edit_plan::SnapshotSource::Disk,
            None,
            "fn stale_target() {}\n",
            "fn fresh_target() {}\n",
        )],
        vec!["replace stale target".to_owned()],
        true,
        Duration::from_secs(60),
    );
    let plan_id = plan.id().clone();
    actor.store_edit_plan(plan).await.unwrap();

    let profile = crate::config::CargoFeatureProfile {
        features: vec!["serde".to_owned(), "alloc".to_owned(), "serde".to_owned()],
        all_features: false,
        no_default_features: true,
    };
    registry
        .update_cargo_features(&project_id, profile.clone())
        .await
        .unwrap();

    assert_eq!(registry.identity(&project_id).await.unwrap(), identity);
    assert_eq!(
        registry.cargo_features(&project_id).await.unwrap(),
        Some(profile.normalized())
    );
    let replacement = registry.actor_for_project(&project_id).await.unwrap();
    assert!(
        replacement
            .inspect_edit_plan(plan_id, project_id.to_string())
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cargo_feature_update_rolls_back_after_failed_activation() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    let lsp = root.path().join("profile-failure-lsp.py");
    fs::write(&lsp, PROFILE_FAILURE_LSP).unwrap();
    let mut permissions = fs::metadata(&lsp).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&lsp, permissions).unwrap();

    let mut server = crate::config::LspServerConfig::rust_analyzer();
    server.command = lsp.display().to_string();
    server.heuristics = None;
    let old_profile = crate::config::CargoFeatureProfile {
        features: vec!["good-feature".to_owned()],
        all_features: false,
        no_default_features: false,
    };
    let registry = ProjectRegistry::new(4);
    let project_id = ProjectId::new("rollback-profile").unwrap();
    let actor = registry
        .add_with_config(
            ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(root.path()).unwrap()),
            Some(ProjectConfig {
                lsp_servers: Some(vec![server]),
                heuristics_max_depth: Some(3),
                redaction_patterns: None,
                persist_environment: false,
                edit_safety: None,
                cargo_features: Some(old_profile.clone()),
            }),
        )
        .await
        .unwrap();
    let state = registry.activate(&project_id).await.unwrap();
    assert!(matches!(
        state.status(),
        ProjectStatus::Starting | ProjectStatus::Ready
    ));
    let state = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let state = actor.query().await.unwrap();
            if state.status() == ProjectStatus::Ready {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(state.status(), ProjectStatus::Ready);

    let result = registry
        .update_cargo_features(
            &project_id,
            crate::config::CargoFeatureProfile {
                features: vec!["bad-feature".to_owned()],
                all_features: false,
                no_default_features: false,
            },
        )
        .await;

    assert!(matches!(
        result,
        Err(ProjectRegistryError::Actor(ProjectActorError::Operation(_)))
    ));
    assert_eq!(
        registry.cargo_features(&project_id).await.unwrap(),
        Some(old_profile)
    );
    let current = registry.actor_for_project(&project_id).await.unwrap();
    assert!(current.sender.same_channel(&actor.sender));
    assert_eq!(
        current.query().await.unwrap().status(),
        ProjectStatus::Ready
    );
}

#[tokio::test]
async fn project_registry_restores_persisted_cargo_feature_profile() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let store = ProjectRegistrationStore::new(root.path().join("state/projects.json"));
    let profile = crate::config::CargoFeatureProfile {
        features: vec!["serde".to_owned(), "alloc".to_owned()],
        all_features: false,
        no_default_features: true,
    };
    let registry = ProjectRegistry::new(2).with_persistence(store.clone());
    registry
        .add_with_config(
            ProjectIdentity::new(
                ProjectId::new("persisted-profile").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ),
            Some(ProjectConfig {
                cargo_features: Some(profile.clone()),
                ..ProjectConfig::default()
            }),
        )
        .await
        .unwrap();

    let restored = ProjectRegistry::new(2).with_persistence(store);
    assert_eq!(restored.restore_from_persistence().await.unwrap(), 1);
    assert_eq!(
        restored
            .cargo_features(&ProjectId::new("persisted-profile").unwrap())
            .await
            .unwrap(),
        Some(profile.normalized())
    );
}

#[tokio::test]
async fn cargo_feature_update_invalidates_only_that_projects_deferred_resources() {
    let first_root = TempDir::new().unwrap();
    let second_root = TempDir::new().unwrap();
    let first_id = ProjectId::new("first-profile").unwrap();
    let second_id = ProjectId::new("second-profile").unwrap();
    let registry = ProjectRegistry::new(2);
    registry
        .add(ProjectIdentity::new(
            first_id.clone(),
            CanonicalRoot::new(first_root.path()).unwrap(),
        ))
        .await
        .unwrap();
    registry
        .add(ProjectIdentity::new(
            second_id.clone(),
            CanonicalRoot::new(second_root.path()).unwrap(),
        ))
        .await
        .unwrap();
    let first_reference = registry.deferred_results.lock().unwrap().insert_scoped(
        serde_json::json!("first"),
        "first".to_owned(),
        first_id.as_str(),
    );
    let second_reference = registry.deferred_results.lock().unwrap().insert_scoped(
        serde_json::json!("second"),
        "second".to_owned(),
        second_id.as_str(),
    );
    let first_token = first_reference
        .uri
        .strip_prefix("mcpls-deferred:///")
        .unwrap();
    let second_token = second_reference
        .uri
        .strip_prefix("mcpls-deferred:///")
        .unwrap();

    registry
        .update_cargo_features(
            &first_id,
            crate::config::CargoFeatureProfile {
                features: vec!["serde".to_owned()],
                all_features: false,
                no_default_features: false,
            },
        )
        .await
        .unwrap();

    assert!(registry.read_deferred_resource(first_token).is_err());
    assert_eq!(
        registry.read_deferred_resource(second_token).unwrap().value,
        serde_json::json!("second")
    );
}

#[tokio::test]
async fn project_registry_keeps_failed_removal_registered_and_persisted() {
    let root = TempDir::new().unwrap();
    let state_path = root.path().join("state/projects.json");
    let store = ProjectRegistrationStore::new(&state_path);
    let registry = ProjectRegistry::new(2).with_persistence(store.clone());
    let project_id = ProjectId::new("failed-removal").unwrap();
    let identity =
        ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(root.path()).unwrap());
    let (sender, receiver) = mpsc::channel(1);
    drop(receiver);
    let (_, status) = watch::channel(ProjectStatus::Starting);
    let (_, state) = watch::channel(ProjectState::new(
        ProjectStatus::Starting,
        ProjectRuntimeSummary::default(),
    ));
    let (events, _) = broadcast::channel(1);
    registry.projects.write().await.insert(
        project_id.clone(),
        ProjectEntry {
            identity: identity.clone(),
            actors: vec![ProjectActorEntry {
                actor: ProjectHandle {
                    sender: ProjectRequestSender::new(sender),
                    status,
                    state,
                    events,
                    event_history: std::sync::Arc::new(std::sync::Mutex::new(
                        ProjectEventHistory::new(1),
                    )),
                },
                mutation: std::sync::Arc::new(Mutex::new(())),
                compatibility: ProjectCompatibility::Resolved(None),
                translator_template: None,
                roots: vec![identity.root().clone()],
            }],
            config: None,
        },
    );
    store
        .save(&[PersistedProject {
            project_id: project_id.to_string(),
            root: identity.root().as_path().to_path_buf(),
            additional_roots: Vec::new(),
            config: None,
        }])
        .unwrap();

    let result = registry.remove(project_id.clone()).await;

    assert!(matches!(
        result,
        Err(ProjectRegistryError::Actor(ProjectActorError::Closed))
    ));
    assert_eq!(registry.list().await, vec![identity]);
    assert_eq!(store.load().unwrap().projects.len(), 1);
}

#[tokio::test]
async fn project_registry_reopens_work_after_failed_actor_shutdown() {
    let root = TempDir::new().unwrap();
    let registry = ProjectRegistry::new(2);
    let project_id = ProjectId::new("retry-removal").unwrap();
    let identity =
        ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(root.path()).unwrap());
    let (sender, mut receiver) = mpsc::channel::<ProjectRequest>(2);
    tokio::spawn(async move {
        while let Some(request) = receiver.recv().await {
            let (request, _) = request.into_timed();
            match request {
                ProjectRequest::PublishEvent { reply, .. }
                | ProjectRequest::SetStatus { reply, .. } => {
                    let _ = reply.send(());
                }
                _ => {}
            }
        }
    });
    let (_, status) = watch::channel(ProjectStatus::Starting);
    let (_, state) = watch::channel(ProjectState::new(
        ProjectStatus::Starting,
        ProjectRuntimeSummary::default(),
    ));
    let (events, _) = broadcast::channel(1);
    let actor = ProjectHandle {
        sender: ProjectRequestSender::new(sender),
        status,
        state,
        events,
        event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
    };
    registry.projects.write().await.insert(
        project_id.clone(),
        ProjectEntry {
            identity,
            actors: vec![ProjectActorEntry {
                actor: actor.clone(),
                mutation: std::sync::Arc::new(Mutex::new(())),
                compatibility: ProjectCompatibility::Resolved(None),
                translator_template: None,
                roots: vec![CanonicalRoot::new(root.path()).unwrap()],
            }],
            config: None,
        },
    );

    assert!(matches!(
        registry.remove(project_id).await,
        Err(ProjectRegistryError::Actor(ProjectActorError::Cancelled))
    ));
    assert!(actor.set_status(ProjectStatus::Ready).await.is_ok());
}

#[tokio::test]
async fn project_registry_reports_persistence_and_shutdown_state() {
    let transient = ProjectRegistry::new(2);
    assert!(!transient.persistence_configured());
    assert!(!transient.is_shutting_down());

    let state_path = tempfile::tempdir().unwrap().path().join("projects.json");
    let persistent =
        ProjectRegistry::new(2).with_persistence(ProjectRegistrationStore::new(state_path));
    assert!(persistent.persistence_configured());
    persistent.shutdown_all().await;
    assert!(persistent.is_shutting_down());
}

#[tokio::test]
async fn project_registry_restores_existing_roots_and_prunes_missing_roots() {
    let root = TempDir::new().unwrap();
    let state_path = root.path().join("state/projects.json");
    let store = ProjectRegistrationStore::new(&state_path);
    store
        .save(&[
            PersistedProject {
                project_id: "existing".to_string(),
                root: root.path().to_path_buf(),
                additional_roots: Vec::new(),
                config: None,
            },
            PersistedProject {
                project_id: "missing".to_string(),
                root: root.path().join("gone"),
                additional_roots: Vec::new(),
                config: None,
            },
        ])
        .unwrap();
    let registry = ProjectRegistry::new(2).with_persistence(store.clone());

    assert_eq!(registry.restore_from_persistence().await.unwrap(), 1);
    assert_eq!(registry.list().await.len(), 1);
    assert_eq!(registry.list().await[0].id().as_str(), "existing");
    assert_eq!(
        registry
            .status(&ProjectId::new("existing").unwrap())
            .await
            .unwrap()
            .status(),
        ProjectStatus::Dormant
    );
    assert_eq!(store.load().unwrap().projects.len(), 1);
}

#[tokio::test]
async fn active_actor_for_path_wakes_a_dormant_restored_project() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    let store = ProjectRegistrationStore::new(root.path().join("state/projects.json"));
    store
        .save(&[PersistedProject {
            project_id: "dormant".to_owned(),
            root: root.path().to_path_buf(),
            additional_roots: Vec::new(),
            config: None,
        }])
        .unwrap();
    let registry = ProjectRegistry::new(2).with_persistence(store);
    registry.restore_from_persistence().await.unwrap();

    registry.active_actor_for_path(&file).await.unwrap();

    assert_ne!(
        registry
            .status(&ProjectId::new("dormant").unwrap())
            .await
            .unwrap()
            .status(),
        ProjectStatus::Dormant
    );
}

#[tokio::test]
async fn project_registry_defers_compatibility_for_a_restored_single_root() {
    let root = TempDir::new().unwrap();
    let store = ProjectRegistrationStore::new(root.path().join("state/projects.json"));
    store
        .save(&[PersistedProject {
            project_id: "dormant".to_string(),
            root: root.path().to_path_buf(),
            additional_roots: Vec::new(),
            config: None,
        }])
        .unwrap();
    let registry = ProjectRegistry::new(2).with_persistence(store);

    registry.restore_from_persistence().await.unwrap();

    let projects = registry.projects.read().await;
    assert_eq!(
        projects[&ProjectId::new("dormant").unwrap()]
            .primary()
            .compatibility,
        ProjectCompatibility::Deferred
    );
    drop(projects);
}

#[tokio::test]
async fn adding_a_linked_root_resolves_deferred_compatibility() {
    let repository = TempDir::new().unwrap();
    let git_dir = repository.path().join(".git");
    let worktree_git_dir = git_dir.join("worktrees/linked");
    fs::create_dir_all(&worktree_git_dir).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();
    fs::create_dir(git_dir.join("objects")).unwrap();
    fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
    let worktree = TempDir::new().unwrap();
    fs::write(
        worktree.path().join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .unwrap();
    write_compatible_roots_with_changed_manifests(&[repository.path(), worktree.path()]);
    let project_id = ProjectId::new("repository").unwrap();
    let store = ProjectRegistrationStore::new(repository.path().join("state/projects.json"));
    store
        .save(&[PersistedProject {
            project_id: project_id.to_string(),
            root: repository.path().to_path_buf(),
            additional_roots: Vec::new(),
            config: None,
        }])
        .unwrap();
    let registry = ProjectRegistry::new(2).with_persistence(store);
    registry.restore_from_persistence().await.unwrap();
    let linked_repository = GitRepositoryIdentity::discover(worktree.path())
        .unwrap()
        .unwrap();

    registry
        .add(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(worktree.path()).unwrap(),
            )
            .with_repository_identity(linked_repository),
        )
        .await
        .unwrap();

    assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 1);
}

#[tokio::test]
async fn project_registry_persists_project_configuration() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let store = ProjectRegistrationStore::new(root.path().join("state/projects.json"));
    let registry = ProjectRegistry::new(2).with_persistence(store.clone());
    let mut server = crate::config::LspServerConfig::rust_analyzer();
    server.command = "/definitely/missing/rust-analyzer".to_string();
    let config = ProjectConfig {
        lsp_servers: Some(vec![server]),
        heuristics_max_depth: Some(3),
        redaction_patterns: None,
        persist_environment: false,
        edit_safety: None,
        cargo_features: None,
    };

    registry
        .add_with_config(
            ProjectIdentity::new(
                ProjectId::new("configured").unwrap(),
                CanonicalRoot::new(root.path()).unwrap(),
            ),
            Some(config.clone()),
        )
        .await
        .unwrap();

    assert_eq!(store.load().unwrap().projects[0].config, Some(config));

    let restored = ProjectRegistry::new(2).with_persistence(store);
    assert_eq!(restored.restore_from_persistence().await.unwrap(), 1);
    let state = restored
        .actor_for_project(&ProjectId::new("configured").unwrap())
        .await
        .unwrap()
        .query()
        .await
        .unwrap();
    assert_eq!(
        state.runtime().configured_language_ids(),
        vec!["rust".to_string()]
    );
}

#[tokio::test]
async fn project_registry_activation_uses_configuration_snapshot() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let mut translator = Translator::new();
    let mut config = crate::config::LspServerConfig::rust_analyzer();
    config.command = "/definitely/missing/custom-rust-lsp".to_string();
    translator.set_lsp_configs(vec![config], Some(1));
    let registry =
        ProjectRegistry::with_translator_template(2, translator.configuration_template());
    let id = ProjectId::new("fixture").unwrap();
    registry
        .add(ProjectIdentity::new(
            id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();

    let result = registry.activate(&id).await;

    assert!(matches!(
        result,
        Err(ProjectRegistryError::Actor(ProjectActorError::Operation(_)))
    ));
    let state = registry.status(&id).await.unwrap();
    assert_eq!(state.status(), ProjectStatus::Failed);
    assert_eq!(
        state.runtime().configured_language_ids(),
        &["rust".to_string()]
    );
}

#[tokio::test]
async fn nested_cargo_project_is_governed_by_rust_residency_budget() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("crates/fixture");
    fs::create_dir_all(&nested).unwrap();
    fs::write(
        nested.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let mut translator = Translator::new();
    translator.set_lsp_configs(
        vec![crate::config::LspServerConfig::rust_analyzer()],
        Some(10),
    );
    let registry =
        ProjectRegistry::with_translator_template(2, translator.configuration_template());
    let id = ProjectId::new("nested").unwrap();
    registry
        .add(ProjectIdentity::new(
            id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();

    let projects = registry.projects.read().await;
    let governed = projects.get(&id).unwrap().actors[0]
        .actor
        .sender
        .residency
        .is_some();
    drop(projects);
    assert!(governed);
}

#[tokio::test]
async fn project_registry_keeps_independent_projects_isolated() {
    let first_root = TempDir::new().unwrap();
    let second_root = TempDir::new().unwrap();
    let first = ProjectIdentity::new(
        ProjectId::new("first").unwrap(),
        CanonicalRoot::new(first_root.path()).unwrap(),
    );
    let second = ProjectIdentity::new(
        ProjectId::new("second").unwrap(),
        CanonicalRoot::new(second_root.path()).unwrap(),
    );
    let registry = ProjectRegistry::new(2);
    registry.add(first).await.unwrap();
    registry.add(second).await.unwrap();

    registry
        .restart(&ProjectId::new("first").unwrap())
        .await
        .unwrap();
    assert_eq!(
        registry
            .status(&ProjectId::new("second").unwrap())
            .await
            .unwrap()
            .status(),
        ProjectStatus::Starting
    );
}

#[tokio::test]
async fn project_registry_isolates_failed_lsp_recovery_between_projects() {
    let first_root = TempDir::new().unwrap();
    let second_root = TempDir::new().unwrap();
    for root in [first_root.path(), second_root.path()] {
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
    }

    let mut broken_server = crate::config::LspServerConfig::rust_analyzer();
    broken_server.command = "/definitely/missing/mcpls-language-server".to_string();
    broken_server.heuristics = None;
    let registry = ProjectRegistry::new(4);
    let first_id = ProjectId::new("first").unwrap();
    let second_id = ProjectId::new("second").unwrap();
    registry
        .add_with_config(
            ProjectIdentity::new(
                first_id.clone(),
                CanonicalRoot::new(first_root.path()).unwrap(),
            ),
            Some(ProjectConfig {
                lsp_servers: Some(vec![broken_server]),
                heuristics_max_depth: Some(3),
                redaction_patterns: None,
                persist_environment: false,
                edit_safety: None,
                cargo_features: None,
            }),
        )
        .await
        .unwrap();
    registry
        .add(ProjectIdentity::new(
            second_id.clone(),
            CanonicalRoot::new(second_root.path()).unwrap(),
        ))
        .await
        .unwrap();

    let first = registry.actor_for_project(&first_id).await.unwrap();
    let second = registry.actor_for_project(&second_id).await.unwrap();
    first.set_status(ProjectStatus::Ready).await.unwrap();
    second.set_status(ProjectStatus::Ready).await.unwrap();
    let mut second_events = second.subscribe_events();
    first
        .sender
        .send(ProjectRequest::ServerExited { generation: 0 })
        .await
        .unwrap();

    let first_state = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = first.query().await.unwrap();
            if state.status() == ProjectStatus::Failed {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert_eq!(first_state.status(), ProjectStatus::Failed);
    assert_eq!(
        registry.status(&second_id).await.unwrap().status(),
        ProjectStatus::Ready
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), second_events.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn project_status_reports_failure_in_any_actor_group() {
    let primary_root = TempDir::new().unwrap();
    let secondary_root = TempDir::new().unwrap();
    let project_id = ProjectId::new("logical").unwrap();
    let primary = spawn_project_actor(2);
    let secondary = spawn_project_actor(2);
    primary.set_status(ProjectStatus::Ready).await.unwrap();
    secondary.fail("secondary toolchain failed").await.unwrap();

    let primary_root = CanonicalRoot::new(primary_root.path()).unwrap();
    let secondary_root = CanonicalRoot::new(secondary_root.path()).unwrap();
    let mut entry = ProjectEntry::new(
        ProjectIdentity::new(project_id.clone(), primary_root.clone()),
        primary,
        std::sync::Arc::new(Mutex::new(())),
        None,
        None,
        None,
    );
    entry.identity.add_root(secondary_root.clone());
    entry.actors.push(ProjectActorEntry::new(
        secondary,
        std::sync::Arc::new(Mutex::new(())),
        None,
        None,
        secondary_root,
    ));

    let registry = ProjectRegistry::new(2);
    registry
        .projects
        .write()
        .await
        .insert(project_id.clone(), entry);

    let state = registry.status(&project_id).await.unwrap();

    assert_eq!(state.status(), ProjectStatus::Failed);
    assert_eq!(state.last_error(), Some("secondary toolchain failed"));
    let counts = registry.status_counts().await;
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.ready, 0);
}

#[test]
fn project_status_aggregate_preserves_ready_for_one_actor() {
    let state = ProjectState::aggregate([ProjectState::new(
        ProjectStatus::Ready,
        ProjectRuntimeSummary::default(),
    )]);

    assert_eq!(state.status(), ProjectStatus::Ready);
}

#[test]
fn partial_activation_is_degraded() {
    assert_eq!(
        activation_status(ActivationHealth::Degraded, false),
        ProjectStatus::Degraded
    );
}

#[tokio::test]
async fn project_registry_serializes_restart_and_remove() {
    let root = TempDir::new().unwrap();
    let registry = ProjectRegistry::new(2);
    let id = ProjectId::new("race").unwrap();
    registry
        .add(ProjectIdentity::new(
            id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();

    let restart_registry = registry.clone();
    let remove_registry = registry.clone();
    let restart_id = id.clone();
    let remove_id = id.clone();
    let (restart, remove) = tokio::join!(
        restart_registry.restart(&restart_id),
        remove_registry.remove(remove_id),
    );

    assert!(restart.is_ok() || matches!(restart, Err(ProjectRegistryError::Actor(_))));
    assert!(remove.is_ok());
    assert!(registry.list().await.is_empty());
}

#[tokio::test]
async fn project_registry_remove_blocks_re_registration_until_removal_finishes() {
    let root = TempDir::new().unwrap();
    let registry = ProjectRegistry::new(2);
    let id = ProjectId::new("race").unwrap();
    let identity = ProjectIdentity::new(id.clone(), CanonicalRoot::new(root.path()).unwrap());
    registry.add(identity.clone()).await.unwrap();

    let mutation = registry.projects.read().await.get(&id).unwrap().actors[0]
        .mutation
        .clone();
    let guard = mutation.lock().await;
    let remove_registry = registry.clone();
    let remove_id = id.clone();
    let remove = tokio::spawn(async move { remove_registry.remove(remove_id).await });

    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        tokio::time::timeout(Duration::from_millis(10), registry.add(identity.clone()))
            .await
            .unwrap(),
        Err(ProjectRegistryError::ProjectRemoving(project)) if project == id
    ));

    drop(guard);
    assert!(remove.await.unwrap().is_ok());
    registry.add(identity).await.unwrap();
    assert_eq!(registry.list().await.len(), 1);
}

#[tokio::test]
async fn project_registry_removal_rejects_new_actor_requests() {
    let root = TempDir::new().unwrap();
    let registry = ProjectRegistry::new(2);
    let id = ProjectId::new("removing").unwrap();
    let identity = ProjectIdentity::new(id.clone(), CanonicalRoot::new(root.path()).unwrap());
    let actor = registry.add(identity.clone()).await.unwrap();

    let mutation = registry.projects.read().await.get(&id).unwrap().actors[0]
        .mutation
        .clone();
    let guard = mutation.lock().await;
    let remove_registry = registry.clone();
    let remove_id = id.clone();
    let remove = tokio::spawn(async move { remove_registry.remove(remove_id).await });

    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        registry.add(identity).await,
        Err(ProjectRegistryError::ProjectRemoving(project)) if project == id
    ));
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_millis(10),
            actor.set_status(ProjectStatus::Ready)
        )
        .await,
        Ok(Err(ProjectActorError::Closed))
    ));

    drop(guard);
    assert!(remove.await.unwrap().is_ok());
}

#[tokio::test]
async fn project_registry_shuts_down_all_registered_actors() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let registry = ProjectRegistry::new(2);
    let first_id = ProjectId::new("first").unwrap();
    let second_id = ProjectId::new("second").unwrap();
    let first_actor = registry
        .add(ProjectIdentity::new(
            first_id.clone(),
            CanonicalRoot::new(first.path()).unwrap(),
        ))
        .await
        .unwrap();
    let second_actor = registry
        .add(ProjectIdentity::new(
            second_id.clone(),
            CanonicalRoot::new(second.path()).unwrap(),
        ))
        .await
        .unwrap();

    let report = registry.shutdown_all().await;

    assert!(report.failed.is_empty());
    assert_eq!(report.stopped, vec![first_id, second_id]);
    assert_eq!(*first_actor.status().borrow(), ProjectStatus::Stopped);
    assert_eq!(*second_actor.status().borrow(), ProjectStatus::Stopped);
}

#[tokio::test]
async fn project_registry_shutdown_rejects_new_actor_requests_before_draining() {
    let root = TempDir::new().unwrap();
    let registry = ProjectRegistry::new(2);
    let project_id = ProjectId::new("shutdown-race").unwrap();
    registry
        .add(ProjectIdentity::new(
            project_id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();
    let actor = registry.actor_for_project(&project_id).await.unwrap();
    let mutation = registry
        .projects
        .read()
        .await
        .get(&project_id)
        .unwrap()
        .actors[0]
        .mutation
        .clone();
    let guard = mutation.lock().await;

    let shutdown_registry = registry.clone();
    let shutdown = tokio::spawn(async move { shutdown_registry.shutdown_all().await });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    assert!(matches!(
        actor.set_status(ProjectStatus::Ready).await,
        Err(ProjectActorError::Closed)
    ));

    drop(guard);
    let report = shutdown.await.unwrap();
    assert!(report.failed.is_empty());
    assert!(report.stopped.contains(&project_id));
}

#[tokio::test]
async fn project_registry_rejects_registration_after_shutdown_begins() {
    let registry = ProjectRegistry::new(2);
    registry.shutdown_all().await;

    let root = TempDir::new().unwrap();
    let result = registry
        .add(ProjectIdentity::new(
            ProjectId::new("late").unwrap(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await;

    assert!(matches!(result, Err(ProjectRegistryError::ShuttingDown)));
}

#[tokio::test]
async fn project_registry_reports_shutdown_timeout() {
    let root = TempDir::new().unwrap();
    let registry = ProjectRegistry::new(2).with_shutdown_timeout(Duration::ZERO);
    let project_id = ProjectId::new("slow").unwrap();
    let (sender, mut requests) = mpsc::channel(1);
    tokio::spawn(async move {
        if let Some(ProjectRequest::Shutdown { .. }) = requests.recv().await {
            std::future::pending::<()>().await;
        }
    });
    let (_, status) = watch::channel(ProjectStatus::Starting);
    let (_, state) = watch::channel(ProjectState::new(
        ProjectStatus::Starting,
        ProjectRuntimeSummary::default(),
    ));
    let (events, _) = broadcast::channel(1);
    let actor = ProjectHandle {
        sender: ProjectRequestSender::new(sender),
        status,
        state,
        events,
        event_history: std::sync::Arc::new(std::sync::Mutex::new(ProjectEventHistory::new(1))),
    };
    registry.projects.write().await.insert(
        project_id.clone(),
        ProjectEntry {
            identity: ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(root.path()).unwrap(),
            ),
            actors: vec![ProjectActorEntry {
                actor,
                mutation: std::sync::Arc::new(Mutex::new(())),
                compatibility: ProjectCompatibility::Resolved(None),
                translator_template: None,
                roots: vec![CanonicalRoot::new(root.path()).unwrap()],
            }],
            config: None,
        },
    );

    let report = registry.shutdown_all().await;

    assert!(report.stopped.is_empty());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].project_id, project_id);
    assert_eq!(report.failed[0].error, "shutdown timed out after 0ns");
}

#[tokio::test]
async fn project_registry_shutdown_waits_for_project_mutations() {
    let root = TempDir::new().unwrap();
    let registry = ProjectRegistry::new(2);
    let project_id = ProjectId::new("project").unwrap();
    registry
        .add(ProjectIdentity::new(
            project_id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();
    let mutation = registry
        .projects
        .read()
        .await
        .get(&project_id)
        .unwrap()
        .actors[0]
        .mutation
        .clone();
    let guard = mutation.lock().await;
    let shutdown_registry = registry.clone();
    let mut shutdown = tokio::spawn(async move { shutdown_registry.shutdown_all().await });

    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut shutdown)
            .await
            .is_err()
    );
    drop(guard);

    let report = shutdown.await.unwrap();
    assert_eq!(report.stopped, vec![project_id]);
    assert!(report.failed.is_empty());
}

#[tokio::test]
async fn project_registry_rejects_conflicting_duplicate_ids() {
    let first_root = TempDir::new().unwrap();
    let second_root = TempDir::new().unwrap();
    let registry = ProjectRegistry::new(2);
    registry
        .add(ProjectIdentity::new(
            ProjectId::new("same").unwrap(),
            CanonicalRoot::new(first_root.path()).unwrap(),
        ))
        .await
        .unwrap();

    let result = registry
        .add(ProjectIdentity::new(
            ProjectId::new("same").unwrap(),
            CanonicalRoot::new(second_root.path()).unwrap(),
        ))
        .await;

    assert!(matches!(
        result,
        Err(ProjectRegistryError::ConflictingProject { id, .. }) if id.as_str() == "same"
    ));
}

#[tokio::test]
async fn project_registry_adds_compatible_worktree_to_one_logical_project() {
    let repository = TempDir::new().unwrap();
    let git_dir = repository.path().join(".git");
    let worktree_git_dir = git_dir.join("worktrees").join("linked");
    fs::create_dir_all(&worktree_git_dir).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();
    fs::create_dir(git_dir.join("objects")).unwrap();
    fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

    let worktree = TempDir::new().unwrap();
    fs::write(
        worktree.path().join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .unwrap();
    for root in [repository.path(), worktree.path()] {
        fs::write(
            root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
    }
    let repository_identity = GitRepositoryIdentity::discover(repository.path())
        .unwrap()
        .unwrap();
    let linked_identity = GitRepositoryIdentity::discover(worktree.path())
        .unwrap()
        .unwrap();
    let project_id = ProjectId::new("repository").unwrap();
    let registry = ProjectRegistry::new(2);

    registry
        .add(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(repository.path()).unwrap(),
            )
            .with_repository_identity(repository_identity),
        )
        .await
        .unwrap();
    let wrong_id = ProjectId::new("worktree").unwrap();
    let wrong_id_result = registry
        .add(
            ProjectIdentity::new(wrong_id, CanonicalRoot::new(worktree.path()).unwrap())
                .with_repository_identity(linked_identity.clone()),
        )
        .await;
    assert!(matches!(
        wrong_id_result,
        Err(ProjectRegistryError::LinkedWorktreeProject { existing_id, .. })
            if existing_id == project_id
    ));
    let actor = registry
        .add(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(worktree.path()).unwrap(),
            )
            .with_repository_identity(linked_identity),
        )
        .await
        .unwrap();

    assert_eq!(registry.list().await.len(), 1);
    assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 1);
    assert_eq!(actor.query().await.unwrap().workspace_roots().len(), 2);
    let file = worktree.path().join("src.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    let (resolved_id, resolved_actor) = registry.project_for_path(&file).await.unwrap();
    assert_eq!(resolved_id, project_id);
    assert!(resolved_actor.sender.same_channel(&actor.sender));
}

#[tokio::test]
async fn project_registry_keeps_unknown_worktree_in_one_logical_project() {
    let repository = TempDir::new().unwrap();
    let git_dir = repository.path().join(".git");
    let worktree_git_dir = git_dir.join("worktrees").join("linked");
    fs::create_dir_all(&worktree_git_dir).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();
    fs::create_dir(git_dir.join("objects")).unwrap();
    fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

    let worktree = TempDir::new().unwrap();
    fs::write(
        worktree.path().join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .unwrap();
    for root in [repository.path(), worktree.path()] {
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
    }

    let repository_identity = GitRepositoryIdentity::discover(repository.path())
        .unwrap()
        .unwrap();
    let linked_identity = GitRepositoryIdentity::discover(worktree.path())
        .unwrap()
        .unwrap();
    let project_id = ProjectId::new("repository").unwrap();
    let registry = ProjectRegistry::new(2);

    registry
        .add(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(repository.path()).unwrap(),
            )
            .with_repository_identity(repository_identity),
        )
        .await
        .unwrap();
    let linked_actor = registry
        .add(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(worktree.path()).unwrap(),
            )
            .with_repository_identity(linked_identity),
        )
        .await
        .unwrap();

    let projects = registry.list().await;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].roots().len(), 2);
    assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 2);
    assert_eq!(
        linked_actor.query().await.unwrap().workspace_roots().len(),
        1
    );
    let file = worktree.path().join("src.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    let (resolved_id, resolved_actor) = registry.project_for_path(&file).await.unwrap();
    assert_eq!(resolved_id, project_id);
    assert!(resolved_actor.sender.same_channel(&linked_actor.sender));
    let explicit_id_actor = registry
        .active_actor_for_project_path(&project_id, &file)
        .await
        .unwrap();
    assert!(explicit_id_actor.sender.same_channel(&linked_actor.sender));
}

#[tokio::test]
async fn project_registry_applies_a_plan_from_a_non_primary_worktree_actor() {
    let repository = TempDir::new().unwrap();
    let git_dir = repository.path().join(".git");
    let worktree_git_dir = git_dir.join("worktrees").join("linked");
    fs::create_dir_all(&worktree_git_dir).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();
    fs::create_dir(git_dir.join("objects")).unwrap();
    fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

    let worktree = TempDir::new().unwrap();
    fs::write(
        worktree.path().join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .unwrap();
    for root in [repository.path(), worktree.path()] {
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
    }
    let file = worktree.path().join("src.rs");
    fs::write(&file, "before\n").unwrap();

    let project_id = ProjectId::new("repository").unwrap();
    let registry = ProjectRegistry::new(2);
    let repository_identity = GitRepositoryIdentity::discover(repository.path())
        .unwrap()
        .unwrap();
    registry
        .add(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(repository.path()).unwrap(),
            )
            .with_repository_identity(repository_identity),
        )
        .await
        .unwrap();
    let linked_identity = GitRepositoryIdentity::discover(worktree.path())
        .unwrap()
        .unwrap();
    registry
        .add(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(worktree.path()).unwrap(),
            )
            .with_repository_identity(linked_identity),
        )
        .await
        .unwrap();

    let edit = lsp_types::WorkspaceEdit {
        changes: Some(HashMap::from([(
            crate::bridge::path_to_uri(&file).unwrap(),
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position::new(0, 0),
                    end: lsp_types::Position::new(0, 6),
                },
                new_text: "after".to_owned(),
            }],
        )])),
        document_changes: None,
        change_annotations: None,
    };
    let artifact = registry
        .preview_edit(&project_id, edit, PositionEncoding::Utf8)
        .await
        .unwrap();
    let plan_id = artifact.plan.id().clone();

    let summary = registry
        .inspect_edit_plan(&project_id, plan_id.clone())
        .await
        .unwrap();
    assert_eq!(summary.affected_files, vec![file.clone()]);
    let outcome = registry
        .apply_edit_plan_with_wait(
            &project_id,
            plan_id.clone(),
            Some("separate-worktree-test".to_owned()),
            None,
            Duration::ZERO,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ApplyEditPlanOutcome::Applied(_)));
    assert_eq!(fs::read_to_string(file).unwrap(), "after\n");
    assert!(
        registry
            .actor_for_edit_plan(&project_id, plan_id)
            .await
            .is_ok(),
        "retained applied receipts must remain routable"
    );

    let created = worktree.path().join("created.rs");
    let create_edit = lsp_types::WorkspaceEdit {
        changes: None,
        document_changes: Some(lsp_types::DocumentChanges::Operations(vec![
            lsp_types::DocumentChangeOperation::Op(lsp_types::ResourceOp::Create(
                lsp_types::CreateFile {
                    uri: crate::bridge::path_to_uri(&created).unwrap(),
                    options: None,
                    annotation_id: None,
                },
            )),
        ])),
        change_annotations: None,
    };
    let create_artifact = registry
        .preview_edit(&project_id, create_edit, PositionEncoding::Utf8)
        .await
        .unwrap();
    assert!(matches!(
        create_artifact.plan.file_operations(),
        [crate::edit_paths::FileOperation::Create { path, .. }] if path == &created
    ));
    let create_plan_id = create_artifact.plan.id().clone();
    let create_outcome = registry
        .apply_edit_plan_with_wait(
            &project_id,
            create_plan_id,
            Some("separate-worktree-create-test".to_owned()),
            None,
            Duration::ZERO,
        )
        .await
        .unwrap();
    assert!(matches!(create_outcome, ApplyEditPlanOutcome::Applied(_)));
    assert!(created.is_file());
}

#[tokio::test]
async fn project_registry_previews_a_code_action_from_a_non_primary_worktree_actor() {
    let repository = TempDir::new().unwrap();
    let git_dir = repository.path().join(".git");
    let worktree_git_dir = git_dir.join("worktrees").join("linked");
    fs::create_dir_all(&worktree_git_dir).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();
    fs::create_dir(git_dir.join("objects")).unwrap();
    fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

    let worktree = TempDir::new().unwrap();
    fs::write(
        worktree.path().join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .unwrap();
    for root in [repository.path(), worktree.path()] {
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
    }
    let file = worktree.path().join("src.rs");
    fs::write(&file, "before\n").unwrap();

    let project_id = ProjectId::new("repository").unwrap();
    let registry = ProjectRegistry::new(2);
    let repository_identity = GitRepositoryIdentity::discover(repository.path())
        .unwrap()
        .unwrap();
    registry
        .add(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(repository.path()).unwrap(),
            )
            .with_repository_identity(repository_identity),
        )
        .await
        .unwrap();
    let linked_identity = GitRepositoryIdentity::discover(worktree.path())
        .unwrap()
        .unwrap();
    let linked_actor = registry
        .add(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(worktree.path()).unwrap(),
            )
            .with_repository_identity(linked_identity),
        )
        .await
        .unwrap();

    let edit = lsp_types::WorkspaceEdit {
        changes: Some(HashMap::from([(
            crate::bridge::path_to_uri(&file).unwrap(),
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position::new(0, 0),
                    end: lsp_types::Position::new(0, 6),
                },
                new_text: "after".to_owned(),
            }],
        )])),
        document_changes: None,
        change_annotations: None,
    };
    let action_id = linked_actor
        .store_code_action(StoredCodeAction {
            file_path: file.display().to_string(),
            action: lsp_types::CodeActionOrCommand::CodeAction(lsp_types::CodeAction {
                title: "replace text".to_owned(),
                edit: Some(edit),
                ..lsp_types::CodeAction::default()
            }),
            created_at: Instant::now(),
        })
        .await
        .unwrap();

    let artifact = registry
        .preview_code_action(&project_id, action_id, PositionEncoding::Utf8)
        .await
        .unwrap();
    assert_eq!(artifact.plan.files()[0].path(), file.as_path());
    assert!(artifact.plan.safe_to_apply());
    assert_eq!(fs::read_to_string(file).unwrap(), "before\n");
}

#[tokio::test]
async fn linked_worktrees_share_only_when_cargo_profiles_match() {
    let (repository, worktrees, roots) = compatible_worktree_fixture();
    let project_id = ProjectId::new("profile-linked").unwrap();
    let mut server = crate::config::LspServerConfig::rust_analyzer();
    server.command = "rust-analyzer".to_owned();
    server.heuristics = None;
    let mut template_source = Translator::new();
    template_source.set_lsp_configs(vec![server], Some(3));
    let registry =
        ProjectRegistry::with_translator_template(4, template_source.configuration_template());
    let same_profile = ProjectConfig {
        cargo_features: Some(crate::config::CargoFeatureProfile {
            features: vec!["shared".to_owned()],
            all_features: false,
            no_default_features: false,
        }),
        ..ProjectConfig::default()
    };
    let different_profile = ProjectConfig {
        cargo_features: Some(crate::config::CargoFeatureProfile {
            features: vec!["isolated".to_owned()],
            all_features: false,
            no_default_features: false,
        }),
        ..ProjectConfig::default()
    };
    let repository_identity = GitRepositoryIdentity::discover(repository.path())
        .unwrap()
        .unwrap();
    registry
        .add_with_config(
            ProjectIdentity::new(project_id.clone(), CanonicalRoot::new(&roots[0]).unwrap())
                .with_repository_identity(repository_identity),
            Some(same_profile.clone()),
        )
        .await
        .unwrap();

    let linked_identity = GitRepositoryIdentity::discover(worktrees[0].path())
        .unwrap()
        .unwrap();
    let shared = registry
        .add_with_config(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(worktrees[0].path()).unwrap(),
            )
            .with_repository_identity(linked_identity),
            Some(same_profile),
        )
        .await
        .unwrap();
    assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 1);
    assert_eq!(shared.query().await.unwrap().workspace_roots().len(), 2);

    let isolated_identity = GitRepositoryIdentity::discover(worktrees[1].path())
        .unwrap()
        .unwrap();
    let isolated = registry
        .add_with_config(
            ProjectIdentity::new(
                project_id.clone(),
                CanonicalRoot::new(worktrees[1].path()).unwrap(),
            )
            .with_repository_identity(isolated_identity),
            Some(different_profile),
        )
        .await
        .unwrap();
    assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 2);
    assert!(!shared.sender.same_channel(&isolated.sender));
}

#[cfg(unix)]
#[tokio::test]
async fn compatible_linked_worktrees_share_one_lsp_process() {
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    let (repository, _worktrees, roots) = compatible_worktree_fixture();
    let counter = repository.path().join("spawn-count");
    let lsp = repository.path().join("counting-lsp.py");
    fs::write(&lsp, DUPLICATE_ACTIVATION_LSP).unwrap();
    let mut permissions = fs::metadata(&lsp).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&lsp, permissions).unwrap();

    let mut config = crate::config::LspServerConfig::rust_analyzer();
    config.command = lsp.display().to_string();
    config.heuristics = None;
    config.env = HashMap::from([(
        "MCPLS_SPAWN_COUNTER".to_string(),
        counter.display().to_string(),
    )]);
    let mut template_source =
        Translator::new().with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
    template_source.set_lsp_configs(vec![config], Some(3));
    let registry =
        ProjectRegistry::with_translator_template(4, template_source.configuration_template());
    let project_id = ProjectId::new("repository").unwrap();
    add_compatible_roots(&registry, &project_id, &roots).await;

    let state = registry.activate(&project_id).await.unwrap();
    assert!(matches!(
        state.status(),
        ProjectStatus::Starting | ProjectStatus::Ready
    ));
    let ready = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if registry.status(&project_id).await.unwrap().status() == ProjectStatus::Ready {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        ready.is_ok(),
        "linked-worktree project did not become ready"
    );
    assert_eq!(state.workspace_roots().len(), 5);
    assert_eq!(registry.actor_group_count(&project_id).await.unwrap(), 1);
    assert_eq!(fs::read_to_string(counter).unwrap(), "1");
}

#[tokio::test]
async fn twenty_registered_worktrees_remain_four_idle_actor_groups() {
    let mut template_source =
        Translator::new().with_extensions(std::collections::HashMap::from([(
            "rs".to_string(),
            "rust".to_string(),
        )]));
    template_source.set_lsp_configs(
        vec![crate::config::LspServerConfig::rust_analyzer()],
        Some(3),
    );
    let registry =
        ProjectRegistry::with_translator_template(4, template_source.configuration_template())
            .with_rust_residency_limit(1);
    let mut fixtures = Vec::new();

    for index in 0..4 {
        let (repository, worktrees, roots) = compatible_worktree_fixture();
        let project_id = ProjectId::new(format!("repository-{index}")).unwrap();
        add_compatible_roots(&registry, &project_id, &roots).await;
        fixtures.push((repository, worktrees));
    }

    let snapshot = registry.status_snapshot().await;
    assert_eq!(registry.list().await.len(), 4);
    assert_eq!(registry.total_actor_group_count().await, 4);
    assert_eq!(snapshot.actor_groups, 4);
    assert_eq!(snapshot.counts.starting, 0);
    assert_eq!(snapshot.counts.ready + snapshot.counts.dormant, 4);
    assert_eq!(snapshot.counts.failed, 0);
    assert_eq!(snapshot.queue_pressure.queued, 0);
    assert!(
        snapshot
            .summaries
            .iter()
            .all(|summary| summary.actor_group_count == 1)
    );
    assert_eq!(
        snapshot
            .summaries
            .iter()
            .map(|summary| summary.roots.len())
            .sum::<usize>(),
        20
    );
    drop(fixtures);
}

#[cfg(unix)]
#[test]
fn resolve_path_canonicalizes_symlink_aliases() {
    use std::os::unix::fs::symlink;

    let workspace = TempDir::new().unwrap();
    let alias_parent = TempDir::new().unwrap();
    let alias = alias_parent.path().join("workspace");
    symlink(workspace.path(), &alias).unwrap();
    let file = workspace.path().join("src.rs");
    fs::write(&file, "fn main() {}").unwrap();

    let project = ProjectIdentity::new(
        ProjectId::new("workspace").unwrap(),
        CanonicalRoot::new(&alias).unwrap(),
    );
    let project_resolver = ProjectResolver::new([project]).unwrap();

    assert_eq!(
        project_resolver
            .resolve_path(alias.join("src.rs"))
            .unwrap()
            .id()
            .as_str(),
        "workspace"
    );
}

#[test]
fn oversized_hover_contents_are_deferred_without_loss() {
    let deferred_results = std::sync::Arc::new(std::sync::Mutex::new(DeferredResultStore::new()));
    let contents = "λ".repeat(MAX_INLINE_HOVER_CONTENT_BYTES);
    let mut result = HoverResult {
        provider: "test".to_owned(),
        kind: crate::bridge::translator::NavigationKind::Hover,
        contents: contents.clone(),
        contents_resource: None,
        range: None,
        source: SourceContext::Unavailable {
            reason: crate::bridge::translator::SourceUnavailableReason::NotFound,
        },
        truncated: false,
        symbol_handle: None,
    };

    defer_oversized_hover_contents(&mut result, &deferred_results, "project").unwrap();

    let reference = result.contents_resource.as_ref().unwrap();
    assert!(result.truncated);
    assert!(result.contents.len() <= MAX_INLINE_HOVER_CONTENT_BYTES + "... (truncated)".len());
    assert_eq!(
        reference.total_bytes,
        Some(
            serde_json::to_vec(&serde_json::json!({
                "contents": contents
            }))
            .unwrap()
            .len()
        )
    );
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();
    let value = deferred_results
        .lock()
        .unwrap()
        .read_scoped(token, "project")
        .unwrap();
    assert_eq!(value["contents"], contents);
    assert!(
        deferred_results
            .lock()
            .unwrap()
            .read_scoped(token, "different-project")
            .is_err()
    );
}

#[tokio::test]
async fn hover_deferred_resource_is_invalidated_with_project_snapshot() {
    let root = TempDir::new().unwrap();
    let project_id = ProjectId::new("hover-project").unwrap();
    let registry = ProjectRegistry::new(2);
    registry
        .add(ProjectIdentity::new(
            project_id.clone(),
            CanonicalRoot::new(root.path()).unwrap(),
        ))
        .await
        .unwrap();

    let reference = registry
        .store_deferred_resource(
            &project_id,
            "hover_contents",
            serde_json::json!({"contents": "stale hover"}),
        )
        .unwrap();
    let token = reference.uri.strip_prefix("mcpls-deferred:///").unwrap();

    registry
        .update_cargo_features(
            &project_id,
            crate::config::CargoFeatureProfile {
                features: vec!["serde".to_owned()],
                all_features: false,
                no_default_features: false,
            },
        )
        .await
        .unwrap();

    assert!(registry.read_deferred_resource(token).is_err());
}
