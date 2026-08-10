//! Assertion helpers for e2e test sub-cases.
#![allow(dead_code)]

use serde_json::Value;

/// Extract JSON from native structured content, falling back to legacy text.
///
/// MCP tool responses have the shape:
/// New servers populate `result.structuredContent`; compatibility servers put
/// the same JSON in the first text block.
///
/// Returns the inner text string or an empty string if absent.
pub fn content_text(response: &Value) -> String {
    (!response["result"]["structuredContent"].is_null())
        .then(|| response["result"]["structuredContent"].to_string())
        .or_else(|| {
            response["result"]["content"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|item| item["text"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// Assert that the MCP response is not an MCP-level error (isError = true).
///
/// Returns serialized structured content, or legacy text, on success.
pub fn assert_tool_ok(response: &Value) -> String {
    let is_error = response["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        !is_error,
        "Expected successful tool response, got isError=true: {}",
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("<no text>")
    );
    content_text(response)
}

/// Assert that a JSON string parsed from tool text contains a symbol with the given name.
///
/// `symbols` should be an array of objects each having at least a `name` field.
pub fn assert_contains_symbol(symbols: &Value, name: &str) {
    let arr = symbols
        .as_array()
        .unwrap_or_else(|| panic!("expected array of symbols, got {symbols}"));
    let found = arr.iter().any(|s| s["name"].as_str().unwrap_or("") == name);
    assert!(found, "symbol '{name}' not found in {symbols}");
}

/// Assert that a URI ends with the given suffix.
pub fn assert_uri_ends_with(uri: &str, suffix: &str) {
    assert!(
        uri.ends_with(suffix),
        "expected URI to end with '{suffix}', got '{uri}'"
    );
}

/// Build a `file://` URI for an absolute path.
///
/// Handles macOS `/private/var` → `/var` symlinks by using the path as-is.
pub fn file_uri(path: &std::path::Path) -> String {
    url::Url::from_file_path(path)
        .unwrap_or_else(|()| panic!("cannot convert path to file URI: {}", path.display()))
        .to_string()
}
