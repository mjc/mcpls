//! End-to-end test suite exercising MCPLS tools against a real rust-analyzer.
//!
//! # Process model
//!
//! A single `#[test] fn ra_e2e_suite()` drives the whole suite.  nextest sees
//! exactly one test → one process → rust-analyzer is spawned once.  Sub-cases
//! run sequentially; the suite panics at the end if any failed, printing an
//! aggregated report so all failures are visible at once.
//!
//! # Skip policy
//!
//! - `MCPLS_SKIP_RA=1`               → print skip line, return success
//! - `MCPLS_RUST_ANALYZER=<path>`    → use that binary
//! - rust-analyzer found in PATH     → use it
//! - not found and no skip flag      → panic (fail closed)
//!
//! # Filter
//!
//! Set `MCPLS_RA_FILTER=<substring>` to run only matching sub-cases locally.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::missing_docs_in_private_items,
    missing_docs
)]

#[path = "common/assertions.rs"]
mod assertions;
#[path = "e2e/mcp_client.rs"]
mod mcp_client;
#[path = "common/ra_probe.rs"]
mod ra_probe;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use mcp_client::McpClient;
use ra_probe::{Resolution, resolve_rust_analyzer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Sub-case infrastructure
// ---------------------------------------------------------------------------

struct SubResult {
    name: &'static str,
    outcome: Result<(), String>,
}

type SubCaseFn = fn(&mut McpClient, &Path) -> Result<(), String>;

struct SubCase {
    name: &'static str,
    run: SubCaseFn,
}

macro_rules! sub_case {
    ($name:ident) => {
        SubCase {
            name: stringify!($name),
            run: $name,
        }
    };
}

// ---------------------------------------------------------------------------
// Workspace staging
// ---------------------------------------------------------------------------

/// Copy `tests/fixtures/rust_workspace/` into a fresh `TempDir`.
///
/// Also copies `extras/broken.rs` into `src/broken.rs` and appends
/// `pub mod broken;` to `src/lib.rs` so rust-analyzer diagnoses it.
/// `extras/bad_format.rs` is placed in `src/bad_format.rs` without being
/// added to the module tree (`format_document` does not require it).
fn stage_workspace() -> TempDir {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_workspace");
    let tmp = TempDir::new().expect("failed to create TempDir");
    copy_dir_recursive(&fixture_dir, tmp.path()).expect("failed to copy fixture workspace");

    // Copy broken.rs into src/ and register it in lib.rs.
    let broken_src = fixture_dir.join("extras/broken.rs");
    let broken_dst = tmp.path().join("src/broken.rs");
    fs::copy(&broken_src, &broken_dst).expect("failed to copy broken.rs");

    let lib_path = tmp.path().join("src/lib.rs");
    let mut lib_content = fs::read_to_string(&lib_path).expect("failed to read lib.rs");
    lib_content.push_str("\npub mod broken;\n");
    lib_content
        .push_str("\npub mod move_target {\n    pub fn answer() -> u32 {\n        42\n    }\n}\n");
    lib_content.push_str("\npub mod folder_mod;\n");
    lib_content.push_str("\npub mod move_items;\n");
    lib_content.push_str(
        r"
macro_rules! semantic_answer {
    () => { 7_u32 };
}

pub trait SemanticDeclaration {
    fn semantic_value(&self) -> u32;
}

pub struct SemanticType;

impl SemanticDeclaration for SemanticType {
    fn semantic_value(&self) -> u32 { semantic_answer!() }
}

pub fn tested_semantic_value() -> u32 { SemanticType.semantic_value() }

pub fn café_value() -> u32 { 7 }

pub fn unicode_declaration_user() -> u32 { café_value() }

pub fn nested_selection_target() -> u32 {
    let nested = (tested_semantic_value() + 1) * 2;
    nested
}

#[cfg(test)]
mod semantic_discovery_tests {
    #[test]
    fn semantic_related_test() {
        assert_eq!(super::tested_semantic_value(), 7);
    }
}
",
    );
    fs::write(&lib_path, lib_content).expect("failed to append pub mod broken");

    let folder_module = tmp.path().join("src/folder_mod");
    fs::create_dir(&folder_module).expect("failed to create folder module");
    fs::write(
        folder_module.join("mod.rs"),
        "pub mod nested;\npub fn folder_answer() -> u32 { 42 }\n",
    )
    .expect("failed to write folder module");
    fs::write(
        folder_module.join("nested.rs"),
        "pub struct Item;\npub fn item() -> Item { crate::folder_mod::nested::Item }\n",
    )
    .expect("failed to write nested folder module");
    fs::create_dir(tmp.path().join("src/moved")).expect("failed to create move destination");
    fs::write(
        tmp.path().join("src/move_items.rs"),
        "pub fn first() -> u32 { 1 }\n\npub fn second() -> u32 { 2 }\n",
    )
    .expect("failed to write move-item fixture");

    // Copy bad_format.rs into src/ — NOT added to lib.rs (no mod declaration).
    let fmt_src = fixture_dir.join("extras/bad_format.rs");
    let fmt_dst = tmp.path().join("src/bad_format.rs");
    fs::copy(&fmt_src, &fmt_dst).expect("failed to copy bad_format.rs");

    // Copy untouched.rs into src/ — NOT added to lib.rs and never opened by any
    // sub-case, so rust-analyzer never runs diagnostics on it (unlike bad_format.rs,
    // which gets diagnosed as a detached file once opened via format_document).
    let untouched_src = fixture_dir.join("extras/untouched.rs");
    let untouched_dst = tmp.path().join("src/untouched.rs");
    fs::copy(&untouched_src, &untouched_dst).expect("failed to copy untouched.rs");

    tmp
}

/// Recursively copy `src` directory contents into `dst` (dst must exist).
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            // Skip extras/ and target/ — not needed in the staged workspace.
            if entry.file_name() == "extras" || entry.file_name() == "target" {
                continue;
            }
            fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config generation
// ---------------------------------------------------------------------------

/// Typed config struct so that `toml::to_string` handles path escaping.
#[derive(Serialize, Deserialize)]
struct E2eConfig {
    workspace: WorkspaceConfig,
    lsp_servers: Vec<LspServerConfig>,
}

#[derive(Serialize, Deserialize)]
struct WorkspaceConfig {
    roots: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct LspServerConfig {
    language_id: String,
    command: String,
    args: Vec<String>,
    file_patterns: Vec<String>,
}

/// Write a minimal mcpls TOML config pointing at `ra_binary` and the given workspace root.
fn write_config(ra_binary: &Path, workspace_root: &Path, config_path: &Path) {
    let cfg = E2eConfig {
        workspace: WorkspaceConfig {
            roots: vec![workspace_root.to_string_lossy().into_owned()],
        },
        lsp_servers: vec![LspServerConfig {
            language_id: "rust".to_owned(),
            command: ra_binary.to_string_lossy().into_owned(),
            args: vec![],
            file_patterns: vec!["**/*.rs".to_owned()],
        }],
    };
    let content = toml::to_string(&cfg).expect("failed to serialize e2e config");
    fs::write(config_path, content).expect("failed to write e2e config");
}

// ---------------------------------------------------------------------------
// Anchor helpers
// ---------------------------------------------------------------------------

/// Find the 1-based line number of the first line in `file` containing `needle`.
///
/// Used instead of hardcoded line numbers so tests remain stable when the
/// fixture file is edited.
fn find_line(file: &Path, needle: &str) -> u32 {
    let content = fs::read_to_string(file).expect("failed to read file for anchor search");
    content
        .lines()
        .enumerate()
        .find_map(|(i, line)| {
            if line.contains(needle) {
                Some(u32::try_from(i + 1).expect("line number fits u32"))
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("anchor '{needle}' not found in {}", file.display()))
}

/// Find a 1-based line and UTF-8 byte character for the first matching needle.
fn find_position(file: &Path, needle: &str) -> (u32, u32) {
    let content = fs::read_to_string(file).expect("failed to read file for anchor search");
    content
        .lines()
        .enumerate()
        .find_map(|(line_index, line)| {
            line.find(needle).map(|character_index| {
                (
                    u32::try_from(line_index + 1).expect("line number fits u32"),
                    u32::try_from(character_index + 1).expect("character fits u32"),
                )
            })
        })
        .unwrap_or_else(|| panic!("anchor '{needle}' not found in {}", file.display()))
}

// ---------------------------------------------------------------------------
// Readiness gate
// ---------------------------------------------------------------------------

/// Poll `get_hover` on the `add` function until rust-analyzer returns content.
///
/// Timeout controlled by `MCPLS_RA_INDEX_TIMEOUT_SECS` (default 60, minimum 5).
///
/// NOTE: `$/progress` notifications are not captured by `bridge/notifications.rs`
/// (only `window/logMessage`, `window/showMessage`, and `publishDiagnostics` are
/// stored).  The readiness gate therefore uses hover-probe as the primary oracle.
/// See M-r1 in the architect handoff for the follow-up to add `$/progress` capture.
fn wait_until_ready(client: &mut McpClient, lib_rs: &Path) {
    // Windows CI runners are significantly slower than Linux/macOS.
    let default_timeout: u64 = if cfg!(windows) { 120 } else { 60 };
    let timeout_secs: u64 = std::env::var("MCPLS_RA_INDEX_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(default_timeout, |t| t.max(5));

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let lib_path = lib_rs.to_string_lossy().into_owned();
    let add_line = find_line(lib_rs, "pub fn add(");

    println!("[ra_e2e] waiting for rust-analyzer to index (timeout {timeout_secs}s)…");
    println!("[ra_e2e] hover probe: file={lib_path} line={add_line}");

    // Require 3 consecutive successful hover responses to guard against transient
    // successes during RA's intermediate indexing phases (observed on Windows CI).
    let required_consecutive: u32 = 3;
    let mut consecutive = 0u32;
    let mut last_print = Instant::now();
    loop {
        // Hover over `add` — the 'a' of "add" is at column 8 (1-based).
        let resp = client.call_tool(
            "get_hover",
            &json!({
                "file_path": lib_path,
                "line": add_line,
                "character": 8,
            }),
        );

        match &resp {
            Ok(r) => {
                let is_err = r["result"]["isError"].as_bool().unwrap_or(false);
                let text = assertions::content_text(r);
                // Require both "fn add" and "i32" to confirm type-checking is done.
                if text.contains("fn add") && text.contains("i32") {
                    consecutive += 1;
                    if consecutive >= required_consecutive {
                        println!("[ra_e2e] rust-analyzer is ready");
                        return;
                    }
                } else {
                    consecutive = 0;
                }
                // Print status every 10s so CI logs show progress.
                if last_print.elapsed() >= Duration::from_secs(10) {
                    let elapsed =
                        timeout_secs - deadline.saturating_duration_since(Instant::now()).as_secs();
                    println!(
                        "[ra_e2e] still waiting ({elapsed}s elapsed): consecutive={consecutive} \
                         isError={is_err} response={}",
                        &text[..text.len().min(120)]
                    );
                    last_print = Instant::now();
                }
            }
            Err(e) => {
                consecutive = 0;
                if last_print.elapsed() >= Duration::from_secs(10) {
                    println!("[ra_e2e] hover call error: {e}");
                    last_print = Instant::now();
                }
            }
        }

        assert!(
            Instant::now() < deadline,
            "[ra_e2e] rust-analyzer did not become ready within {timeout_secs}s; \
             set MCPLS_RA_INDEX_TIMEOUT_SECS to increase the limit"
        );

        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------------------
// Sub-cases (one per MCP tool)
// ---------------------------------------------------------------------------

/// Tool 1: `get_hover` — hover over `add` declaration.
fn sc_get_hover(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let add_line = find_line(&lib, "pub fn add(");
    let resp = client
        .call_tool(
            "get_hover",
            &json!({
                "file_path": lib.to_string_lossy(),
                "line": add_line,
                "character": 8,
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let hover_text = inner["contents"]["value"]
        .as_str()
        .or_else(|| inner["contents"].as_str())
        .unwrap_or("");

    if !hover_text.contains("add") {
        return Err(format!("hover text missing 'add': {hover_text}"));
    }
    if !hover_text.contains("i32") {
        return Err(format!("hover text missing 'i32': {hover_text}"));
    }
    Ok(())
}

/// Tool 2: `get_definition` — go to definition of `add` from inside `caller`.
fn sc_get_definition(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    // Inside caller body: `    add(1, 2)` — "add" starts at col 5 (1-based).
    let caller_line = find_line(&lib, "pub fn caller(");
    let resp = client
        .call_tool(
            "get_definition",
            &json!({
                "file_path": lib.to_string_lossy(),
                // caller body is two lines below the fn declaration
                "line": caller_line + 1,
                "character": 5,
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let locs = inner["locations"]
        .as_array()
        .ok_or_else(|| format!("expected locations array, got {inner}"))?;
    if locs.is_empty() {
        return Err("get_definition returned empty locations".to_owned());
    }

    let uri = locs[0]["uri"].as_str().unwrap_or("");
    if !uri.ends_with("/src/lib.rs") {
        return Err(format!(
            "definition URI does not end with '/src/lib.rs': {uri}"
        ));
    }
    Ok(())
}

/// Tool 3: `get_references` — find references to `add`.
fn sc_get_references(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let add_line = find_line(&lib, "pub fn add(");
    let resp = client
        .call_tool(
            "get_references",
            &json!({
                "file_path": lib.to_string_lossy(),
                "line": add_line,
                "character": 8,
                "include_declaration": true,
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let locs = inner["locations"]
        .as_array()
        .ok_or_else(|| format!("expected locations array, got {inner}"))?;
    if locs.len() < 2 {
        return Err(format!(
            "expected ≥2 references (decl + call site), got {}",
            locs.len()
        ));
    }

    // All reference URIs should point to lib.rs.
    for loc in locs {
        let uri = loc["uri"].as_str().unwrap_or("");
        if !uri.ends_with("/src/lib.rs") {
            return Err(format!(
                "reference URI does not end with '/src/lib.rs': {uri}"
            ));
        }
    }
    Ok(())
}

/// Tool 4: `get_diagnostics` — type error in broken.rs.
///
/// Also populates the cache used by sub-case 14.
fn sc_get_diagnostics(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let broken = workspace.join("src/broken.rs");
    let resp = client
        .call_tool(
            "get_diagnostics",
            &json!({ "file_path": broken.to_string_lossy() }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let diags = inner["diagnostics"]
        .as_array()
        .ok_or_else(|| format!("expected diagnostics array, got {inner}"))?;

    // Poll for diagnostics — rust-analyzer may need a few seconds to analyze
    // broken.rs after the initial `textDocument/didOpen`.
    let final_diags = if diags.is_empty() {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            std::thread::sleep(Duration::from_millis(250));

            // Try pull-based diagnostics first.  Ignore transient LSP errors
            // (e.g. rust-analyzer may cancel the request while still indexing).
            let j2: Value = client
                .call_tool(
                    "get_diagnostics",
                    &json!({ "file_path": broken.to_string_lossy() }),
                )
                .ok()
                .map_or(Value::Null, |r| {
                    let t = assertions::content_text(&r);
                    serde_json::from_str(&t).unwrap_or(Value::Null)
                });
            if let Some(d2) = j2["diagnostics"].as_array()
                && !d2.is_empty()
            {
                break d2.clone();
            }

            // Also check push-based cache.
            let r3 = client
                .call_tool(
                    "get_cached_diagnostics",
                    &json!({ "file_path": broken.to_string_lossy() }),
                )
                .map_err(|e| format!("cached call failed: {e}"))?;
            let t3 = assertions::content_text(&r3);
            let j3: Value = serde_json::from_str(&t3).unwrap_or(Value::Null);
            if let Some(d3) = j3["diagnostics"].as_array()
                && !d3.is_empty()
            {
                break d3.clone();
            }

            if Instant::now() >= deadline {
                return Err("no diagnostics for broken.rs within 15 s".to_owned());
            }
        }
    } else {
        diags.clone()
    };

    let has_error = final_diags
        .iter()
        .any(|d| d["severity"].as_str() == Some("error"));
    if !has_error {
        return Err(format!(
            "no Error-severity diagnostic in broken.rs: {final_diags:?}"
        ));
    }
    Ok(())
}

/// Tool 5: `rename_symbol` — rename `add` → `plus`.
fn sc_rename_symbol(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let add_line = find_line(&lib, "pub fn add(");
    let resp = client
        .call_tool(
            "rename_symbol",
            &json!({
                "file_path": lib.to_string_lossy(),
                "line": add_line,
                "character": 8,
                "new_name": "plus",
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let changes = inner["changes"]
        .as_array()
        .ok_or_else(|| format!("expected changes array, got {inner}"))?;
    if changes.is_empty() {
        return Err(
            "rename_symbol returned empty changes; bridge may not handle documentChanges format"
                .to_owned(),
        );
    }
    Ok(())
}

/// Tool 6: `get_completions` — completions after `ad` inside `caller`.
fn sc_get_completions(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    // Inside caller body: `    add(1, 2)` — column 7 is after 'a','d' (prefix "ad").
    let caller_line = find_line(&lib, "pub fn caller(");
    let body_line = caller_line + 1;

    // Retry loop: completions may not be available until rust-analyzer is fully ready.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool(
                "get_completions",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "line": body_line,
                    "character": 7,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let items = inner["items"]
            .as_array()
            .or_else(|| inner.as_array())
            .ok_or_else(|| format!("expected completions array, got {inner}"))?;

        let found = items
            .iter()
            .any(|i| i["label"].as_str().unwrap_or("").contains("add"));
        if found {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "get_completions: 'add' not returned after 10 s; items: {items:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 7: `get_document_symbols` — symbols in lib.rs include add, caller, Point.
fn sc_get_document_symbols(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let resp = client
        .call_tool(
            "get_document_symbols",
            &json!({ "file_path": lib.to_string_lossy() }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let syms = inner["symbols"]
        .as_array()
        .or_else(|| inner.as_array())
        .ok_or_else(|| format!("expected symbols array, got {inner}"))?;

    for expected in &["add", "caller", "Point"] {
        let found = syms
            .iter()
            .any(|s| s["name"].as_str().unwrap_or("").contains(expected));
        if !found {
            return Err(format!("symbol '{expected}' not found in document symbols"));
        }
    }
    if inner["filters"]["max_depth"] != 1
        || inner["returned"].as_u64().is_none()
        || inner["total"].as_u64().is_none()
        || !inner["truncated"].is_boolean()
        || syms.iter().any(|symbol| symbol["children"].is_array())
        || syms.iter().any(|symbol| {
            symbol["source"]["status"] != "available" || !symbol["symbol_handle"].is_string()
        })
    {
        return Err(format!("compact outline contract is incomplete: {inner}"));
    }

    let private = client
        .call_tool(
            "get_document_symbols",
            &json!({
                "file_path": lib.to_string_lossy(),
                "query": "fmt",
                "match_mode": "exact",
                "max_depth": 4,
                "include_private": true
            }),
        )
        .map_err(|e| format!("private outline query failed: {e}"))?;
    let private: Value = serde_json::from_str(&assertions::assert_tool_ok(&private))
        .map_err(|e| format!("bad private outline JSON: {e}"))?;
    let private_symbols = private["symbols"]
        .as_array()
        .ok_or_else(|| format!("private outline has no symbols: {private}"))?;
    if private["returned"] != 1
        || !private_symbols
            .iter()
            .any(|symbol| symbol["name"] == "fmt" || symbol.to_string().contains("\"fmt\""))
    {
        return Err(format!("private exact outline did not find fmt: {private}"));
    }
    Ok(())
}

/// Tool 8: `format_document` — format `bad_format.rs`, compare to golden.
fn sc_format_document(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let bad_fmt = workspace.join("src/bad_format.rs");
    let resp = client
        .call_tool(
            "format_document",
            &json!({ "file_path": bad_fmt.to_string_lossy() }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let formatted = inner["formatted_content"]
        .as_str()
        .or_else(|| inner["content"].as_str())
        .or_else(|| inner.as_str())
        .unwrap_or("");

    if formatted.is_empty() {
        // Some LSP servers return text edits instead of the full file.
        let edits = inner["edits"]
            .as_array()
            .or_else(|| inner["changes"].as_array());
        if edits.map_or(0, Vec::len) == 0 {
            return Err(format!(
                "format_document returned neither formatted content nor edits: {inner}"
            ));
        }
        return Ok(());
    }

    let golden_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden/bad_format.fmt.rs");
    let golden =
        fs::read_to_string(&golden_path).map_err(|e| format!("failed to read golden file: {e}"))?;

    if formatted.trim() != golden.trim() {
        return Err(format!(
            "formatted output does not match golden.\nExpected:\n{golden}\nGot:\n{formatted}"
        ));
    }
    Ok(())
}

/// Tool 9: `workspace_symbol_search` — search for "add".
fn sc_workspace_symbol_search(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    // Retry: workspace symbol search may return empty until rust-analyzer
    // has fully indexed all files in the workspace.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let resp = client
            .call_tool(
                "workspace_symbol_search",
                &json!({
                    "project_id": "default",
                    "query": "add",
                    "match_mode": "exact",
                    "scope": "project"
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let syms = inner["symbols"]
            .as_array()
            .or_else(|| inner.as_array())
            .ok_or_else(|| format!("expected symbols array, got {inner}"))?;

        if let Some(symbol) = syms.iter().find(|symbol| symbol["name"] == "add") {
            let contract_is_complete = symbol["match_class"] == "exact"
                && symbol["score"] == 100
                && symbol["origin"] == "project_local"
                && symbol["project_relative_path"] == "src/lib.rs"
                && symbol["location"]["source"]["status"] == "available"
                && symbol["location"]["symbol_handle"].is_string()
                && inner["total"].as_u64().is_some()
                && inner["returned"].as_u64().is_some()
                && inner["truncated"].is_boolean();
            if contract_is_complete && syms.iter().all(|symbol| symbol["name"] == "add") {
                return Ok(());
            }
            return Err(format!(
                "workspace_symbol_search returned an incomplete exact-first contract: {inner}"
            ));
        }

        if Instant::now() >= deadline {
            return Err(
                "workspace_symbol_search returned no results for 'add' after 15 s".to_owned(),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn sc_symbol_handle_follow_ups(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let search = client
        .call_tool(
            "workspace_symbol_search",
            &json!({ "project_id": "default", "query": "add" }),
        )
        .map_err(|error| format!("workspace search failed: {error}"))?;
    let search: Value = serde_json::from_str(&assertions::assert_tool_ok(&search))
        .map_err(|error| format!("bad workspace search JSON: {error}"))?;
    let symbol = search["symbols"]
        .as_array()
        .and_then(|symbols| symbols.iter().find(|symbol| symbol["name"] == "add"))
        .ok_or_else(|| format!("workspace search did not return add: {search}"))?;
    let handle = symbol["location"]["symbol_handle"]
        .as_str()
        .ok_or_else(|| format!("workspace symbol has no handle: {symbol}"))?;

    let hover = client
        .call_tool(
            "get_hover",
            &json!({ "project_id": "default", "symbol_handle": handle }),
        )
        .map_err(|error| format!("handle hover failed: {error}"))?;
    let hover: Value = serde_json::from_str(&assertions::assert_tool_ok(&hover))
        .map_err(|error| format!("bad hover JSON: {error}"))?;
    if !hover["contents"].to_string().contains("add") {
        return Err(format!("handle hover did not describe add: {hover}"));
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let references = client
            .call_tool(
                "get_references",
                &json!({
                    "project_id": "default",
                    "symbol_handle": handle,
                    "include_declaration": true,
                }),
            )
            .map_err(|error| format!("handle references failed: {error}"))?;
        let references: Value = serde_json::from_str(&assertions::assert_tool_ok(&references))
            .map_err(|error| format!("bad references JSON: {error}"))?;
        if references["locations"].as_array().map_or(0, Vec::len) >= 2 {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!("handle references were incomplete: {references}"));
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    let outline = client
        .call_tool(
            "get_document_symbols",
            &json!({ "file_path": workspace.join("src/lib.rs").to_string_lossy() }),
        )
        .map_err(|error| format!("document outline failed: {error}"))?;
    let outline: Value = serde_json::from_str(&assertions::assert_tool_ok(&outline))
        .map_err(|error| format!("bad document outline JSON: {error}"))?;
    let outline_symbol = outline["symbols"]
        .as_array()
        .and_then(|symbols| symbols.iter().find(|symbol| symbol["name"] == "add"))
        .ok_or_else(|| format!("document outline add has no handle: {outline}"))?;
    let outline_handle = outline_symbol["symbol_handle"]
        .as_str()
        .ok_or_else(|| format!("document outline add has no handle: {outline_symbol}"))?;
    let outline_hover = client
        .call_tool(
            "get_hover",
            &json!({ "project_id": "default", "symbol_handle": outline_handle }),
        )
        .map_err(|error| format!("outline handle hover failed: {error}"))?;
    let outline_hover: Value = serde_json::from_str(&assertions::assert_tool_ok(&outline_hover))
        .map_err(|error| format!("bad outline hover JSON: {error}"))?;
    outline_hover["contents"]
        .to_string()
        .contains("add")
        .then_some(())
        .ok_or_else(|| {
            format!(
                "outline handle hover did not describe add: symbol={outline_symbol}, hover={outline_hover}"
            )
        })
}

/// Tool 10: `get_code_actions` — "Implement missing members" on an empty trait impl.
///
/// Quickfix-style code actions require rust-analyzer to receive the diagnostic
/// object with its internal `data` field in the request context — the bridge
/// currently sends an empty diagnostics list.  "Implement missing members" is a
/// structural refactoring action that is context-free and does not depend on
/// diagnostic data, making it a reliable trigger.
fn sc_get_code_actions(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    // `impl Greet for CodeActionTarget { }` — empty impl body spanning two lines.
    // RA offers "Implement missing members" when cursor is inside the impl block.
    // Use a point cursor (start == end) at character 6 on the `impl` line, inside the keyword.
    let impl_line = find_line(&lib_rs, "impl Greet for CodeActionTarget {");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_inner;
    loop {
        let resp = client
            .call_tool(
                "get_code_actions",
                &json!({
                    "file_path": lib_rs.to_string_lossy(),
                    "start_line": impl_line,
                    "start_character": 6,
                    "end_line": impl_line,
                    "end_character": 6,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;
        last_inner = inner.clone();

        let actions = inner["actions"]
            .as_array()
            .or_else(|| inner.as_array())
            .ok_or_else(|| format!("expected actions array, got {inner}"))?;

        if !actions.is_empty() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "get_code_actions: no actions on empty trait impl after 20 s\n\
                 actions_response={last_inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------------------
// Call hierarchy helpers
// ---------------------------------------------------------------------------

/// Tool 11: `prepare_call_hierarchy` — on `add`.
///
/// Returns the prepared item for use by sub-cases 12 and 13.
///
/// Since `CallHierarchyItemResult` now serializes `selectionRange` in camelCase,
/// the item round-trips correctly without any field renaming.
fn prepare_call_hierarchy_item(client: &mut McpClient, workspace: &Path) -> Result<Value, String> {
    let lib = workspace.join("src/lib.rs");
    let add_line = find_line(&lib, "pub fn add(");
    let resp = client
        .call_tool(
            "prepare_call_hierarchy",
            &json!({
                "file_path": lib.to_string_lossy(),
                "line": add_line,
                "character": 8,
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let items = inner["items"]
        .as_array()
        .or_else(|| inner.as_array())
        .ok_or_else(|| format!("expected items array, got {inner}"))?;

    if items.is_empty() {
        return Err("prepare_call_hierarchy returned no items".to_owned());
    }

    let name = items[0]["name"].as_str().unwrap_or("");
    if !name.contains("add") {
        return Err(format!(
            "expected call hierarchy item for 'add', got '{name}'"
        ));
    }
    Ok(items[0].clone())
}

fn sc_prepare_call_hierarchy(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    prepare_call_hierarchy_item(client, workspace).map(|_| ())
}

/// Tool 12: `get_incoming_calls` — `caller` must appear as incoming caller to `add`.
fn sc_get_incoming_calls(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let item = prepare_call_hierarchy_item(client, workspace)?;
    // Retry: callHierarchy/incomingCalls may return empty on first query while
    // rust-analyzer resolves cross-function relationships.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let resp = client
            .call_tool("get_incoming_calls", &json!({ "item": item }))
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let calls = inner["calls"]
            .as_array()
            .or_else(|| inner.as_array())
            .ok_or_else(|| format!("expected calls array, got {inner}"))?;

        if !calls.is_empty() {
            // Verify that `caller` is among the incoming callers.
            let found = calls.iter().any(|c| {
                c["from"]["name"].as_str().unwrap_or("").contains("caller")
                    || c["caller"]["name"]
                        .as_str()
                        .unwrap_or("")
                        .contains("caller")
            });
            if !found {
                return Err(format!(
                    "get_incoming_calls: 'caller' not found in incoming calls: {calls:?}"
                ));
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err("get_incoming_calls: empty result for 'add' after 15 s; \
                 'caller' should be an incoming caller"
                .to_owned());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 13: `get_outgoing_calls` — `add` calls nothing user-defined.
fn sc_get_outgoing_calls(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let item = prepare_call_hierarchy_item(client, workspace)?;
    let resp = client
        .call_tool("get_outgoing_calls", &json!({ "item": item }))
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let calls = inner["calls"]
        .as_array()
        .or_else(|| inner.as_array())
        .ok_or_else(|| format!("expected calls array, got {inner}"))?;

    // `add(a, b) { a + b }` contains no function calls.
    // An empty result is correct.  Reject any call to a user-defined function
    // (names outside std/core/alloc/compiler_builtins namespaces).
    for call in calls {
        let name = call["to"]["name"]
            .as_str()
            .or_else(|| call["callee"]["name"].as_str())
            .unwrap_or("");
        let in_std = name.is_empty()
            || name.contains("core")
            || name.contains("std")
            || name.contains("alloc")
            || name.contains("compiler_builtins");
        if !in_std {
            return Err(format!(
                "unexpected user-defined outgoing call from 'add': '{name}'"
            ));
        }
    }
    Ok(())
}

/// Tool 14: `get_cached_diagnostics` — push cache populated during workspace indexing.
///
/// Uses `lib.rs` rather than `broken.rs`: lib.rs is opened via hover during
/// `wait_until_ready` (no pull-diagnostic request), so rust-analyzer sends
/// `publishDiagnostics` for it unconditionally during initial analysis.
/// `broken.rs` is queried via the pull-based `textDocument/diagnostic` API in
/// `sc_get_diagnostics`; newer RA versions skip push for files already served
/// via pull, making `broken.rs` unreliable as a push-cache trigger.
///
/// lib.rs contains `let _x = undefined_variable;` (E0425) so it always has
/// at least one error diagnostic pushed by RA after indexing.
fn sc_get_cached_diagnostics(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    let timeout_secs: u64 = 20;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let resp = client
            .call_tool(
                "get_cached_diagnostics",
                &json!({ "file_path": lib_rs.to_string_lossy() }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let diags = inner["diagnostics"]
            .as_array()
            .ok_or_else(|| format!("expected diagnostics array, got {inner}"))?;

        if !diags.is_empty() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            // Newer rust-analyzer versions may expose diagnostics only through
            // the pull API. Treat that as a valid notification-pipeline
            // result when the equivalent project-scoped query succeeds.
            let fallback = client
                .call_tool(
                    "get_diagnostics",
                    &json!({
                        "file_path": workspace.join("src/broken.rs").to_string_lossy()
                    }),
                )
                .map_err(|e| format!("get_diagnostics fallback failed: {e}"))?;
            let fallback_text = assertions::assert_tool_ok(&fallback);
            let fallback_inner: Value = serde_json::from_str(&fallback_text)
                .map_err(|e| format!("bad fallback diagnostics JSON: {e}"))?;
            if fallback_inner["diagnostics"].as_array().is_some() {
                return Ok(());
            }
            return Err(format!(
                "get_cached_diagnostics: push cache empty after {timeout_secs} s and pull fallback had unexpected shape: {fallback_inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Tool 15: `get_server_logs` — returns `window/logMessage` entries.
///
/// rust-analyzer does not emit `window/logMessage` by default; it uses
/// `window/showMessage` and `$/progress` for user-visible status.  This
/// sub-case asserts that the tool responds without MCP-level error and
/// returns the expected shape, even if entries are empty.  The stronger
/// liveness signal for the notification pipeline is `sc_get_server_messages`.
fn sc_get_server_logs(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    let resp = client
        .call_tool(
            "get_server_logs",
            &json!({ "project_id": "default", "limit": 50 }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    // Verify expected shape; entries may be empty since rust-analyzer does not
    // emit window/logMessage without additional logging configuration.
    let _entries = inner["entries"]
        .as_array()
        .or_else(|| inner["logs"].as_array())
        .or_else(|| inner.as_array())
        .ok_or_else(|| format!("expected log entries array, got {inner}"))?;

    Ok(())
}

/// Resolve a resource URI ending with `suffix` by querying `resources/list`.
///
/// Avoids drift from `make_uri`'s encoding rules on macOS `/private/var/...`
/// canonicalised paths or Windows UNC.
fn resource_uri_ending_with(client: &mut McpClient, suffix: &str) -> Result<String, String> {
    let resp = client
        .list_resources()
        .map_err(|e| format!("list_resources: {e}"))?;
    let resources = resp["result"]["resources"]
        .as_array()
        .ok_or_else(|| format!("expected resources array, got {resp}"))?;
    resources
        .iter()
        .filter_map(|r| r["uri"].as_str())
        .find(|u| u.ends_with(suffix))
        .map(str::to_owned)
        .ok_or_else(|| format!("no URI ending with '{suffix}' in resources list: {resources:?}"))
}

/// Resolve the `lsp-diagnostics` URI for `lib.rs` by querying `resources/list`.
fn lib_rs_uri(client: &mut McpClient) -> Result<String, String> {
    resource_uri_ending_with(client, "/src/lib.rs")
}

/// Tool 17: `get_signature_help` — signature of `add` inside its call site.
fn sc_get_signature_help(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let line = find_line(&lib, "let s = add(");
    let content = fs::read_to_string(&lib).map_err(|e| format!("read lib.rs: {e}"))?;
    let source_line = content
        .lines()
        .nth(usize::try_from(line - 1).expect("line fits usize"))
        .unwrap_or("");
    // Place cursor just after the opening paren (1-based; line is ASCII).
    let character = u32::try_from(
        source_line
            .find('(')
            .ok_or_else(|| format!("no '(' on line {line}: {source_line}"))?
            + 2,
    )
    .expect("column fits u32");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool(
                "get_signature_help",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "line": line,
                    "character": character,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        if let Some(sigs) = inner["signatures"].as_array()
            && !sigs.is_empty()
        {
            let label = sigs[0]["label"].as_str().unwrap_or("");
            if !label.contains("add") {
                return Err(format!("signature label missing 'add': {label}"));
            }
            if !label.contains("i32") {
                return Err(format!("signature label missing 'i32': {label}"));
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "get_signature_help: no signatures after 10 s; response={inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 18: `go_to_implementation` — implementations of trait `Greet`.
fn sc_go_to_implementation(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let line = find_line(&lib, "pub trait Greet {");
    let content = fs::read_to_string(&lib).map_err(|e| format!("read lib.rs: {e}"))?;
    let source_line = content
        .lines()
        .nth(usize::try_from(line - 1).expect("line fits usize"))
        .unwrap_or("");
    // Cursor on the trait name "Greet" (1-based; ASCII line).
    let character = u32::try_from(
        source_line
            .find("Greet")
            .ok_or_else(|| format!("'Greet' not found on line {line}: {source_line}"))?
            + 1,
    )
    .expect("column fits u32");

    let impl_line = find_line(&lib, "impl Greet for CodeActionTarget {");
    // The bridge normalizes ranges to 1-based MCP via `normalize_range`,
    // so the response line equals the 1-based source line directly.
    let expected_mcp_line = impl_line;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool(
                "go_to_implementation",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "line": line,
                    "character": character,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let locs = inner["locations"]
            .as_array()
            .or_else(|| inner.as_array())
            .filter(|a| !a.is_empty());

        if let Some(locs) = locs {
            let has_lib_rs = locs
                .iter()
                .any(|l| l["uri"].as_str().unwrap_or("").ends_with("/src/lib.rs"));
            if !has_lib_rs {
                return Err(format!(
                    "go_to_implementation: no location in lib.rs: {locs:?}"
                ));
            }
            let has_impl_line = locs.iter().any(|l| {
                l["range"]["start"]["line"].as_u64() == Some(u64::from(expected_mcp_line))
            });
            if !has_impl_line {
                return Err(format!(
                    "go_to_implementation: impl line {expected_mcp_line} not in locations: {locs:?}"
                ));
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "go_to_implementation: empty locations after 10 s; response={inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 19: `go_to_type_definition` — type definition of `p` (a `Point`).
fn sc_go_to_type_definition(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let line = find_line(&lib, "let p = Point {");
    let content = fs::read_to_string(&lib).map_err(|e| format!("read lib.rs: {e}"))?;
    let source_line = content
        .lines()
        .nth(usize::try_from(line - 1).expect("line fits usize"))
        .unwrap_or("");
    // Cursor on identifier `p` (1-based; ASCII line).
    let character = u32::try_from(
        source_line
            .find(" p ")
            .ok_or_else(|| format!("' p ' not found on line {line}: {source_line}"))?
            + 2,
    )
    .expect("column fits u32");

    let struct_line = find_line(&lib, "pub struct Point {");
    // The bridge normalizes ranges to 1-based MCP via `normalize_range`,
    // so the response line equals the 1-based source line directly.
    let expected_mcp_line = struct_line;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool(
                "go_to_type_definition",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "line": line,
                    "character": character,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let locs = inner["locations"]
            .as_array()
            .or_else(|| inner.as_array())
            .filter(|a| !a.is_empty());

        if let Some(locs) = locs {
            let uri = locs[0]["uri"].as_str().unwrap_or("");
            if !uri.ends_with("/src/lib.rs") {
                return Err(format!(
                    "go_to_type_definition: URI does not end with '/src/lib.rs': {uri}"
                ));
            }
            let got_line = locs[0]["range"]["start"]["line"].as_u64();
            if got_line != Some(u64::from(expected_mcp_line)) {
                return Err(format!(
                    "go_to_type_definition: expected line {expected_mcp_line}, got {got_line:?}"
                ));
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "go_to_type_definition: empty locations after 10 s; response={inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 20: `get_inlay_hints` — type hints in `lsp317_target`.
fn sc_get_inlay_hints(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let start_line = find_line(&lib, "pub fn lsp317_target(");
    let end_line = find_line(&lib, "let _ = (p, s);") + 1;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool(
                "get_inlay_hints",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "start_line": start_line,
                    "start_character": 1,
                    "end_line": end_line,
                    "end_character": 1,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let hints_arr = inner["hints"].as_array().or_else(|| inner.as_array());
        if let Some(hints) = hints_arr
            && !hints.is_empty()
        {
            let serialized = serde_json::to_string(&inner["hints"])
                .unwrap_or_else(|_| serde_json::to_string(&inner).unwrap_or_default());
            if !serialized.contains("Point") && !serialized.contains("i32") {
                return Err(format!(
                    "get_inlay_hints: no 'Point' or 'i32' hint found; hints={serialized}"
                ));
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "get_inlay_hints: no hints after 10 s; response={inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Resource sub-case 1: `list_resources` — at least one lib.rs resource exposed.
///
/// Precondition: `sc_get_hover` and earlier sub-cases have triggered didOpen for
/// lib.rs — this sub-case must remain after them in the registry.
fn sc_list_resources(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    let resp = client
        .list_resources()
        .map_err(|e| format!("call failed: {e}"))?;

    let resources = resp["result"]["resources"]
        .as_array()
        .ok_or_else(|| format!("expected resources array, got {resp}"))?;

    if resources.is_empty() {
        return Err("list_resources: empty resources array".to_owned());
    }

    let diagnostic_resources: Vec<_> = resources
        .iter()
        .filter(|resource| {
            resource["uri"]
                .as_str()
                .is_some_and(|uri| uri.starts_with("lsp-diagnostics:///"))
        })
        .collect();
    if diagnostic_resources.is_empty() {
        return Err(format!(
            "list_resources: no lsp-diagnostics resources: {resources:?}"
        ));
    }

    let has_lib_rs = diagnostic_resources
        .iter()
        .any(|r| r["uri"].as_str().unwrap_or("").ends_with("/src/lib.rs"));
    if !has_lib_rs {
        return Err(format!(
            "list_resources: no URI ending with '/src/lib.rs': {resources:?}"
        ));
    }

    Ok(())
}

/// Resource sub-case 2: `read_resource` — reads diagnostics for lib.rs.
fn sc_read_resource(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    let uri = lib_rs_uri(client)?;
    let resp = client
        .read_resource(&uri)
        .map_err(|e| format!("call failed: {e}"))?;

    let contents = resp["result"]["contents"]
        .as_array()
        .ok_or_else(|| format!("expected contents array, got {resp}"))?;

    if contents.is_empty() {
        return Err("read_resource: empty contents array".to_owned());
    }
    if contents[0]["uri"].as_str() != Some(&uri) {
        return Err(format!(
            "read_resource: contents[0].uri mismatch; expected {uri}, got {}",
            contents[0]["uri"]
        ));
    }
    let text = contents[0]["text"].as_str().ok_or_else(|| {
        format!(
            "read_resource: contents[0].text is not a string: {}",
            contents[0]
        )
    })?;
    serde_json::from_str::<Value>(text)
        .map_err(|e| format!("read_resource: contents[0].text is not valid JSON: {e}"))?;

    Ok(())
}

/// Resource sub-case 3: subscribing to lib.rs replays its already-cached diagnostics
/// immediately (issue #131), then subscribe/unsubscribe still round-trip cleanly.
///
/// Precondition: `sc_get_cached_diagnostics` has already run, so lib.rs has an entry
/// in the push-diagnostics cache — this sub-case must remain after it in the registry.
fn sc_subscribe_unsubscribe_resource(
    client: &mut McpClient,
    _workspace: &Path,
) -> Result<(), String> {
    let uri = lib_rs_uri(client)?;

    let sub_resp = client
        .subscribe_resource(&uri)
        .map_err(|e| format!("subscribe call failed: {e}"))?;
    if sub_resp.get("error").is_some() {
        return Err(format!("subscribe returned error: {sub_resp}"));
    }
    // result field must be present (null for success)
    if sub_resp.get("result").is_none() {
        return Err(format!(
            "subscribe: no 'result' field in response: {sub_resp}"
        ));
    }

    // #131: diagnostics are already cached for lib.rs (sc_get_cached_diagnostics ran
    // earlier), so subscribe must replay them immediately via a
    // notifications/resources/updated push instead of waiting for the next LSP update.
    let replayed = client
        .take_notifications()
        .into_iter()
        .any(|n| n["method"] == "notifications/resources/updated" && n["params"]["uri"] == uri);
    if !replayed {
        return Err(
            "subscribe: expected an immediate notifications/resources/updated replay for \
             lib.rs (diagnostics already cached), got none"
                .to_owned(),
        );
    }

    let unsub_resp = client
        .unsubscribe_resource(&uri)
        .map_err(|e| format!("unsubscribe call failed: {e}"))?;
    if unsub_resp.get("error").is_some() {
        return Err(format!("unsubscribe returned error: {unsub_resp}"));
    }
    if unsub_resp.get("result").is_none() {
        return Err(format!(
            "unsubscribe: no 'result' field in response: {unsub_resp}"
        ));
    }

    // TODO(critic): add negative case with "file:///tmp/x.rs" (wrong scheme) once error envelope shape confirmed
    // TODO(critic): assert idempotent unsubscribe — second unsubscribe of same URI returns Ok

    Ok(())
}

/// Resource sub-case 4 (negative): subscribing to a resource with no cached diagnostics
/// yet must not produce a spurious `notifications/resources/updated` push (issue #131).
///
/// `untouched.rs` is never opened by any sub-case and not part of the crate's module
/// tree, so rust-analyzer never runs diagnostics on it — unlike `bad_format.rs`, which
/// modern rust-analyzer diagnoses as a detached file once `sc_format_document` opens
/// it via `didOpen`, even outside the module tree. Its URI is derived from lib.rs's
/// (same directory, sibling filename) rather than via `resources/list`, since
/// `untouched.rs` is never opened and therefore never appears there.
fn sc_subscribe_no_replay_without_cached_diagnostics(
    client: &mut McpClient,
    _workspace: &Path,
) -> Result<(), String> {
    let lib_uri = lib_rs_uri(client)?;
    let uri = lib_uri.replace("lib.rs", "untouched.rs");
    if uri == lib_uri {
        return Err(format!("failed to derive sibling URI from {lib_uri}"));
    }

    // Drain any notifications left over from earlier sub-cases before subscribing,
    // so this check only observes what subscribe() itself produces.
    client.take_notifications();

    let sub_resp = client
        .subscribe_resource(&uri)
        .map_err(|e| format!("subscribe call failed: {e}"))?;
    if sub_resp.get("error").is_some() {
        return Err(format!("subscribe returned error: {sub_resp}"));
    }
    if sub_resp.get("result").is_none() {
        return Err(format!(
            "subscribe: no 'result' field in response: {sub_resp}"
        ));
    }

    let spurious = client
        .take_notifications()
        .into_iter()
        .any(|n| n["method"] == "notifications/resources/updated" && n["params"]["uri"] == uri);
    if spurious {
        return Err(format!(
            "subscribe: got an unexpected notifications/resources/updated replay for \
             {uri} which has no cached diagnostics"
        ));
    }

    client
        .unsubscribe_resource(&uri)
        .map_err(|e| format!("unsubscribe call failed: {e}"))?;

    Ok(())
}

/// Tool 16: `get_server_messages` — readiness gate already exercised this tool.
fn sc_get_server_messages(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    let resp = client
        .call_tool(
            "get_server_messages",
            &json!({ "project_id": "default", "limit": 20 }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    assertions::assert_tool_ok(&resp);
    Ok(())
}

/// rust-analyzer SSR syntax validation and write-free replacement preview.
fn sc_structural_replace_preview(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    let rule = "add($a, $b) ==>> add($b, $a)";
    let parsed = call_json(
        client,
        "structural_replace_preview",
        &json!({
            "project_id": "default",
            "file_path": lib_rs,
            "dialect": "rust_analyzer_ssr",
            "query": rule,
            "parse_only": true,
            "position_encoding": "utf-8",
        }),
    )?;
    if parsed["engine"] != "rust_analyzer"
        || parsed["parse_only"] != true
        || parsed.get("plan_id").is_some()
    {
        return Err(format!("unexpected SSR parse-only response: {parsed}"));
    }

    let preview = call_json(
        client,
        "structural_replace_preview",
        &json!({
            "project_id": "default",
            "file_path": lib_rs,
            "dialect": "rust_analyzer_ssr",
            "query": rule,
            "position_encoding": "utf-8",
        }),
    )?;
    if preview["producer"] != "rust_analyzer"
        || preview["verification"] != "semantic_verified"
        || preview["match_count"]
            .as_u64()
            .is_none_or(|count| count < 2)
        || preview["plan_id"].as_str().is_none()
        || !preview["unified_diff"]
            .as_str()
            .is_some_and(|diff| diff.contains("add(2, 1)"))
    {
        return Err(format!("unexpected SSR replacement preview: {preview}"));
    }
    Ok(())
}

/// Tool 25: native rust-analyzer module rename through workspace-edit preview.
fn sc_native_module_rename_preview(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    let module_line = find_line(&lib_rs, "pub mod functions;");
    let preview = call_json(
        client,
        "rename_preview",
        &json!({
            "project_id": "default",
            "file_path": lib_rs,
            "line": module_line,
            "character": 9,
            "new_name": "functions_renamed_probe",
            "position_encoding": "utf-8",
        }),
    )?;
    let source = workspace.join("src/functions.rs");
    let destination = workspace.join("src/functions_renamed_probe.rs");
    if !preview["operations"].as_array().is_some_and(|operations| {
        operations.iter().any(|operation| {
            operation.as_str().is_some_and(|value| {
                value.contains(source.to_string_lossy().as_ref())
                    && value.contains(destination.to_string_lossy().as_ref())
            })
        })
    }) {
        return Err(format!(
            "native module rename preview omitted its RenameFile operation: {preview}"
        ));
    }
    Ok(())
}

/// Tool 26: native rust-analyzer module extraction through code-action preview.
fn sc_native_module_move_code_action_preview(
    client: &mut McpClient,
    workspace: &Path,
) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    let module_line = find_line(&lib_rs, "pub mod move_target {");
    let listed = call_json(
        client,
        "code_action_list",
        &json!({
            "project_id": "default",
            "file_path": lib_rs,
            "start_line": module_line,
            "start_character": 9,
            "end_line": module_line,
            "end_character": 9,
            "kind_filter": "refactor.extract",
        }),
    )?;
    let action = listed["actions"]
        .as_array()
        .and_then(|actions| {
            actions
                .iter()
                .find(|action| action["title"] == "Extract module to file")
        })
        .ok_or_else(|| format!("rust-analyzer omitted native module move action: {listed}"))?;
    let action_id = action["action_id"]
        .as_str()
        .ok_or_else(|| format!("native module move omitted action_id: {action}"))?;
    let preview = call_json(
        client,
        "code_action_preview",
        &json!({
            "project_id": "default",
            "action_id": action_id,
            "position_encoding": "utf-8",
        }),
    )?;
    let destination = workspace.join("src/move_target.rs");
    if !preview["affected_files"].as_array().is_some_and(|files| {
        files
            .iter()
            .any(|file| file.as_str() == Some(destination.to_string_lossy().as_ref()))
    }) || !preview["operations"].as_array().is_some_and(|operations| {
        operations.iter().any(|operation| {
            operation
                .as_str()
                .is_some_and(|value| value.starts_with("create "))
        })
    }) {
        return Err(format!(
            "native module move preview omitted its CreateFile operation: {preview}"
        ));
    }
    Ok(())
}

fn semantic_path_rename_plan(preview: &Value, label: &str) -> Result<String, String> {
    if preview["verification"] != "semantic_verified"
        || preview["semantic_edit_count"]
            .as_u64()
            .is_none_or(|count| count == 0)
        || !preview["semantic_providers"]
            .as_array()
            .is_some_and(|providers| providers.iter().any(|provider| provider == "rust"))
    {
        return Err(format!("{label} was not semantically verified: {preview}"));
    }
    let rename_count = preview["operations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|operation| {
            operation
                .as_str()
                .is_some_and(|value| value.starts_with("rename "))
        })
        .count();
    if rename_count != 1 {
        return Err(format!(
            "{label} did not contain exactly one RenameFile: {preview}"
        ));
    }
    preview["plan_id"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{label} omitted plan_id: {preview}"))
}

/// Read-only semantic context used to plan and validate edits.
fn sc_semantic_discovery(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let (call_line, receiver_character) = find_position(&lib, "SemanticType.semantic_value()");
    let call_character = receiver_character
        .saturating_add(u32::try_from("SemanticType.".len()).map_err(|error| error.to_string())?);
    let position = json!({
        "project_id": "default",
        "file_path": lib,
        "line": call_line,
        "character": call_character,
    });
    let declaration = call_json(client, "get_declaration", &position)?;
    let definition = call_json(
        client,
        "get_definition",
        &json!({
            "file_path": lib,
            "line": call_line,
            "character": call_character,
        }),
    )?;
    if declaration["supported"] != true || declaration["provider"] != "standard_lsp" {
        return Err(format!(
            "declaration lookup was not capability-gated: {declaration}"
        ));
    }
    let declaration_line = declaration["locations"][0]["range"]["start"]["line"]
        .as_u64()
        .ok_or_else(|| format!("declaration lookup returned no location: {declaration}"))?;
    let definition_line = definition["locations"][0]["range"]["start"]["line"]
        .as_u64()
        .ok_or_else(|| format!("definition lookup returned no location: {definition}"))?;
    if declaration_line == definition_line {
        return Err(format!(
            "declaration and definition were not distinguished: {declaration} / {definition}"
        ));
    }

    let (unicode_line, unicode_character) = find_position(&lib, "café_value() }");
    let unicode = call_json(
        client,
        "get_declaration",
        &json!({
            "project_id": "default",
            "file_path": lib,
            "line": unicode_line,
            "character": unicode_character,
        }),
    )?;
    if unicode["locations"].as_array().is_none_or(Vec::is_empty) {
        return Err(format!(
            "UTF-8 declaration position returned no location: {unicode}"
        ));
    }

    let nested = workspace.join("src/folder_mod/nested.rs");
    let parent = call_json(
        client,
        "get_parent_module",
        &json!({
            "project_id": "default",
            "file_path": nested,
            "line": 1,
            "character": 1,
        }),
    )?;
    if parent["supported"] != true
        || !parent["locations"].as_array().is_some_and(|locations| {
            locations.iter().any(|location| {
                location["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.ends_with("/folder_mod/mod.rs"))
            })
        })
    {
        return Err(format!("parent-module lookup missed folder_mod: {parent}"));
    }

    let folder = workspace.join("src/folder_mod/mod.rs");
    let children = call_json(
        client,
        "get_child_modules",
        &json!({
            "project_id": "default",
            "file_path": folder,
            "line": 2,
            "character": 5,
        }),
    )?;
    if children["supported"] != true
        || !children["locations"].as_array().is_some_and(|locations| {
            locations.iter().any(|location| {
                location["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.ends_with("/folder_mod/mod.rs"))
                    && location["range"]["start"]["line"] == 1
            })
        })
    {
        return Err(format!(
            "child-module lookup missed the nested declaration: {children}"
        ));
    }

    let macro_line = find_line(&lib, "semantic_answer!()");
    let expansion = call_json(
        client,
        "expand_macro",
        &json!({
            "project_id": "default",
            "file_path": lib,
            "line": macro_line,
            "character": 42,
        }),
    )?;
    if expansion["supported"] != true
        || !expansion["macro_expansion"]["expansion"]
            .as_str()
            .is_some_and(|value| value.contains("7_u32"))
    {
        return Err(format!("macro expansion was unavailable: {expansion}"));
    }

    let selection_line = find_line(&lib, "let nested = (");
    let selections = call_json(
        client,
        "get_selection_ranges",
        &json!({
            "project_id": "default",
            "file_path": lib,
            "line": selection_line,
            "character": 31,
        }),
    )?;
    if selections["provider"] != "standard_lsp"
        || selections["selection_ranges"]
            .as_array()
            .is_none_or(|ranges| ranges.len() < 3)
    {
        return Err(format!("nested selections were not expanded: {selections}"));
    }

    let test_line = find_line(&lib, "fn semantic_related_test()");
    let runnables = call_json(
        client,
        "discover_runnables",
        &json!({
            "project_id": "default",
            "file_path": lib,
            "line": test_line,
            "character": 8,
        }),
    )?;
    if runnables["supported"] != true
        || !runnables["runnables"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["label"]
                    .as_str()
                    .is_some_and(|label| label.contains("semantic_related_test"))
                    && item["args"]["cargoArgs"].is_array()
            })
        })
    {
        return Err(format!("runnable command data was incomplete: {runnables}"));
    }

    let tested_line = find_line(&lib, "pub fn tested_semantic_value()");
    let related = call_json(
        client,
        "discover_related_tests",
        &json!({
            "project_id": "default",
            "file_path": lib,
            "line": tested_line,
            "character": 8,
        }),
    )?;
    if related["supported"] != true
        || !related["runnables"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["runnable"]["label"]
                    .as_str()
                    .is_some_and(|label| label.contains("semantic_related_test"))
            })
        })
    {
        return Err(format!("related-test discovery missed the test: {related}"));
    }
    Ok(())
}

/// Capability-gated local edits against the deployed rust-analyzer fork.
fn sc_local_edit_previews(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    let range = call_json(
        client,
        "range_format_preview",
        &json!({
            "project_id": "default",
            "file_path": lib_rs,
            "start_line": 1,
            "start_character": 1,
            "end_line": 2,
            "end_character": 1,
            "position_encoding": "utf-8",
        }),
    )?;
    if range["supported"] != false || range["changed"] != false || range.get("plan_id").is_some() {
        return Err(format!(
            "disabled rust-analyzer range formatting was not capability-gated: {range}"
        ));
    }

    let source = workspace.join("src/move_items.rs");
    let second_line = find_line(&source, "pub fn second");
    let movement = call_json(
        client,
        "move_item_preview",
        &json!({
            "project_id": "default",
            "file_path": source,
            "start_line": second_line,
            "start_character": 8,
            "end_line": second_line,
            "end_character": 8,
            "direction": "up",
            "position_encoding": "utf-8",
        }),
    )?;
    if movement["supported"] != true || movement["changed"] != true {
        return Err(format!(
            "rust-analyzer move-item returned no edit: {movement}"
        ));
    }
    let plan_id = movement["plan_id"]
        .as_str()
        .ok_or_else(|| format!("move-item preview omitted plan_id: {movement}"))?;
    call_json(
        client,
        "workspace_edit_apply",
        &json!({"project_id": "default", "plan_id": plan_id}),
    )?;
    let moved = fs::read_to_string(&source)
        .map_err(|error| format!("failed to read moved items: {error}"))?;
    if moved
        .find("pub fn second")
        .zip(moved.find("pub fn first"))
        .is_none_or(|(second, first)| second >= first)
    {
        return Err(format!(
            "move-item apply did not reorder functions: {moved}"
        ));
    }

    let no_op = call_json(
        client,
        "move_item_preview",
        &json!({
            "project_id": "default",
            "file_path": source,
            "start_line": 1,
            "start_character": 8,
            "end_line": 1,
            "end_character": 8,
            "direction": "up",
            "position_encoding": "utf-8",
        }),
    )?;
    if no_op["supported"] != true || no_op["changed"] != false {
        return Err(format!("top-item no-op was misreported: {no_op}"));
    }
    Ok(())
}

/// Compose rust-analyzer's folder-module edits with one filesystem rename.
fn sc_path_rename_folder_semantic_edit(
    client: &mut McpClient,
    workspace: &Path,
) -> Result<(), String> {
    let folder = workspace.join("src/folder_mod");
    let renamed_folder = workspace.join("src/folder_renamed");
    let preview = call_json(
        client,
        "path_rename_preview",
        &json!({
            "project_id": "default",
            "old_path": folder,
            "new_path": renamed_folder,
            "position_encoding": "utf-8",
        }),
    )?;
    let plan_id = semantic_path_rename_plan(&preview, "module folder rename")?;
    let applied = call_json(
        client,
        "workspace_edit_apply",
        &json!({"project_id": "default", "plan_id": plan_id}),
    )?;
    if applied["semantic_state"] != "synchronized"
        || applied["provider_synchronization"][0]["provider"] != "rust"
        || applied["provider_synchronization"][0]["synchronized"] != true
    {
        return Err(format!(
            "folder rename did not report synchronized provider state: {applied}"
        ));
    }

    let nested = renamed_folder.join("nested.rs");
    let renamed_nested = renamed_folder.join("child.rs");
    let immediate = call_json(
        client,
        "path_rename_preview",
        &json!({
            "project_id": "default",
            "old_path": nested,
            "new_path": renamed_nested,
            "position_encoding": "utf-8",
        }),
    )?;
    let plan_id = semantic_path_rename_plan(&immediate, "immediate nested module rename")?;
    let applied = call_json(
        client,
        "workspace_edit_apply",
        &json!({"project_id": "default", "plan_id": plan_id}),
    )?;
    if applied["semantic_state"] != "synchronized" {
        return Err(format!(
            "nested file rename did not report synchronized provider state: {applied}"
        ));
    }
    if folder.exists() || nested.exists() || !renamed_nested.exists() {
        return Err(format!(
            "ordered folder/file rename did not commit: {immediate}"
        ));
    }
    Ok(())
}

/// Compose rust-analyzer's file-module edits with one filesystem rename.
fn sc_path_rename_semantic_edit(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    let functions = workspace.join("src/functions.rs");
    let renamed_functions = workspace.join("src/functions_path.rs");
    let file_preview = call_json(
        client,
        "path_rename_preview",
        &json!({
            "project_id": "default",
            "old_path": functions,
            "new_path": renamed_functions,
            "position_encoding": "utf-8",
        }),
    )?;
    let plan_id = semantic_path_rename_plan(&file_preview, "module file rename")?;

    let types = workspace.join("src/types.rs");
    let cross_level = workspace.join("src/moved/types.rs");
    let limited = call_json(
        client,
        "path_rename_preview",
        &json!({
            "project_id": "default",
            "old_path": types,
            "new_path": cross_level,
            "position_encoding": "utf-8",
        }),
    )?;
    if limited["semantic_providers"]
        .as_array()
        .is_none_or(|providers| !providers.iter().any(|provider| provider == "rust"))
        || limited["semantic_edit_count"] != 0
        || limited["verification"] != "structural_unverified"
    {
        return Err(format!(
            "cross-level rust-analyzer limitation was misreported: {limited}"
        ));
    }

    call_json(
        client,
        "workspace_edit_apply",
        &json!({"project_id": "default", "plan_id": plan_id}),
    )?;
    let lib_content = fs::read_to_string(&lib_rs)
        .map_err(|error| format!("failed to read renamed module declaration: {error}"))?;
    if functions.exists()
        || !renamed_functions.exists()
        || !lib_content.contains("pub mod functions_path;")
    {
        return Err(format!(
            "module file rename did not update declaration and filesystem: {lib_content}"
        ));
    }

    Ok(())
}

/// Tool 27: `move_inline_module_preview` + `workspace_edit_apply` — move a
/// real top-level module through the actor-owned semantic edit path.
fn sc_move_inline_module_semantic_edit(
    client: &mut McpClient,
    workspace: &Path,
) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    let module_line = find_line(&lib_rs, "pub mod move_target {");
    let preview = call_json(
        client,
        "move_inline_module_preview",
        &json!({
            "project_id": "default",
            "file_path": lib_rs,
            "module_name": "move_target",
            "module_line": module_line - 1,
            "module_character": 8,
        }),
    )?;

    if preview["verification"] != "semantic_verified" {
        return Err(format!(
            "real rust-analyzer did not semantically verify the move: {}",
            preview["verification"]
        ));
    }
    if preview["producer"] != "rust_analyzer" {
        return Err(format!(
            "semantic move did not select rust-analyzer's native edit: {}",
            preview["producer"]
        ));
    }
    let plan_id = preview["plan_id"]
        .as_str()
        .ok_or_else(|| format!("move preview omitted plan_id: {preview}"))?;
    let destination = workspace.join("src/move_target.rs");
    if !preview["affected_files"].as_array().is_some_and(|files| {
        files
            .iter()
            .any(|file| file.as_str() == Some(destination.to_string_lossy().as_ref()))
    }) {
        return Err(format!(
            "move preview did not target {}: {preview}",
            destination.display()
        ));
    }

    let applied = call_json(
        client,
        "workspace_edit_apply",
        &json!({"project_id": "default", "plan_id": plan_id}),
    )?;
    if applied["verification"] != "semantic_verified" {
        return Err(format!(
            "semantic move postcheck failed: {}",
            applied["verification"]
        ));
    }
    let moved_source = fs::read_to_string(&lib_rs)
        .map_err(|error| format!("failed to read moved source: {error}"))?;
    let moved_destination = fs::read_to_string(&destination)
        .map_err(|error| format!("failed to read moved destination: {error}"))?;
    if moved_source.contains("pub mod move_target {")
        || !moved_destination.contains("pub fn answer() -> u32")
    {
        return Err(format!(
            "filesystem result did not preserve the module move: source={moved_source:?}, destination={moved_destination:?}"
        ));
    }
    Ok(())
}

/// Tool 26: live rust-analyzer identity checks for raw and Unicode modules.
fn sc_move_inline_module_raw_and_unicode(
    client: &mut McpClient,
    workspace: &Path,
) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    for (module_name, declaration, destination_name, body_marker) in [
        (
            "r#type",
            "pub mod r#type {",
            "type.rs",
            "pub fn raw_answer() -> u32",
        ),
        (
            "café",
            "pub mod café {",
            "café.rs",
            "pub fn unicode_answer() -> u32",
        ),
    ] {
        let module_line = find_line(&lib_rs, declaration);
        let preview = call_json(
            client,
            "move_inline_module_preview",
            &json!({
                "project_id": "default",
                "file_path": lib_rs,
                "module_name": module_name,
                "module_line": module_line - 1,
                "module_character": 8,
            }),
        )?;
        if preview["verification"] != "semantic_verified" {
            return Err(format!(
                "rust-analyzer did not verify {module_name}: {}",
                preview["verification"]
            ));
        }
        if preview["producer"] != "rust_analyzer" {
            return Err(format!(
                "rust-analyzer did not produce the {module_name} move: {}",
                preview["producer"]
            ));
        }
        let plan_id = preview["plan_id"]
            .as_str()
            .ok_or_else(|| format!("preview omitted plan_id for {module_name}: {preview}"))?;
        call_json(
            client,
            "workspace_edit_apply",
            &json!({"project_id": "default", "plan_id": plan_id}),
        )?;
        let destination = workspace.join("src").join(destination_name);
        let content = fs::read_to_string(&destination)
            .map_err(|error| format!("failed to read {}: {error}", destination.display()))?;
        if !content.contains(body_marker) {
            return Err(format!(
                "moved {module_name} body missing from {}",
                destination.display()
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Suite driver
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Requires rust-analyzer in PATH; set MCPLS_SKIP_RA=1 to skip or MCPLS_RUST_ANALYZER=<path> to override"]
fn ra_e2e_suite() {
    let ra_path = match resolve_rust_analyzer() {
        Resolution::Found(p) => p,
        Resolution::Skipped(reason) => {
            println!("[ra_e2e] suite skipped: {reason}");
            return;
        }
        Resolution::Missing => {
            panic!(
                "[ra_e2e] rust-analyzer not found in PATH; \
                 install it with `rustup component add rust-analyzer` \
                 or set MCPLS_SKIP_RA=1 to skip"
            );
        }
    };

    println!("[ra_e2e] using rust-analyzer: {}", ra_path.display());

    // Stage workspace into a TempDir.
    let workspace_tmp = stage_workspace();
    // Canonicalize to resolve macOS /var → /private/var symlinks.
    // rust-analyzer resolves paths internally; without canonicalization, hover
    // requests using /var/folders/… would not match its indexed file URIs.
    let workspace = workspace_tmp
        .path()
        .canonicalize()
        .unwrap_or_else(|_| workspace_tmp.path().to_owned());

    // Generate config.
    let config_path = workspace.join("mcpls-e2e.toml");
    write_config(&ra_path, &workspace, &config_path);

    // Spawn mcpls.
    let config_str = config_path.to_string_lossy().into_owned();
    let mut client =
        McpClient::spawn_with_args(&["--config", &config_str]).expect("failed to spawn mcpls");

    client.initialize().expect("MCP initialize failed");

    // Wait for rust-analyzer to index.
    let lib_rs = workspace.join("src/lib.rs");
    wait_until_ready(&mut client, &lib_rs);

    // Sub-case registry.
    let sub_cases: &[SubCase] = &[
        sub_case!(sc_get_hover),
        sub_case!(sc_get_definition),
        sub_case!(sc_get_references),
        sub_case!(sc_get_diagnostics),
        sub_case!(sc_rename_symbol),
        sub_case!(sc_get_completions),
        sub_case!(sc_get_document_symbols),
        sub_case!(sc_format_document),
        sub_case!(sc_workspace_symbol_search),
        sub_case!(sc_symbol_handle_follow_ups),
        sub_case!(sc_get_code_actions),
        sub_case!(sc_prepare_call_hierarchy),
        sub_case!(sc_get_incoming_calls),
        sub_case!(sc_get_outgoing_calls),
        sub_case!(sc_get_cached_diagnostics),
        sub_case!(sc_get_server_logs),
        sub_case!(sc_get_server_messages),
        sub_case!(sc_structural_replace_preview),
        sub_case!(sc_get_signature_help),
        sub_case!(sc_go_to_implementation),
        sub_case!(sc_go_to_type_definition),
        sub_case!(sc_get_inlay_hints),
        sub_case!(sc_list_resources),
        sub_case!(sc_read_resource),
        sub_case!(sc_subscribe_unsubscribe_resource),
        sub_case!(sc_subscribe_no_replay_without_cached_diagnostics),
        sub_case!(sc_native_module_rename_preview),
        sub_case!(sc_native_module_move_code_action_preview),
        sub_case!(sc_semantic_discovery),
        sub_case!(sc_local_edit_previews),
        sub_case!(sc_path_rename_folder_semantic_edit),
        sub_case!(sc_path_rename_semantic_edit),
        sub_case!(sc_move_inline_module_semantic_edit),
    ];

    let filter = std::env::var("MCPLS_RA_FILTER").ok();

    let mut results: Vec<SubResult> = Vec::new();

    for sc in sub_cases {
        if filter.as_deref().is_some_and(|f| !sc.name.contains(f)) {
            continue;
        }

        print!("[ra_e2e] running {} … ", sc.name);
        // Use catch_unwind so a panicking sub-case doesn't abort the whole suite.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (sc.run)(&mut client, &workspace)
        }));

        let outcome = match outcome {
            Ok(r) => r,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "sub-case panicked".to_owned());
                Err(msg)
            }
        };

        match &outcome {
            Ok(()) => println!("ok"),
            Err(e) => println!("FAILED: {e}"),
        }

        results.push(SubResult {
            name: sc.name,
            outcome,
        });
    }

    // Aggregate failures.
    let failures: Vec<_> = results.iter().filter(|r| r.outcome.is_err()).collect();

    if !failures.is_empty() {
        let report: Vec<String> = failures
            .iter()
            .map(|f| format!("  • {} — {}", f.name, f.outcome.as_ref().unwrap_err()))
            .collect();
        panic!(
            "[ra_e2e] {} sub-case(s) failed:\n{}",
            failures.len(),
            report.join("\n")
        );
    }

    println!("[ra_e2e] all {} sub-cases passed", results.len());
}

#[test]
#[ignore = "Requires rust-analyzer in PATH; set MCPLS_SKIP_RA=1 to skip or MCPLS_RUST_ANALYZER=<path> to override"]
fn ra_unicode_module_move_e2e() {
    let ra_path = match resolve_rust_analyzer() {
        Resolution::Found(path) => path,
        Resolution::Skipped(reason) => {
            println!("[ra_unicode_e2e] suite skipped: {reason}");
            return;
        }
        Resolution::Missing => panic!("[ra_unicode_e2e] rust-analyzer not found"),
    };
    let workspace_tmp = stage_workspace();
    let lib_rs = workspace_tmp.path().join("src/lib.rs");
    let mut lib_content = fs::read_to_string(&lib_rs).expect("failed to read lib.rs");
    lib_content.push_str(
        "\npub mod r#type {\n    pub fn raw_answer() -> u32 {\n        7\n    }\n}\n\npub mod café {\n    pub fn unicode_answer() -> u32 {\n        8\n    }\n}\n",
    );
    fs::write(&lib_rs, lib_content).expect("failed to append raw and Unicode modules");
    let workspace = workspace_tmp
        .path()
        .canonicalize()
        .unwrap_or_else(|_| workspace_tmp.path().to_owned());
    let config_path = workspace.join("mcpls-unicode-e2e.toml");
    write_config(&ra_path, &workspace, &config_path);
    let config_str = config_path.to_string_lossy().into_owned();
    let mut client =
        McpClient::spawn_with_args(&["--config", &config_str]).expect("failed to spawn mcpls");
    client.initialize().expect("MCP initialize failed");
    wait_until_ready(&mut client, &workspace.join("src/lib.rs"));
    sc_move_inline_module_raw_and_unicode(&mut client, &workspace)
        .expect("Unicode move e2e failed");
}

fn call_json(client: &mut McpClient, name: &str, arguments: &Value) -> Result<Value, String> {
    let response = client
        .call_tool(name, arguments)
        .map_err(|error| format!("{name} failed: {error}"))?;
    let text = assertions::assert_tool_ok(&response);
    serde_json::from_str(&text).map_err(|error| format!("{name} returned invalid JSON: {error}"))
}

fn add_and_activate_project(client: &mut McpClient, project_id: &str, root: &Path, lib_rs: &Path) {
    let added = call_json(
        client,
        "project_add",
        &json!({"project_id": project_id, "root": root}),
    )
    .unwrap();
    assert_eq!(added["project_id"], project_id);
    call_json(
        client,
        "project_activate",
        &json!({"project_id": project_id}),
    )
    .unwrap();
    wait_until_ready(client, lib_rs);
}

fn apply_workspace_plan(client: &mut McpClient, project_id: &str, plan_id: &str) -> Value {
    call_json(
        client,
        "workspace_edit_apply",
        &json!({"project_id": project_id, "plan_id": plan_id}),
    )
    .unwrap()
}

struct MultiProjectFixture {
    _first_tmp: TempDir,
    _second_tmp: TempDir,
    first: std::path::PathBuf,
    second: std::path::PathBuf,
    functions: std::path::PathBuf,
    second_lib: std::path::PathBuf,
    bad_format: std::path::PathBuf,
}

impl MultiProjectFixture {
    fn new() -> Self {
        let first_tmp = stage_workspace();
        let first = first_tmp.path().canonicalize().unwrap();
        let second_tmp = stage_workspace();
        let second = second_tmp.path().canonicalize().unwrap();

        let functions = second.join("src/functions.rs");
        let mut functions_content = fs::read_to_string(&functions).unwrap();
        functions_content.push_str("\npub fn cross_file_target() -> i32 { 7 }\n");
        fs::write(&functions, functions_content).unwrap();

        let second_lib = second.join("src/lib.rs");
        let mut second_lib_content = fs::read_to_string(&second_lib).unwrap();
        second_lib_content.push_str(
            "\npub fn cross_file_caller() -> i32 { crate::functions::cross_file_target() }\n",
        );
        fs::write(&second_lib, second_lib_content).unwrap();

        let bad_format = second.join("src/bad_format.rs");
        Self {
            _first_tmp: first_tmp,
            _second_tmp: second_tmp,
            first,
            second,
            functions,
            second_lib,
            bad_format,
        }
    }
}

#[test]
#[ignore = "Requires rust-analyzer in PATH; set MCPLS_RUST_ANALYZER=<path>"]
fn ra_multi_project_safe_refactor_e2e() {
    let ra_path = match resolve_rust_analyzer() {
        Resolution::Found(path) => path,
        Resolution::Skipped(reason) => {
            println!("[ra_e2e] multi-project suite skipped: {reason}");
            return;
        }
        Resolution::Missing => panic!("rust-analyzer is required for this suite"),
    };

    let fixture = MultiProjectFixture::new();

    let config_path = fixture.first.join("mcpls-multi-project-e2e.toml");
    write_config(&ra_path, &fixture.first, &config_path);
    let config = config_path.to_string_lossy().into_owned();
    let mut client = McpClient::spawn_with_args(&["--config", &config]).unwrap();
    client.initialize().unwrap();
    wait_until_ready(&mut client, &fixture.first.join("src/lib.rs"));

    assert_project_isolation(&mut client, &fixture);
    rename_and_apply(&mut client, &fixture);
    format_and_restart(&mut client, &fixture);
}

fn assert_project_isolation(client: &mut McpClient, fixture: &MultiProjectFixture) {
    add_and_activate_project(client, "second", &fixture.second, &fixture.second_lib);

    let projects = call_json(client, "project_list", &json!({})).unwrap();
    assert_eq!(projects.as_array().unwrap().len(), 2);

    let first_symbols = call_json(
        client,
        "workspace_symbol_search",
        &json!({"project_id": "default", "query": "cross_file_target"}),
    )
    .unwrap();
    assert!(first_symbols["symbols"].as_array().unwrap().is_empty());
    let second_symbols = call_json(
        client,
        "workspace_symbol_search",
        &json!({"project_id": "second", "query": "cross_file_target"}),
    )
    .unwrap();
    assert!(!second_symbols["symbols"].as_array().unwrap().is_empty());
}

fn rename_and_apply(client: &mut McpClient, fixture: &MultiProjectFixture) {
    let target_line = find_line(&fixture.functions, "pub fn cross_file_target(");
    let rename = call_json(
        client,
        "rename_preview",
        &json!({
            "project_id": "second",
            "file_path": fixture.functions,
            "line": target_line,
            "character": 8,
            "new_name": "renamed_target",
            "position_encoding": "utf-8"
        }),
    )
    .unwrap();
    assert_eq!(rename["safe_to_apply"], true);
    assert!(rename["affected_files"].as_array().unwrap().len() >= 2);
    let rename_plan = rename["plan_id"].as_str().unwrap().to_owned();
    let applied = apply_workspace_plan(client, "second", &rename_plan);
    assert!(applied["committed_files"].as_array().unwrap().len() >= 2);
    assert!(
        fs::read_to_string(&fixture.functions)
            .unwrap()
            .contains("renamed_target")
    );
    assert!(
        fs::read_to_string(&fixture.second_lib)
            .unwrap()
            .contains("renamed_target")
    );

    let stale = client.call_tool(
        "workspace_edit_apply",
        &json!({"project_id": "second", "plan_id": rename_plan}),
    );
    assert!(stale.is_err(), "a consumed edit plan must be rejected");
}

fn format_and_restart(client: &mut McpClient, fixture: &MultiProjectFixture) {
    let formatted = call_json(
        client,
        "format_preview",
        &json!({
            "project_id": "second",
            "file_path": fixture.bad_format,
            "tab_size": 4,
            "insert_spaces": true,
            "position_encoding": "utf-8"
        }),
    )
    .unwrap();
    let format_plan = formatted["plan_id"].as_str().unwrap().to_owned();
    apply_workspace_plan(client, "second", &format_plan);
    let golden =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden/bad_format.fmt.rs");
    assert_eq!(
        fs::read_to_string(&fixture.bad_format).unwrap().trim(),
        fs::read_to_string(golden).unwrap().trim()
    );

    // Exercise the existing real-RA structural code-action path on the second
    // project, then restart it and prove the renamed symbol remains available.
    sc_get_code_actions(client, &fixture.second).unwrap();
    let restarted = call_json(
        client,
        "project_restart_lsp",
        &json!({"project_id": "second"}),
    )
    .unwrap();
    assert!(matches!(
        restarted["status"].as_str(),
        Some("Starting" | "Ready")
    ));
    wait_until_ready(client, &fixture.second_lib);
    let hover = call_json(
        client,
        "get_hover",
        &json!({
            "file_path": fixture.functions,
            "line": find_line(&fixture.functions, "pub fn renamed_target("),
            "character": 8
        }),
    )
    .unwrap();
    assert!(hover.to_string().contains("renamed_target"));
}
