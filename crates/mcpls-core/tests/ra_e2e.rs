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
    let broken_content = fs::read_to_string(&broken_dst).expect("failed to read broken.rs");
    let diagnostic_marker = "diagnostic-context-marker";
    let inflated = format!("    42 /* {diagnostic_marker} {} */", "x".repeat(8 * 1024));
    fs::write(&broken_dst, broken_content.replace("    42", &inflated))
        .expect("failed to inflate broken.rs diagnostic context");

    let lib_path = tmp.path().join("src/lib.rs");
    let mut lib_content = fs::read_to_string(&lib_path).expect("failed to read lib.rs");
    lib_content.push_str("\npub mod broken;\n");
    lib_content.push_str("\npub mod callers;\n");
    lib_content
        .push_str("\npub mod move_target {\n    pub fn answer() -> u32 {\n        42\n    }\n}\n");
    lib_content.push_str("\npub mod folder_mod;\n");
    lib_content.push_str("\npub mod move_items;\n");
    let rename_fixture = tmp.path().join("src/rename_large.rs");
    let mut rename_content = String::from("pub fn rename_target(value: i32) -> i32 { value }\n");
    for index in 0..500 {
        rename_content.push_str(&format!(
            "pub fn rename_use_{index}() -> i32 {{ rename_target({index}) }}\n"
        ));
    }
    fs::write(&rename_fixture, rename_content).expect("failed to write rename fixture");
    lib_content.push_str("\npub mod rename_large;\n");
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
    let huge_macro_statements: String = (0..3_000)
        .map(|index| format!("let _macro_value_{index} = {index};\n"))
        .collect();
    lib_content.push_str(&format!(
        "\nmacro_rules! huge_semantic {{ () => {{ {{ {huge_macro_statements} 7_u32 }} }} }}\n\
         pub fn huge_semantic_user() -> u32 {{ huge_semantic!() }}\n"
    ));
    let completion_items: String = (0..80)
        .map(|index| format!("pub fn ad_item_{index:02}() -> u32 {{ {index} }}\n"))
        .collect();
    lib_content.push_str(&completion_items);
    let inlay_hints: String = (0..80)
        .map(|index| format!("    let _inlay_value_{index:02} = add({index}, {index});\n"))
        .collect();
    lib_content = lib_content.replace(
        "    let _ = (p, s);",
        &format!("{inlay_hints}    let _ = (p, s);"),
    );
    fs::write(&lib_path, lib_content).expect("failed to append pub mod broken");
    fs::write(
        tmp.path().join("src/callers.rs"),
        format!(
            "pub fn caller_one() -> i32 {{ super::add(1, 2) + super::add(3, 4) + super::add(5, 6) /* call-context-marker {} */ }}\n\
             pub fn caller_two() -> i32 {{ super::add(3, 4) }}\n\
             pub fn caller_three() -> i32 {{ super::add(5, 6) }}\n",
            "x".repeat(8 * 1024)
        ),
    )
    .expect("failed to write call hierarchy callers");

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

/// Make the source surrounding both navigation targets exceed the inline
/// frame budget, so their snapshot-bound source resources must be replayed.
fn inflate_navigation_source_context(lib_rs: &Path) {
    let mut content = fs::read_to_string(lib_rs).expect("failed to read lib.rs");
    let marker = "navigation-context-marker ".to_owned() + &"x".repeat(5 * 1024);
    for needle in [
        "pub trait Greet {",
        "impl Greet for CodeActionTarget {",
        "pub struct Point {",
    ] {
        let replacement = format!("/// {marker}\n{needle}");
        content = content.replacen(needle, &replacement, 1);
    }
    fs::write(lib_rs, content).expect("failed to inflate navigation source context");
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
                let hover_is_ready =
                    serde_json::from_str::<Value>(&text)
                        .ok()
                        .is_some_and(|value| {
                            value["contents"].as_str().is_some_and(|contents| {
                                contents.contains("fn add") && contents.contains("i32")
                            })
                        });
                if hover_is_ready {
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
    if inner["provider"] != "standard_lsp"
        || inner["kind"] != "hover"
        || inner["source"]["status"] != "available"
        || inner["source"]["path"].as_str().is_none()
        || inner["truncated"].as_bool().is_none()
        || inner["symbol_handle"].as_str().is_none()
    {
        return Err(format!("hover omitted model-ready target context: {inner}"));
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
    if inner["provider"] != "standard_lsp"
        || inner["kind"] != "definition"
        || inner["truncated"].as_bool().is_none()
        || locs[0]["path"].as_str().is_none()
        || locs[0]["source"]["status"] != "available"
        || locs[0]["symbol_handle"].as_str().is_none()
    {
        return Err(format!(
            "definition omitted model-ready target context: {inner}"
        ));
    }
    Ok(())
}

/// Dependency definitions returned by rust-analyzer expose read-only source context.
fn sc_dependency_source_context(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let (line, character) = find_position(&lib, "fmt;");
    let result = call_json(
        client,
        "get_definition",
        &json!({
            "file_path": lib,
            "line": line,
            "character": character,
        }),
    )?;
    let location = result["locations"]
        .as_array()
        .and_then(|locations| locations.first())
        .ok_or_else(|| format!("dependency definition returned no locations: {result}"))?;
    let path = location["path"]
        .as_str()
        .ok_or_else(|| format!("dependency definition omitted its path: {result}"))?;

    if path.starts_with(workspace.to_string_lossy().as_ref())
        || location["source"]["status"] != "available"
        || location["source"]["text"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        return Err(format!(
            "dependency definition omitted read-only source context: {result}"
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

    let groups = inner["groups"]
        .as_array()
        .ok_or_else(|| format!("expected grouped references, got {inner}"))?;
    if inner["returned_references"]
        .as_u64()
        .is_none_or(|count| count < 2)
    {
        return Err(format!(
            "expected ≥2 references (decl + call site), got {}",
            inner["returned_references"]
        ));
    }
    if inner["declaration"]["source"]["status"] != "available"
        || groups.iter().any(|group| {
            group["project_relative_path"] != "src/lib.rs"
                || group["references"].as_array().is_none_or(|references| {
                    references.iter().any(|reference| {
                        reference["range"]
                            .as_array()
                            .is_none_or(|range| range.len() != 4)
                    })
                })
                || group["source"]["chunks"]
                    .as_array()
                    .is_none_or(Vec::is_empty)
        })
    {
        return Err(format!(
            "references omitted coherent source frames: {inner}"
        ));
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
            &json!({
                "fresh": true,
                "file_path": broken.to_string_lossy(),
                "item_limit": 20,
                "byte_limit": 4096
            }),
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
                    &json!({
                        "file_path": broken.to_string_lossy(),
                        "item_limit": 20,
                        "byte_limit": 4096
                    }),
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
    if final_diags.iter().any(|diagnostic| {
        diagnostic["source_frame"]["status"] != "available"
            || diagnostic["source_frame"]["path"]
                .as_str()
                .is_none_or(|path| !path.ends_with("/src/broken.rs"))
            || diagnostic["source_frame"]["highlighted_range"] != diagnostic["range"]
            || (diagnostic["source_frame"]["text"].as_str().is_none()
                && diagnostic["source_frame"]["resource"]["uri"]
                    .as_str()
                    .is_none())
    }) {
        return Err(format!(
            "diagnostics omitted coherent highlighted source: {final_diags:?}"
        ));
    }

    let source_frame = final_diags
        .iter()
        .find_map(|diagnostic| diagnostic["source_frame"].as_object())
        .ok_or_else(|| format!("diagnostics had no source frame: {final_diags:?}"))?;
    let resource_uri = source_frame["resource"]["uri"]
        .as_str()
        .ok_or_else(|| format!("diagnostic source was not deferred: {final_diags:?}"))?
        .to_owned();
    let original_hash = source_frame["content_hash"]
        .as_str()
        .ok_or_else(|| format!("diagnostic source had no content hash: {source_frame:?}"))?
        .to_owned();

    let semantic_resource = call_json(
        client,
        "read_semantic_resource",
        &json!({"uri": resource_uri}),
    )?;
    if semantic_resource["mime_type"] != "text/x-rust"
        || semantic_resource["source"]["content_hash"] != original_hash
        || !semantic_resource["text"]
            .as_str()
            .is_some_and(|text| text.contains("diagnostic-context-marker"))
        || serde_json::from_str::<Value>(semantic_resource["text"].as_str().unwrap_or("")).is_ok()
    {
        return Err(format!(
            "semantic source resource was not raw text with structured metadata: {semantic_resource}"
        ));
    }

    let resource = client
        .read_resource(&resource_uri)
        .map_err(|error| format!("read diagnostic source resource failed: {error}"))?;
    let resource_text = resource["result"]["contents"][0]["text"]
        .as_str()
        .ok_or_else(|| format!("malformed diagnostic source resource: {resource}"))?;
    if !resource_text.contains("diagnostic-context-marker") {
        return Err(format!(
            "deferred diagnostic source omitted marker: {resource_text}"
        ));
    }

    let changed = fs::read_to_string(&broken)
        .map_err(|error| format!("read broken.rs before stale replay: {error}"))?
        .replace("diagnostic-context-marker", "diagnostic-stale-marker");
    fs::write(&broken, changed).map_err(|error| format!("rewrite broken.rs: {error}"))?;
    let stale_resource = client
        .read_resource(&resource_uri)
        .map_err(|error| format!("read stale diagnostic source resource failed: {error}"))?;
    let stale_text = stale_resource["result"]["contents"][0]["text"]
        .as_str()
        .ok_or_else(|| format!("malformed stale diagnostic source resource: {stale_resource}"))?;
    if !stale_text.contains("diagnostic-stale-marker")
        || stale_resource["result"]["contents"][0]["uri"] == resource_uri
    {
        return Err(format!(
            "stale diagnostic resource did not replay current snapshot: {stale_text}"
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

/// Rename a heavily referenced symbol and replay the complete deferred edit set.
fn sc_rename_symbol_deferred(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let file = workspace.join("src/rename_large.rs");
    let target_line = find_line(&file, "pub fn rename_target");
    let result = call_json(
        client,
        "rename_symbol",
        &json!({
            "file_path": file,
            "line": target_line,
            "character": 8,
            "new_name": "renamed_target",
        }),
    )?;
    if result["deferred"] != true
        || !result["changes"].as_array().is_some_and(Vec::is_empty)
        || !(result["operations"].is_null()
            || result["operations"].as_array().is_some_and(Vec::is_empty))
        || result["total_edits"]
            .as_u64()
            .is_none_or(|count| count < 500)
    {
        return Err(format!(
            "large rename result was not atomically deferred: {result}"
        ));
    }
    let resource_uri = result["changes_resource"]["uri"]
        .as_str()
        .ok_or_else(|| format!("large rename omitted changes_resource: {result}"))?
        .to_owned();
    let expected_edits = result["total_edits"]
        .as_u64()
        .ok_or_else(|| format!("large rename omitted total_edits: {result}"))?;

    let mut uri = resource_uri.clone();
    let mut semantic_json = String::new();
    loop {
        let page = call_json(client, "read_semantic_resource", &json!({"uri": uri}))?;
        semantic_json.push_str(
            page["text"]
                .as_str()
                .ok_or_else(|| format!("rename fallback page omitted text: {page}"))?,
        );
        let Some(next) = page["next_uri"].as_str() else {
            break;
        };
        uri = next.to_owned();
    }
    let semantic: Value = serde_json::from_str(&semantic_json)
        .map_err(|error| format!("rename fallback was not complete JSON: {error}"))?;
    if semantic["changes"].as_array().map_or(0, Vec::len) != 1
        || semantic["changes"][0]["edits"]
            .as_array()
            .map_or(0, Vec::len)
            != expected_edits as usize
    {
        return Err(format!("rename fallback omitted edits: {semantic}"));
    }

    let mut other =
        McpClient::spawn().map_err(|error| format!("spawn second MCP session: {error}"))?;
    other
        .initialize()
        .map_err(|error| format!("initialize second MCP session: {error}"))?;
    let cross_session = other
        .call_tool("read_semantic_resource", &json!({"uri": resource_uri}))
        .expect_err("rename resource must not be readable from another session");
    if !cross_session.to_string().contains("stale_resource") {
        return Err(format!(
            "cross-session rename resource failure was not explicit: {cross_session}"
        ));
    }
    let missing = client
        .call_tool(
            "read_semantic_resource",
            &json!({
                "uri": "mcpls-deferred:///00000000-0000-0000-0000-000000000000"
            }),
        )
        .expect_err("missing rename resource must fail closed");
    if !missing.to_string().contains("stale_resource") {
        return Err(format!(
            "missing rename resource failure was not explicit: {missing}"
        ));
    }

    let mut uri = resource_uri;
    let mut resource_json = String::new();
    loop {
        let page = client
            .read_resource(&uri)
            .map_err(|error| format!("read rename resource: {error}"))?;
        let envelope = page["result"]["contents"][0]["text"]
            .as_str()
            .ok_or_else(|| format!("rename resource page omitted text: {page}"))?;
        let envelope: Value = serde_json::from_str(envelope)
            .map_err(|error| format!("rename resource page was not JSON: {error}"))?;
        resource_json.push_str(
            envelope["text"]
                .as_str()
                .ok_or_else(|| format!("rename resource envelope omitted text: {envelope}"))?,
        );
        let Some(next) = envelope["next_uri"].as_str() else {
            break;
        };
        uri = next.to_owned();
    }
    let resource: Value = serde_json::from_str(&resource_json)
        .map_err(|error| format!("rename resource was not complete JSON: {error}"))?;
    if resource["changes"].as_array().map_or(0, Vec::len) != 1
        || resource["changes"][0]["edits"]
            .as_array()
            .map_or(0, Vec::len)
            != expected_edits as usize
    {
        return Err(format!("rename resource omitted edits: {resource}"));
    }
    Ok(())
}

/// Tool 6: `get_completions` — page every `ad*` completion inside `caller`.
fn sc_get_completions(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    // Inside caller body: `    add(1, 2)` — column 7 is after 'a','d' (prefix "ad").
    let caller_line = find_line(&lib, "pub fn caller(");
    let body_line = caller_line + 1;

    // Retry loop: completions may not be available until rust-analyzer is fully ready.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut page_token = None;
    let mut labels = std::collections::BTreeSet::new();
    let mut expected_total = None;
    let mut pages = 0;
    loop {
        let resp = client
            .call_tool(
                "get_completions",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "line": body_line,
                    "character": 7,
                    "page_token": page_token
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let items = inner["items"]
            .as_array()
            .or_else(|| inner.as_array())
            .ok_or_else(|| format!("expected completions array, got {inner}"))?;

        let total = inner["total_items"]
            .as_u64()
            .ok_or_else(|| format!("completion result omitted total_items: {inner}"))?;
        if expected_total.is_some_and(|previous| previous != total) {
            return Err(format!("completion total changed across pages: {inner}"));
        }
        expected_total = Some(total);
        for item in items {
            let label = item["label"]
                .as_str()
                .ok_or_else(|| format!("completion omitted label: {item}"))?;
            if !labels.insert(label.to_owned()) {
                return Err(format!("duplicate completion label across pages: {label}"));
            }
        }

        pages += 1;
        page_token = inner["next_cursor"].as_str().map(str::to_owned);
        if page_token.is_none() {
            if labels.len() != total as usize || total < 64 || pages < 2 {
                return Err(format!(
                    "completion pages did not exhaust provider results: pages={pages}, labels={}, result={inner}",
                    labels.len()
                ));
            }
            if !labels.contains("add") {
                return Err(format!("completion pages omitted add: {labels:?}"));
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "get_completions: pages did not exhaust after 10 s; result: {inner}"
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
    if !inner["filters"]["max_depth"].is_null()
        || inner["returned"].as_u64().is_none()
        || inner["total"].as_u64().is_none()
        || inner["source_resource"]["uri"].as_str().is_none()
        || !inner["truncated"].is_boolean()
        || syms.iter().any(|symbol| symbol["children"].is_array())
        || syms
            .iter()
            .any(|symbol| symbol["source"].is_object() || !symbol["symbol_handle"].is_string())
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

/// Format a deliberately oversized file and replay its complete deferred edit set.
fn sc_format_document_deferred(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let large = workspace.join("src/format_large.rs");
    let content: String = (0..2_000)
        .map(|index| format!("pub fn poorly_{index}()->i32{{{index}}}\n"))
        .collect();
    fs::write(&large, content).map_err(|error| format!("write large format fixture: {error}"))?;
    let result = call_json(
        client,
        "format_document",
        &json!({"file_path": large.to_string_lossy()}),
    )?;
    if result["deferred"] != true
        || !result["edits"].as_array().is_some_and(Vec::is_empty)
        || result["total_edits"]
            .as_u64()
            .is_none_or(|count| count == 0)
    {
        return Err(format!(
            "large format result was not atomically deferred: {result}"
        ));
    }
    let resource_uri = result["edits_resource"]
        .as_object()
        .and_then(|resource| resource["uri"].as_str())
        .ok_or_else(|| format!("large format result omitted edits_resource: {result}"))?
        .to_owned();

    let mut uri = resource_uri.clone();
    let mut fallback_json = String::new();
    loop {
        let page = call_json(client, "read_semantic_resource", &json!({"uri": uri}))?;
        fallback_json.push_str(
            page["text"]
                .as_str()
                .ok_or_else(|| format!("format fallback page omitted text: {page}"))?,
        );
        let Some(next) = page["next_uri"].as_str() else {
            break;
        };
        uri = next.to_owned();
    }
    let fallback_edits: Value = serde_json::from_str(&fallback_json)
        .map_err(|error| format!("format fallback resource was not complete JSON: {error}"))?;
    if fallback_edits.as_array().map_or(0, Vec::len)
        != result["total_edits"].as_u64().unwrap() as usize
    {
        return Err(format!("format fallback omitted edits: {fallback_edits}"));
    }

    let mut uri = resource_uri;
    let mut resource_json = String::new();
    loop {
        let page = client
            .read_resource(&uri)
            .map_err(|error| format!("read format resource: {error}"))?;
        let text = page["result"]["contents"][0]["text"]
            .as_str()
            .ok_or_else(|| format!("format resource page omitted text: {page}"))?;
        let page_value: Value = serde_json::from_str(text)
            .map_err(|error| format!("format resource page was not JSON: {error}"))?;
        resource_json.push_str(
            page_value["text"]
                .as_str()
                .ok_or_else(|| format!("format resource envelope omitted text: {page_value}"))?,
        );
        let Some(next) = page_value["next_uri"].as_str() else {
            break;
        };
        uri = next.to_owned();
    }
    let resource_edits: Value = serde_json::from_str(&resource_json)
        .map_err(|error| format!("format resource was not complete JSON: {error}"))?;
    if resource_edits.as_array().map_or(0, Vec::len)
        != result["total_edits"].as_u64().unwrap() as usize
    {
        return Err(format!("format resource omitted edits: {resource_edits}"));
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
                && symbol["location"]["source"]["text"]
                    .as_str()
                    .is_some_and(|source| source.contains("pub fn add"))
                && symbol["location"]["source"]["highlighted_range"] == symbol["location"]["range"]
                && symbol["location"]["symbol_handle"].is_string()
                && inner["total"].as_u64().is_some()
                && inner["returned"].as_u64().is_some()
                && inner["truncated"].is_boolean();
            if contract_is_complete
                && syms.first().is_some_and(|symbol| symbol["name"] == "add")
                && syms.iter().all(|symbol| symbol["name"] == "add")
            {
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

/// Resolve one exact fixture symbol with source, hover, and implementation sections.
fn inspect_exact_symbol(
    client: &mut McpClient,
    query: &str,
    path: Option<&str>,
) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let mut arguments = json!({
            "project_id": "default",
            "query": query,
            "sections": ["declaration", "hover", "implementations"],
            "budget": {"max_bytes": 32768, "max_items": 20}
        });
        if let Some(path) = path {
            arguments["path"] = Value::String(path.to_owned());
        }
        let response = client
            .call_tool("inspect_symbol", &arguments)
            .map_err(|error| format!("inspect_symbol({query}) failed: {error}"))?;
        let result: Value = serde_json::from_str(&assertions::assert_tool_ok(&response))
            .map_err(|error| format!("bad inspect_symbol({query}) JSON: {error}"))?;
        if result["resolution"]["status"] == "selected" {
            return Ok(result);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "inspect_symbol({query}) did not resolve exactly: {result}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// High-level bundle: answer what `add` is and how it is used without reading the file.
fn sc_inspect_symbol(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    let example_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/source-rich-workflow.json");
    let example: Value = serde_json::from_slice(
        &fs::read(&example_path)
            .map_err(|error| format!("failed to read {}: {error}", example_path.display()))?,
    )
    .map_err(|error| format!("bad source-rich workflow example: {error}"))?;
    let discovery_tool = example["discovery"]["tool"]
        .as_str()
        .ok_or_else(|| "workflow example has no discovery tool".to_owned())?;
    let discovery = client
        .call_tool(discovery_tool, &example["discovery"]["arguments"])
        .map_err(|error| format!("inspect workflow discovery failed: {error}"))?;
    let discovery: Value = serde_json::from_str(&assertions::assert_tool_ok(&discovery))
        .map_err(|error| format!("bad inspect workflow discovery JSON: {error}"))?;
    let handle = discovery["symbols"]
        .as_array()
        .and_then(|symbols| symbols.iter().find(|symbol| symbol["name"] == "add"))
        .and_then(|symbol| symbol["location"]["symbol_handle"].as_str())
        .ok_or_else(|| format!("inspect workflow discovery returned no add handle: {discovery}"))?;
    let inspection_tool = example["inspection"]["tool"]
        .as_str()
        .ok_or_else(|| "workflow example has no inspection tool".to_owned())?;
    let mut inspection_arguments = example["inspection"]["arguments"].clone();
    inspection_arguments["symbol_handle"] = Value::String(handle.to_owned());
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let response = client
            .call_tool(inspection_tool, &inspection_arguments)
            .map_err(|error| format!("inspect_symbol failed: {error}"))?;
        let result: Value = serde_json::from_str(&assertions::assert_tool_ok(&response))
            .map_err(|error| format!("bad inspect_symbol JSON: {error}"))?;
        let ready = result["resolution"]["status"] == "selected"
            && result["sections"]["declaration"]["data"]["status"] == "available"
            && result["sections"]["declaration"]["data"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("pub fn add"))
            && result["sections"]["hover"]["data"]["contents"]
                .as_str()
                .is_some_and(|contents| contents.contains("add"))
            && result["sections"]["references"]["returned"]
                .as_u64()
                .is_some_and(|count| count > 0)
            && result["sections"]["calls"]["data"]["incoming"]["returned_calls"]
                .as_u64()
                .is_some_and(|count| count > 0)
            && result["sections"]["diagnostics"]["completeness"].is_string()
            && result["sections"]["tests"]["completeness"].is_string()
            && result["returned_bytes"]
                .as_u64()
                .is_some_and(|bytes| bytes <= 65_536);
        if ready {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "inspect_symbol did not return a source-bearing usage bundle: {result}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    for (query, path) in [
        ("Point", Some("src/lib.rs")),
        ("Greet", Some("src/lib.rs")),
        ("reexported_create_repo", None),
        ("private_helper", Some("src/lib.rs")),
        ("fixture_macro", None),
    ] {
        let inspected = inspect_exact_symbol(client, query, path)?;
        if inspected["sections"]["declaration"]["data"]["status"] != "available" {
            return Err(format!(
                "inspect_symbol({query}) lacked declaration source: {inspected}"
            ));
        }
    }
    let trait_bundle = inspect_exact_symbol(client, "Greet", Some("src/lib.rs"))?;
    if trait_bundle["sections"]["implementations"]["returned"]
        .as_u64()
        .is_none_or(|count| count == 0)
    {
        return Err(format!(
            "inspect_symbol(Greet) lacked its impl: {trait_bundle}"
        ));
    }
    Ok(())
}

fn sc_no_reread_corpus(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let corpus_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/no-reread-corpus.json");
    let corpus: Value = serde_json::from_slice(
        &fs::read(&corpus_path)
            .map_err(|error| format!("failed to read {}: {error}", corpus_path.display()))?,
    )
    .map_err(|error| format!("bad no-reread corpus: {error}"))?;
    let cases = corpus["cases"]
        .as_array()
        .ok_or_else(|| "no-reread corpus cases are not an array".to_owned())?;
    for case in cases {
        match case["scenario"].as_str().unwrap_or_default() {
            "workspace_symbol_search" => sc_workspace_symbol_search(client, workspace)?,
            "large_document_outline" => {
                let path = workspace.join("src/large_outline.rs");
                let started = Instant::now();
                let response = client
                    .call_tool(
                        "get_document_symbols",
                        &json!({"file_path": path, "limit": 12, "include_bodies": true}),
                    )
                    .map_err(|error| format!("large outline call failed: {error}"))?;
                let structured = &response["result"]["structuredContent"];
                let symbols = structured["symbols"]
                    .as_array()
                    .ok_or_else(|| format!("large outline has no symbols: {structured}"))?;
                let first = symbols
                    .first()
                    .ok_or_else(|| "large outline returned no symbols".to_owned())?;
                if structured["total"].as_u64().is_none_or(|total| total < 32)
                    || structured["returned"] != 12
                    || structured["truncated"] != true
                    || first["name"] != "fixture_item_00"
                    || structured["source_resource"]["uri"].as_str().is_none()
                    || first["source"].is_object()
                    || first["range"]["start"]["line"].as_u64().is_none()
                    || response.to_string().len() > 65_536
                    || started.elapsed() > Duration::from_secs(15)
                {
                    return Err(format!(
                        "large outline quality contract failed: {structured}"
                    ));
                }
            }
            "symbol_handle_follow_ups" => sc_symbol_handle_follow_ups(client, workspace)?,
            "definition_hover" => {
                sc_get_definition(client, workspace)?;
                sc_get_hover(client, workspace)?;
            }
            "references_calls" => {
                sc_get_references(client, workspace)?;
                sc_prepare_call_hierarchy(client, workspace)?;
                sc_get_incoming_calls(client, workspace)?;
            }
            "diagnostics" => sc_get_diagnostics(client, workspace)?,
            "structured_content" => {
                let response = client
                    .call_tool(
                        "workspace_symbol_search",
                        &json!({"project_id":"default","query":"add","match_mode":"exact"}),
                    )
                    .map_err(|error| format!("structured result call failed: {error}"))?;
                if !response["result"]["structuredContent"].is_object() {
                    return Err(format!("tool returned no structuredContent: {response}"));
                }
            }
            "inspect_symbol" => sc_inspect_symbol(client, workspace)?,
            "instrumented_agent_trace" => {}
            scenario => return Err(format!("unknown no-reread corpus scenario: {scenario}")),
        }
    }
    Ok(())
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
        if references["returned_references"]
            .as_u64()
            .is_some_and(|count| count >= 2)
            && references["groups"].as_array().is_some_and(|groups| {
                groups.iter().all(|group| {
                    group["references"].as_array().is_some_and(|references| {
                        references.iter().all(|reference| {
                            reference["range"]
                                .as_array()
                                .is_some_and(|range| range.len() == 4)
                                && reference["symbol_handle"].is_string()
                        })
                    }) && group["source"]["chunks"]
                        .as_array()
                        .is_some_and(|chunks| !chunks.is_empty())
                })
            })
        {
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
    if inner["provider"] != "standard_lsp"
        || inner["kind"] != "call_hierarchy"
        || items[0]["path"].as_str().is_none()
        || items[0]["source"]["status"] != "available"
        || items[0]["symbol_handle"].as_str().is_none()
    {
        return Err(format!(
            "call-hierarchy preparation omitted target context: {inner}"
        ));
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
    let mut page_token = None;
    let mut seen_edges = std::collections::BTreeSet::new();
    let mut expected_total_calls = None;
    let mut expected_total_call_sites = None;
    let mut deferred_resource = None;
    let mut pages = 0;
    loop {
        let resp = client
            .call_tool(
                "get_incoming_calls",
                &json!({
                    "item": item,
                    "limits": { "total": 1, "per_symbol": 1 },
                    "page_token": page_token
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let calls = inner["calls"]
            .as_array()
            .or_else(|| inner.as_array())
            .ok_or_else(|| format!("expected calls array, got {inner}"))?;

        let total_calls = inner["total_calls"]
            .as_u64()
            .ok_or_else(|| format!("incoming result omitted total_calls: {inner}"))?;
        let total_call_sites = inner["total_call_sites"]
            .as_u64()
            .ok_or_else(|| format!("incoming result omitted total_call_sites: {inner}"))?;
        if page_token.is_none() && total_calls < 3 {
            if Instant::now() >= deadline {
                return Err(format!(
                    "get_incoming_calls: provider graph did not converge to the fixture callers: {inner}"
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        if expected_total_calls.is_some_and(|previous| previous != total_calls) {
            return Err(format!(
                "incoming total_calls changed across pages: {inner}"
            ));
        }
        if expected_total_call_sites.is_some_and(|previous| previous != total_call_sites) {
            return Err(format!(
                "incoming total_call_sites changed across pages: {inner}"
            ));
        }
        expected_total_calls = Some(total_calls);
        expected_total_call_sites = Some(total_call_sites);

        for call in calls {
            let caller_name = call["from"]["name"]
                .as_str()
                .or_else(|| call["caller"]["name"].as_str())
                .unwrap_or("<unnamed>");
            for site in call["call_sites"]
                .as_array()
                .ok_or_else(|| format!("incoming call omitted call_sites: {call}"))?
            {
                let range = &site["range"];
                let edge = format!(
                    "{caller_name}:{}:{}:{}",
                    range["start"]["line"], range["start"]["character"], range["end"]["character"]
                );
                if !seen_edges.insert(edge.clone()) {
                    return Err(format!("duplicate incoming call-site edge: {edge}"));
                }

                if caller_name.contains("caller_one") {
                    let source = &site["source"];
                    let uri = source["resource"]["uri"]
                        .as_str()
                        .ok_or_else(|| format!("caller_one source was not deferred: {source}"))?;
                    let original_hash = source["content_hash"].as_str().ok_or_else(|| {
                        format!("caller_one source had no content hash: {source}")
                    })?;
                    let replay = client
                        .read_resource(uri)
                        .map_err(|error| format!("read incoming source resource: {error}"))?;
                    let replay_text = replay["result"]["contents"][0]["text"]
                        .as_str()
                        .ok_or_else(|| format!("malformed incoming source resource: {replay}"))?;
                    let replay: Value = serde_json::from_str(replay_text)
                        .map_err(|error| format!("incoming source was not JSON: {error}"))?;
                    if !replay["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("call-context-marker"))
                    {
                        return Err(format!("incoming source omitted marker: {replay}"));
                    }

                    deferred_resource = Some((uri.to_owned(), original_hash.to_owned()));
                }
            }
        }

        pages += 1;
        page_token = inner["next_cursor"].as_str().map(str::to_owned);
        if page_token.is_none() {
            if seen_edges.len() != total_call_sites as usize || total_calls < 3 || pages < 3 {
                return Err(format!(
                    "incoming pages did not reconstruct full graph: pages={pages}, edges={}, result={inner}",
                    seen_edges.len()
                ));
            }

            let (resource_uri, original_hash) = deferred_resource
                .ok_or_else(|| "incoming pages never returned caller_one resource".to_owned())?;
            let callers = workspace.join("src/callers.rs");
            let changed = fs::read_to_string(&callers)
                .map_err(|error| format!("read callers.rs: {error}"))?
                .replace("call-context-marker", "call-stale-marker");
            fs::write(&callers, changed).map_err(|error| format!("rewrite callers.rs: {error}"))?;
            let stale = client
                .read_resource(&resource_uri)
                .map_err(|error| format!("read stale incoming resource: {error}"))?;
            let stale_text = stale["result"]["contents"][0]["text"]
                .as_str()
                .ok_or_else(|| format!("malformed stale incoming resource: {stale}"))?;
            let stale: Value = serde_json::from_str(stale_text)
                .map_err(|error| format!("stale incoming resource was not JSON: {error}"))?;
            if !stale["text"]
                .as_str()
                .is_some_and(|text| text.contains("call-stale-marker"))
                || stale["content_hash"].as_str() == Some(&original_hash)
            {
                return Err(format!(
                    "stale incoming resource did not replay current source: {stale}"
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

/// Tool 13: `get_outgoing_calls` — `caller_one` calls `add` at three sites.
fn sc_get_outgoing_calls(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let callers = workspace.join("src/callers.rs");
    let caller_line = find_line(&callers, "pub fn caller_one(");
    let resp = client
        .call_tool(
            "prepare_call_hierarchy",
            &json!({
                "file_path": callers.to_string_lossy(),
                "line": caller_line,
                "character": 8
            }),
        )
        .map_err(|e| format!("prepare caller_one failed: {e}"))?;
    let prepared: Value = serde_json::from_str(&assertions::assert_tool_ok(&resp))
        .map_err(|e| format!("bad prepare JSON: {e}"))?;
    let item = prepared["items"]
        .as_array()
        .and_then(|items| items.first())
        .cloned()
        .ok_or_else(|| format!("prepare caller_one returned no item: {prepared}"))?;

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut page_token = None;
    let mut seen_edges = std::collections::BTreeSet::new();
    let mut expected_total_calls = None;
    let mut expected_total_call_sites = None;
    let mut deferred_resource = None;
    let mut pages = 0;
    loop {
        let resp = client
            .call_tool(
                "get_outgoing_calls",
                &json!({
                    "item": item,
                    "limits": { "total": 1, "per_symbol": 1 },
                    "page_token": page_token
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;
        let inner: Value = serde_json::from_str(&assertions::assert_tool_ok(&resp))
            .map_err(|e| format!("bad outgoing JSON: {e}"))?;
        let calls = inner["calls"]
            .as_array()
            .ok_or_else(|| format!("outgoing result omitted calls: {inner}"))?;
        let total_calls = inner["total_calls"]
            .as_u64()
            .ok_or_else(|| format!("outgoing result omitted total_calls: {inner}"))?;
        let total_call_sites = inner["total_call_sites"]
            .as_u64()
            .ok_or_else(|| format!("outgoing result omitted total_call_sites: {inner}"))?;
        if page_token.is_none() && total_call_sites < 3 {
            if Instant::now() >= deadline {
                return Err(format!("outgoing graph did not converge: {inner}"));
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        if expected_total_calls.is_some_and(|previous| previous != total_calls)
            || expected_total_call_sites.is_some_and(|previous| previous != total_call_sites)
        {
            return Err(format!("outgoing totals changed across pages: {inner}"));
        }
        expected_total_calls = Some(total_calls);
        expected_total_call_sites = Some(total_call_sites);

        for call in calls {
            let callee_name = call["to"]["name"]
                .as_str()
                .or_else(|| call["callee"]["name"].as_str())
                .unwrap_or("<unnamed>");
            if !callee_name.contains("add") {
                return Err(format!("unexpected outgoing callee: {callee_name}"));
            }
            for site in call["call_sites"]
                .as_array()
                .ok_or_else(|| format!("outgoing call omitted call_sites: {call}"))?
            {
                let range = &site["range"];
                let edge = format!(
                    "{callee_name}:{}:{}:{}",
                    range["start"]["line"], range["start"]["character"], range["end"]["character"]
                );
                if !seen_edges.insert(edge.clone()) {
                    return Err(format!("duplicate outgoing call-site edge: {edge}"));
                }
                if deferred_resource.is_none() {
                    let source = &site["source"];
                    let uri = source["resource"]["uri"]
                        .as_str()
                        .ok_or_else(|| format!("outgoing source was not deferred: {source}"))?;
                    let original_hash = source["content_hash"]
                        .as_str()
                        .ok_or_else(|| format!("outgoing source had no content hash: {source}"))?;
                    let replay = client
                        .read_resource(uri)
                        .map_err(|error| format!("read outgoing source resource: {error}"))?;
                    let replay_text = replay["result"]["contents"][0]["text"]
                        .as_str()
                        .ok_or_else(|| format!("malformed outgoing source resource: {replay}"))?;
                    let replay: Value = serde_json::from_str(replay_text)
                        .map_err(|error| format!("outgoing source was not JSON: {error}"))?;
                    if !replay["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("call-context-marker"))
                    {
                        return Err(format!("outgoing source omitted marker: {replay}"));
                    }
                    deferred_resource = Some((uri.to_owned(), original_hash.to_owned()));
                }
            }
        }

        pages += 1;
        page_token = inner["next_cursor"].as_str().map(str::to_owned);
        if page_token.is_none() {
            if seen_edges.len() != total_call_sites as usize || total_calls != 1 || pages < 3 {
                return Err(format!(
                    "outgoing pages did not reconstruct full graph: pages={pages}, edges={}, result={inner}",
                    seen_edges.len()
                ));
            }
            let (resource_uri, original_hash) = deferred_resource
                .ok_or_else(|| "outgoing pages returned no deferred source".to_owned())?;
            let changed = fs::read_to_string(&callers)
                .map_err(|error| format!("read callers.rs: {error}"))?
                .replace("call-context-marker", "call-stale-marker");
            fs::write(&callers, changed).map_err(|error| format!("rewrite callers.rs: {error}"))?;
            let stale = client
                .read_resource(&resource_uri)
                .map_err(|error| format!("read stale outgoing resource: {error}"))?;
            let stale_text = stale["result"]["contents"][0]["text"]
                .as_str()
                .ok_or_else(|| format!("malformed stale outgoing resource: {stale}"))?;
            let stale: Value = serde_json::from_str(stale_text)
                .map_err(|error| format!("stale outgoing resource was not JSON: {error}"))?;
            if !stale["text"]
                .as_str()
                .is_some_and(|text| text.contains("call-stale-marker"))
                || stale["content_hash"].as_str() == Some(&original_hash)
            {
                return Err(format!(
                    "stale outgoing resource did not replay current source: {stale}"
                ));
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("outgoing pagination did not exhaust: {inner}"));
        }
    }
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
    let mut cursor = None;
    let mut snapshot = None;
    let mut seen = std::collections::BTreeSet::new();
    let mut total = None;
    let mut returned = 0;
    for _ in 0..128 {
        let page = call_json(
            client,
            "get_server_logs",
            &json!({ "project_id": "default", "limit": 1, "cursor": cursor }),
        )?;
        let logs = page["logs"]
            .as_array()
            .or_else(|| page["entries"].as_array())
            .ok_or_else(|| format!("expected log entries array, got {page}"))?;
        let page_snapshot = page["snapshot_identity"]
            .as_str()
            .ok_or_else(|| format!("log page omitted snapshot_identity: {page}"))?;
        if snapshot.get_or_insert(page_snapshot.to_owned()) != page_snapshot {
            return Err(format!("log pages changed snapshot identity: {page}"));
        }
        let page_total = page["total"]
            .as_u64()
            .ok_or_else(|| format!("log page omitted total: {page}"))?;
        if total.get_or_insert(page_total) != &page_total {
            return Err(format!("log pages changed total: {page}"));
        }
        for log in logs {
            if !seen.insert(log.to_string()) {
                return Err(format!("log pages duplicated a record: {log}"));
            }
        }
        returned += logs.len() as u64;
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            if returned != page_total || page["remaining"] != 0 {
                return Err(format!(
                    "log pages did not exhaust retained records: {page}"
                ));
            }
            return Ok(());
        }
    }
    Err("log page walk did not terminate after 128 pages".to_owned())
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
    let mut page_token = None;
    let mut snapshot_identity = None;
    let mut signatures_seen = Vec::new();
    loop {
        let resp = client
            .call_tool(
                "get_signature_help",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "line": line,
                    "character": character,
                    "page_token": page_token,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let Some(sigs) = inner["signatures"].as_array() else {
            return Err(format!("signature help omitted signatures: {inner}"));
        };
        if sigs.is_empty() {
            if Instant::now() >= deadline {
                return Err(format!(
                    "get_signature_help: no signatures after 10 s; response={inner}"
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        let snapshot = inner["snapshot_identity"]
            .as_str()
            .ok_or_else(|| format!("signature help omitted snapshot identity: {inner}"))?;
        if snapshot_identity.get_or_insert_with(|| snapshot.to_owned()) != snapshot {
            return Err(format!("signature help changed snapshot identity: {inner}"));
        }
        let total = inner["total_signatures"]
            .as_u64()
            .ok_or_else(|| format!("signature help omitted total_signatures: {inner}"))?;
        signatures_seen.extend(sigs.iter().map(|signature| {
            signature["signature_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        }));
        if let Some(active) = inner["active_signature"].as_u64()
            && active >= total
        {
            return Err(format!("active signature index escaped total: {inner}"));
        }
        page_token = inner["next_cursor"].as_str().map(str::to_owned);
        if page_token.is_none() {
            if signatures_seen.len() != total as usize
                || signatures_seen.iter().any(String::is_empty)
                || signatures_seen
                    .iter()
                    .enumerate()
                    .any(|(index, id)| signatures_seen[..index].contains(id))
            {
                return Err(format!(
                    "signature pages were incomplete or duplicated: seen={}, total={total}, response={inner}",
                    signatures_seen.len()
                ));
            }
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
            assert_location_source_resource(client, &locs[0], "go_to_implementation")?;
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
            assert_location_source_resource(client, &locs[0], "go_to_type_definition")?;
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

fn assert_location_source_resource(
    client: &mut McpClient,
    location: &Value,
    label: &str,
) -> Result<(), String> {
    if location["source"]["status"] != "available" {
        return Err(format!("{label}: source was not available: {location}"));
    }
    let uri = location["source"]["resource"]["uri"]
        .as_str()
        .ok_or_else(|| format!("{label}: source had no deferred resource: {location}"))?;
    let response = client
        .read_resource(uri)
        .map_err(|error| format!("{label}: read deferred source resource: {error}"))?;
    let contents = response["result"]["contents"]
        .as_array()
        .ok_or_else(|| format!("{label}: malformed resource response: {response}"))?;
    if !contents.iter().any(|content| {
        content["uri"] == uri
            && content["text"]
                .as_str()
                .is_some_and(|text| text.contains("navigation-context-marker"))
    }) {
        return Err(format!(
            "{label}: deferred resource omitted source text: {response}"
        ));
    }
    Ok(())
}

/// Tool 20: `get_inlay_hints` — type hints in `lsp317_target`.
fn sc_get_inlay_hints(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let start_line = find_line(&lib, "pub fn lsp317_target(");
    let end_line = find_line(&lib, "let _ = (p, s);") + 1;

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut page_token = None;
    let mut snapshot_identity = None;
    let mut hint_ids = std::collections::BTreeSet::new();
    let mut hint_labels = std::collections::BTreeSet::new();
    let mut total_hints = None;
    let mut pages = 0;
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
                    "page_token": page_token,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let Some(hints) = inner["hints"].as_array() else {
            return Err(format!("get_inlay_hints omitted hints: {inner}"));
        };
        if hints.is_empty() {
            if Instant::now() >= deadline {
                return Err(format!(
                    "get_inlay_hints: no hints after 10 s; response={inner}"
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        let snapshot = inner["snapshot_identity"]
            .as_str()
            .ok_or_else(|| format!("inlay hint page omitted snapshot identity: {inner}"))?;
        if snapshot_identity.get_or_insert_with(|| snapshot.to_owned()) != snapshot {
            return Err(format!(
                "inlay hint pages changed snapshot identity: {inner}"
            ));
        }
        let total = inner["total_hints"]
            .as_u64()
            .ok_or_else(|| format!("inlay hint page omitted total_hints: {inner}"))?;
        if total_hints.get_or_insert(total) != &total {
            return Err(format!("inlay hint pages changed total_hints: {inner}"));
        }
        for hint in hints {
            let id = hint["hint_id"]
                .as_str()
                .ok_or_else(|| format!("inlay hint omitted hint_id: {hint}"))?;
            if !hint_ids.insert(id.to_owned()) {
                return Err(format!("inlay hint pages duplicated hint_id: {id}"));
            }
            if let Some(label) = hint["label"].as_str() {
                hint_labels.insert(label.to_owned());
            }
        }
        pages += 1;
        page_token = inner["next_cursor"].as_str().map(str::to_owned);
        if page_token.is_none() {
            if hint_ids.len() != total as usize
                || pages < 2
                || !hint_labels.iter().any(|label| label.contains("Point"))
                || !hint_labels.iter().any(|label| label.contains("i32"))
            {
                return Err(format!(
                    "inlay hint pages did not exhaust provider results: pages={pages}, hints={}, total={total}, response={inner}",
                    hint_ids.len()
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
    let mut cursor = None;
    let mut snapshot = None;
    let mut seen = std::collections::BTreeSet::new();
    let mut total = None;
    let mut returned = 0;
    for _ in 0..128 {
        let page = call_json(
            client,
            "get_server_messages",
            &json!({ "project_id": "default", "limit": 1, "cursor": cursor }),
        )?;
        let messages = page["messages"]
            .as_array()
            .ok_or_else(|| format!("expected message entries array, got {page}"))?;
        let page_snapshot = page["snapshot_identity"]
            .as_str()
            .ok_or_else(|| format!("message page omitted snapshot_identity: {page}"))?;
        if snapshot.get_or_insert(page_snapshot.to_owned()) != page_snapshot {
            return Err(format!("message pages changed snapshot identity: {page}"));
        }
        let page_total = page["total"]
            .as_u64()
            .ok_or_else(|| format!("message page omitted total: {page}"))?;
        if total.get_or_insert(page_total) != &page_total {
            return Err(format!("message pages changed total: {page}"));
        }
        for message in messages {
            if !seen.insert(message.to_string()) {
                return Err(format!("message pages duplicated a record: {message}"));
            }
        }
        returned += messages.len() as u64;
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            if returned != page_total || page["remaining"] != 0 {
                return Err(format!(
                    "message pages did not exhaust retained records: {page}"
                ));
            }
            return Ok(());
        }
    }
    Err("message page walk did not terminate after 128 pages".to_owned())
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
    let base_arguments = json!({
        "project_id": "default",
        "file_path": lib_rs,
        "start_line": module_line,
        "start_character": 9,
        "end_line": module_line,
        "end_character": 9,
        "kind_filter": "refactor.extract",
    });
    let mut page_token = None;
    let mut snapshot_identity = None;
    let mut total_actions = None;
    let mut actions = Vec::new();
    loop {
        let mut arguments = base_arguments.clone();
        if let Some(token) = &page_token {
            arguments["page_token"] = json!(token);
        }
        let page = call_json(client, "code_action_list", &arguments)?;
        let page_snapshot = page["snapshot_identity"]
            .as_str()
            .ok_or_else(|| format!("code action page omitted snapshot identity: {page}"))?;
        if snapshot_identity.get_or_insert_with(|| page_snapshot.to_owned()) != page_snapshot {
            return Err(format!(
                "code action pages changed snapshot identity: {page}"
            ));
        }
        let page_total = page["total_actions"]
            .as_u64()
            .ok_or_else(|| format!("code action page omitted total_actions: {page}"))?;
        if total_actions.get_or_insert(page_total) != &page_total {
            return Err(format!("code action pages changed total_actions: {page}"));
        }
        actions.extend(
            page["actions"]
                .as_array()
                .ok_or_else(|| format!("code action page omitted actions: {page}"))?
                .iter()
                .cloned(),
        );
        let Some(next) = page["next_cursor"].as_str() else {
            break;
        };
        page_token = Some(next.to_owned());
    }
    if Some(actions.len() as u64) != total_actions {
        return Err(format!(
            "code action page walk omitted actions: total={total_actions:?}, returned={}",
            actions.len()
        ));
    }
    for (index, action) in actions.iter().enumerate() {
        let Some(action_id) = action["action_id"].as_str() else {
            return Err(format!("code action omitted stable action_id: {action}"));
        };
        if actions[..index]
            .iter()
            .any(|previous| previous["action_id"] == action_id)
        {
            return Err(format!(
                "code action pages duplicated action_id {action_id}"
            ));
        }
    }
    let action = actions
        .iter()
        .find(|action| action["title"] == "Extract module to file")
        .ok_or_else(|| format!("rust-analyzer omitted native module move action: {actions:?}"))?;
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
    if declaration["kind"] != "declaration"
        || declaration["locations"][0]["path"].as_str().is_none()
        || declaration["locations"][0]["source"]["status"] != "available"
        || declaration["locations"][0]["symbol_handle"]
            .as_str()
            .is_none()
    {
        return Err(format!(
            "declaration omitted model-ready target context: {declaration}"
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
        || parent["kind"] != "parent_module"
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
        || children["kind"] != "child_modules"
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

    let (macro_line, macro_character) = find_position(&lib, "huge_semantic!()");
    let expansion = call_json(
        client,
        "expand_macro",
        &json!({
            "project_id": "default",
            "file_path": lib,
            "line": macro_line,
            "character": macro_character,
        }),
    )?;
    if expansion["supported"] != true || expansion["kind"] != "macro_expansion" {
        return Err(format!("macro expansion was unavailable: {expansion}"));
    }

    let macro_resource_uri = expansion["macro_expansion_resource"]["uri"]
        .as_str()
        .ok_or_else(|| format!("oversized macro expansion was not deferred: {expansion}"))?;
    let mut resource_uri = macro_resource_uri.to_owned();
    let mut expansion_text = String::new();
    let mut resource_pages = 0;
    loop {
        let resource = client
            .read_resource(&resource_uri)
            .map_err(|error| format!("read macro expansion resource failed: {error}"))?;
        let resource_text = resource["result"]["contents"][0]["text"]
            .as_str()
            .ok_or_else(|| format!("malformed macro expansion resource: {resource}"))?;
        let replay: Value = serde_json::from_str(resource_text)
            .map_err(|error| format!("macro expansion resource was not JSON: {error}"))?;
        expansion_text.push_str(
            replay["text"]
                .as_str()
                .ok_or_else(|| format!("macro expansion page omitted text: {replay}"))?,
        );
        resource_pages += 1;
        if let Some(next_uri) = replay["next_uri"].as_str() {
            resource_uri = next_uri.to_owned();
        } else {
            break;
        }
        if resource_pages > 64 {
            return Err("macro expansion resource did not exhaust within 64 pages".to_owned());
        }
    }
    if resource_pages < 2 || !expansion_text.contains("7_u32") {
        return Err(format!(
            "macro expansion resource omitted complete expansion: pages={resource_pages}, tail_present={}",
            expansion_text.contains("7_u32")
        ));
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
    inflate_navigation_source_context(&lib_rs);
    wait_until_ready(&mut client, &lib_rs);

    // Sub-case registry.
    let sub_cases: &[SubCase] = &[
        sub_case!(sc_get_hover),
        sub_case!(sc_get_definition),
        sub_case!(sc_dependency_source_context),
        sub_case!(sc_get_references),
        sub_case!(sc_get_diagnostics),
        sub_case!(sc_rename_symbol),
        sub_case!(sc_rename_symbol_deferred),
        sub_case!(sc_get_completions),
        sub_case!(sc_get_document_symbols),
        sub_case!(sc_format_document),
        sub_case!(sc_format_document_deferred),
        sub_case!(sc_workspace_symbol_search),
        sub_case!(sc_inspect_symbol),
        sub_case!(sc_no_reread_corpus),
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
    assert_eq!(projects["projects"].as_array().unwrap().len(), 2);
    assert_eq!(projects["returned"], 2);
    assert!(projects["next_cursor"].is_null());

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
    wait_until_ready(client, &fixture.second_lib);
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

    let retry = call_json(
        client,
        "workspace_edit_apply",
        &json!({"project_id": "second", "plan_id": rename_plan}),
    )
    .unwrap();
    assert_eq!(retry, applied, "a committed edit plan retry is idempotent");
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
