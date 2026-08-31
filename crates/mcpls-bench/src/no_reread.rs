use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::BufRead;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TraceEvent {
    Semantic {
        tool: String,
        source_path: Option<String>,
        coordinate_input: bool,
        #[serde(default)]
        request_bytes: usize,
        result_bytes: usize,
        #[serde(default)]
        query_fingerprint: Option<String>,
        #[serde(default)]
        deferred_bytes: usize,
        truncated: bool,
        latency_ms: u64,
        unsupported: bool,
        error: bool,
    },
    SourceRead {
        path: String,
        #[serde(default)]
        output_bytes: usize,
    },
    Lifecycle {
        tool: String,
        request_bytes: usize,
        #[serde(default)]
        result_bytes: usize,
        latency_ms: u64,
    },
    McpTool {
        tool: String,
        request_bytes: usize,
        #[serde(default)]
        result_bytes: usize,
        latency_ms: u64,
    },
    ResourceRead {
        deferred: bool,
        request_bytes: usize,
        #[serde(default)]
        result_bytes: usize,
        latency_ms: u64,
    },
    ShellOutput {
        bytes: usize,
    },
    Compaction,
}

/// Nearest-rank latency percentile summary in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LatencyPercentiles {
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub p99_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceReport {
    /// All MCPLS calls in this task history.
    pub mcpls_calls: usize,
    pub semantic_calls: usize,
    pub lifecycle_calls: usize,
    pub duplicate_queries: usize,
    pub request_bytes: usize,
    pub post_semantic_same_file_reads: usize,
    pub pre_coordinate_source_reads: usize,
    pub coordinate_calls: usize,
    pub result_bytes: usize,
    pub deferred_bytes: usize,
    pub deferred_resource_reads: usize,
    pub shell_output_bytes: usize,
    pub compactions: usize,
    pub latency: LatencyPercentiles,
    pub truncated: usize,
    pub latency_ms: u64,
    pub unsupported: usize,
    pub errors: usize,
    pub post_semantic_same_file_read_rate: Rate,
    pub pre_coordinate_source_read_rate: Rate,
    pub truncation_rate: Rate,
    pub unsupported_rate: Rate,
    pub error_rate: Rate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rate {
    pub numerator: usize,
    pub denominator: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvaluationReport {
    pub schema_version: u32,
    pub aggregate: TraceReport,
    pub by_tool: BTreeMap<String, TraceReport>,
}

#[must_use]
pub fn scrub_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let components = normalized
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    components
        .iter()
        .rposition(|component| *component == "src")
        .map_or_else(
            || components.last().copied().unwrap_or_default().to_owned(),
            |start| components[start..].join("/"),
        )
}

#[must_use]
pub fn classify_trace(events: &[TraceEvent]) -> TraceReport {
    let mut report = TraceReport {
        mcpls_calls: 0,
        semantic_calls: 0,
        lifecycle_calls: 0,
        duplicate_queries: 0,
        request_bytes: 0,
        post_semantic_same_file_reads: 0,
        pre_coordinate_source_reads: 0,
        coordinate_calls: 0,
        result_bytes: 0,
        deferred_bytes: 0,
        deferred_resource_reads: 0,
        shell_output_bytes: 0,
        compactions: 0,
        latency: LatencyPercentiles {
            p50_ms: 0,
            p90_ms: 0,
            p99_ms: 0,
        },
        truncated: 0,
        latency_ms: 0,
        unsupported: 0,
        errors: 0,
        post_semantic_same_file_read_rate: Rate {
            numerator: 0,
            denominator: 0,
        },
        pre_coordinate_source_read_rate: Rate {
            numerator: 0,
            denominator: 0,
        },
        truncation_rate: Rate {
            numerator: 0,
            denominator: 0,
        },
        unsupported_rate: Rate {
            numerator: 0,
            denominator: 0,
        },
        error_rate: Rate {
            numerator: 0,
            denominator: 0,
        },
    };
    let mut query_fingerprints = BTreeSet::new();
    let mut latencies = Vec::new();

    for (index, event) in events.iter().enumerate() {
        if let TraceEvent::Semantic { .. } = event {
            record_semantic(
                &mut report,
                event,
                index,
                events,
                &mut query_fingerprints,
                &mut latencies,
            );
        } else {
            record_non_semantic(&mut report, event, &mut latencies);
        }
    }

    let rate = |numerator, denominator| Rate {
        numerator,
        denominator,
    };
    report.post_semantic_same_file_read_rate =
        rate(report.post_semantic_same_file_reads, report.semantic_calls);
    report.pre_coordinate_source_read_rate =
        rate(report.pre_coordinate_source_reads, report.coordinate_calls);
    report.truncation_rate = rate(report.truncated, report.semantic_calls);
    report.unsupported_rate = rate(report.unsupported, report.semantic_calls);
    report.error_rate = rate(report.errors, report.semantic_calls);
    report.latency = latency_percentiles(&mut latencies);
    report
}

fn record_non_semantic(report: &mut TraceReport, event: &TraceEvent, latencies: &mut Vec<u64>) {
    match event {
        TraceEvent::Lifecycle {
            request_bytes,
            result_bytes,
            latency_ms,
            ..
        } => {
            report.mcpls_calls += 1;
            report.lifecycle_calls += 1;
            report.request_bytes += request_bytes;
            report.result_bytes += result_bytes;
            latencies.push(*latency_ms);
        }
        TraceEvent::McpTool {
            request_bytes,
            result_bytes,
            latency_ms,
            ..
        } => {
            report.mcpls_calls += 1;
            report.request_bytes += request_bytes;
            report.result_bytes += result_bytes;
            latencies.push(*latency_ms);
        }
        TraceEvent::ResourceRead {
            deferred,
            request_bytes,
            result_bytes,
            latency_ms,
        } => {
            report.mcpls_calls += 1;
            report.request_bytes += request_bytes;
            report.result_bytes += result_bytes;
            report.deferred_resource_reads += usize::from(*deferred);
            latencies.push(*latency_ms);
        }
        TraceEvent::ShellOutput { bytes } => report.shell_output_bytes += bytes,
        TraceEvent::Compaction => report.compactions += 1,
        TraceEvent::SourceRead { .. } | TraceEvent::Semantic { .. } => {}
    }
}

fn record_semantic(
    report: &mut TraceReport,
    event: &TraceEvent,
    index: usize,
    events: &[TraceEvent],
    query_fingerprints: &mut BTreeSet<String>,
    latencies: &mut Vec<u64>,
) {
    let TraceEvent::Semantic {
        source_path,
        coordinate_input,
        request_bytes,
        result_bytes,
        query_fingerprint,
        deferred_bytes,
        truncated,
        latency_ms,
        unsupported,
        error,
        ..
    } = event
    else {
        return;
    };
    report.mcpls_calls += 1;
    report.semantic_calls += 1;
    report.request_bytes += request_bytes;
    report.result_bytes += result_bytes;
    report.deferred_bytes += deferred_bytes;
    report.latency_ms += latency_ms;
    report.truncated += usize::from(*truncated);
    report.unsupported += usize::from(*unsupported);
    report.errors += usize::from(*error);
    report.coordinate_calls += usize::from(*coordinate_input);
    latencies.push(*latency_ms);
    report.duplicate_queries += usize::from(
        query_fingerprint
            .as_ref()
            .is_some_and(|fingerprint| !query_fingerprints.insert(fingerprint.clone())),
    );
    let same_path = |event: Option<&TraceEvent>| {
        matches!(
            (source_path.as_deref(), event),
            (Some(source), Some(TraceEvent::SourceRead { path, .. }))
                if scrub_path(source) == scrub_path(path)
        )
    };
    report.post_semantic_same_file_reads += usize::from(same_path(events.get(index + 1)));
    report.pre_coordinate_source_reads += usize::from(
        *coordinate_input && same_path(index.checked_sub(1).and_then(|i| events.get(i))),
    );
}

fn latency_percentiles(latencies: &mut [u64]) -> LatencyPercentiles {
    latencies.sort_unstable();
    let percentile = |percent: usize| {
        let index = latencies
            .len()
            .saturating_mul(percent)
            .saturating_add(99)
            .saturating_div(100)
            .saturating_sub(1);
        latencies.get(index).copied().unwrap_or_default()
    };
    LatencyPercentiles {
        p50_ms: percentile(50),
        p90_ms: percentile(90),
        p99_ms: percentile(99),
    }
}

#[must_use]
pub fn evaluate(events: &[TraceEvent]) -> EvaluationReport {
    let mut by_tool_events = BTreeMap::<String, Vec<TraceEvent>>::new();
    for (index, event) in events.iter().enumerate() {
        let TraceEvent::Semantic { tool, .. } = event else {
            continue;
        };
        let tool_events = by_tool_events.entry(tool.clone()).or_default();
        if let Some(TraceEvent::SourceRead { .. }) = index
            .checked_sub(1)
            .and_then(|previous| events.get(previous))
        {
            tool_events.push(events[index - 1].clone());
        }
        tool_events.push(event.clone());
        if let Some(TraceEvent::SourceRead { .. }) = events.get(index + 1) {
            tool_events.push(events[index + 1].clone());
        }
    }
    EvaluationReport {
        schema_version: 1,
        aggregate: classify_trace(events),
        by_tool: by_tool_events
            .into_iter()
            .map(|(tool, events)| (tool, classify_trace(&events)))
            .collect(),
    }
}

pub fn parse_history(reader: impl BufRead) -> Result<Vec<TraceEvent>> {
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let event: Value = serde_json::from_str(&line).context("parsing history JSONL")?;
        if let Some(semantic) = semantic_history_event(&event) {
            events.push(semantic);
        } else if let Some(call) = other_mcpls_history_event(&event) {
            events.push(call);
        } else if let Some(read) = source_read_history_event(&event) {
            events.push(read);
        } else if let Some(output) = shell_output_history_event(&event) {
            events.push(output);
        } else if compaction_history_event(&event) {
            events.push(TraceEvent::Compaction);
        }
    }
    Ok(events)
}

fn semantic_history_event(event: &Value) -> Option<TraceEvent> {
    let payload = event.get("payload")?;
    let invocation = (event.get("type")?.as_str()? == "event_msg"
        && payload.get("type")?.as_str()? == "mcp_tool_call_end")
        .then(|| payload.get("invocation"))??;
    if invocation.get("server")?.as_str()? != "mcpls" {
        return None;
    }
    let tool = invocation.get("tool")?.as_str()?;
    if !is_semantic_tool(tool) {
        return None;
    }
    let arguments = invocation.get("arguments").unwrap_or(&Value::Null);
    let result = payload.get("result").unwrap_or(&Value::Null);
    let source_path = find_path(arguments)
        .or_else(|| find_path(result))
        .map(|path| scrub_path(&path));
    let duration = payload.get("duration").unwrap_or(&Value::Null);
    let latency_ms = duration
        .get("secs")
        .and_then(Value::as_u64)
        .unwrap_or_default()
        .saturating_mul(1_000)
        + duration
            .get("nanos")
            .and_then(Value::as_u64)
            .unwrap_or_default()
            / 1_000_000;
    let serialized = serde_json::to_vec(result).unwrap_or_default();
    let error = result.get("Err").is_some();
    Some(TraceEvent::Semantic {
        tool: tool.to_owned(),
        source_path,
        coordinate_input: arguments.get("line").is_some()
            && arguments.get("symbol_handle").is_none(),
        request_bytes: serialized_len(arguments),
        result_bytes: serialized.len(),
        query_fingerprint: Some(query_fingerprint(tool, arguments)),
        deferred_bytes: deferred_bytes(result),
        truncated: contains_true(result, "truncated"),
        latency_ms,
        unsupported: error
            && result
                .to_string()
                .to_ascii_lowercase()
                .contains("unsupported"),
        error,
    })
}

fn other_mcpls_history_event(event: &Value) -> Option<TraceEvent> {
    let payload = event.get("payload")?;
    let invocation = (event.get("type")?.as_str()? == "event_msg"
        && payload.get("type")?.as_str()? == "mcp_tool_call_end")
        .then(|| payload.get("invocation"))??;
    if invocation.get("server")?.as_str()? != "mcpls" {
        return None;
    }
    let tool = invocation.get("tool")?.as_str()?;
    if is_semantic_tool(tool) {
        return None;
    }
    let arguments = invocation.get("arguments").unwrap_or(&Value::Null);
    let request_bytes = serialized_len(arguments);
    let result_bytes = serialized_len(payload.get("result").unwrap_or(&Value::Null));
    let latency_ms = history_latency_ms(payload);
    if is_lifecycle_tool(tool) {
        return Some(TraceEvent::Lifecycle {
            tool: tool.to_owned(),
            request_bytes,
            result_bytes,
            latency_ms,
        });
    }
    if tool == "read_semantic_resource" {
        return Some(TraceEvent::ResourceRead {
            deferred: arguments
                .get("uri")
                .and_then(Value::as_str)
                .is_some_and(|uri| uri.starts_with("mcpls-deferred://")),
            request_bytes,
            result_bytes,
            latency_ms,
        });
    }
    Some(TraceEvent::McpTool {
        tool: tool.to_owned(),
        request_bytes,
        result_bytes,
        latency_ms,
    })
}

fn source_read_history_event(event: &Value) -> Option<TraceEvent> {
    let payload = event.get("payload")?;
    if event.get("type")?.as_str()? != "response_item"
        || payload.get("type")?.as_str()? != "custom_tool_call"
        || payload.get("name")?.as_str()? != "exec"
    {
        return None;
    }
    let input = payload.get("input")?.as_str()?;
    let lower = input.to_ascii_lowercase();
    if !["sed ", "rg ", "cat ", "bat ", "read_file", "view_image"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return None;
    }
    source_path_token(input).map(|path| TraceEvent::SourceRead {
        path: scrub_path(path),
        output_bytes: 0,
    })
}

fn shell_output_history_event(event: &Value) -> Option<TraceEvent> {
    let payload = event.get("payload")?;
    if event.get("type")?.as_str()? != "response_item"
        || payload.get("type")?.as_str()? != "custom_tool_call_output"
        || payload.get("name")?.as_str()? != "exec"
    {
        return None;
    }
    Some(TraceEvent::ShellOutput {
        bytes: payload.get("output").map_or(0, |output| {
            output
                .as_str()
                .map_or_else(|| serialized_len(output), str::len)
        }),
    })
}

fn compaction_history_event(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some("event_msg")
        && event
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.contains("compact"))
}

fn history_latency_ms(payload: &Value) -> u64 {
    let duration = payload.get("duration").unwrap_or(&Value::Null);
    duration
        .get("secs")
        .and_then(Value::as_u64)
        .unwrap_or_default()
        .saturating_mul(1_000)
        + duration
            .get("nanos")
            .and_then(Value::as_u64)
            .unwrap_or_default()
            / 1_000_000
}

fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |serialized| serialized.len())
}

fn query_fingerprint(tool: &str, arguments: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tool.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(arguments).unwrap_or_default());
    format!("{:x}", hasher.finalize())
}

fn deferred_bytes(value: &Value) -> usize {
    match value {
        Value::Object(object) => object.iter().fold(0, |total, (key, value)| {
            total.saturating_add(if key == "deferred" {
                value.as_array().map_or(0, |references| {
                    references.iter().fold(0, |total, reference| {
                        total.saturating_add(
                            reference
                                .get("bytes")
                                .and_then(Value::as_u64)
                                .and_then(|bytes| usize::try_from(bytes).ok())
                                .unwrap_or_default(),
                        )
                    })
                })
            } else {
                deferred_bytes(value)
            })
        }),
        Value::Array(values) => values.iter().map(deferred_bytes).sum(),
        Value::String(text) if text.starts_with('{') || text.starts_with('[') => {
            serde_json::from_str(text).map_or(0, |value| deferred_bytes(&value))
        }
        _ => 0,
    }
}

fn source_path_token(input: &str) -> Option<&str> {
    input
        .split(|character: char| {
            character.is_whitespace()
                || matches!(character, '"' | '\'' | '`' | ',' | ';' | ')' | '(')
        })
        .map(|token| token.trim_end_matches([':', '\\', 'n']))
        .find(|token| {
            [
                ".c", ".cc", ".cpp", ".cs", ".go", ".h", ".hpp", ".java", ".js", ".jsx", ".kt",
                ".kts", ".m", ".mm", ".nix", ".php", ".py", ".rb", ".rs", ".scala", ".sh",
                ".swift", ".toml", ".ts", ".tsx",
            ]
            .iter()
            .any(|extension| token.ends_with(extension))
        })
}

fn find_path(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            for key in ["file_path", "path", "uri", "project_relative_path"] {
                if let Some(path) = object.get(key).and_then(Value::as_str) {
                    return Some(path.strip_prefix("file://").unwrap_or(path).to_owned());
                }
            }
            object.values().find_map(find_path)
        }
        Value::Array(array) => array.iter().find_map(find_path),
        Value::String(text) if text.starts_with('{') || text.starts_with('[') => {
            serde_json::from_str(text)
                .ok()
                .and_then(|value| find_path(&value))
        }
        _ => None,
    }
}

fn contains_true(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.get(key).and_then(Value::as_bool) == Some(true)
                || object.values().any(|value| contains_true(value, key))
        }
        Value::Array(array) => array.iter().any(|value| contains_true(value, key)),
        Value::String(text) if text.starts_with('{') || text.starts_with('[') => {
            serde_json::from_str(text).is_ok_and(|value| contains_true(&value, key))
        }
        _ => false,
    }
}

fn is_semantic_tool(tool: &str) -> bool {
    matches!(
        tool,
        "workspace_symbol_search"
            | "workspace_symbol_search_batch"
            | "get_document_symbols"
            | "get_definition"
            | "get_hover"
            | "get_references"
            | "prepare_call_hierarchy"
            | "get_incoming_calls"
            | "get_outgoing_calls"
            | "get_diagnostics"
            | "get_cached_diagnostics"
            | "inspect_symbol"
            | "inspect_symbol_batch"
            | "lexical_search"
    )
}

fn is_lifecycle_tool(tool: &str) -> bool {
    matches!(
        tool,
        "project_add"
            | "project_activate"
            | "project_refresh"
            | "project_remove"
            | "project_restart_lsp"
            | "project_configure_cargo_features"
            | "project_status"
            | "project_list"
            | "project_lsp_capabilities"
            | "server_status"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic(path: &str, coordinate_input: bool) -> TraceEvent {
        TraceEvent::Semantic {
            tool: "get_definition".to_owned(),
            source_path: Some(path.to_owned()),
            coordinate_input,
            request_bytes: 0,
            result_bytes: 512,
            query_fingerprint: None,
            deferred_bytes: 0,
            truncated: false,
            latency_ms: 12,
            unsupported: false,
            error: false,
        }
    }

    #[test]
    fn scrubbing_keeps_only_a_safe_relative_suffix() {
        assert_eq!(
            scrub_path("/home/alice/proprietary/src/payment/card.rs"),
            "src/payment/card.rs"
        );
        assert_eq!(
            scrub_path("C:\\Users\\alice\\secret\\src\\lib.rs"),
            "src/lib.rs"
        );
    }

    #[test]
    fn classification_counts_reads_on_the_same_scrubbed_file_only() {
        let events = [
            TraceEvent::SourceRead {
                path: "/private/repo/src/lib.rs".to_owned(),
                output_bytes: 0,
            },
            semantic("/private/repo/src/lib.rs", true),
            TraceEvent::SourceRead {
                path: "/private/repo/src/lib.rs".to_owned(),
                output_bytes: 0,
            },
            semantic("/private/repo/src/other.rs", false),
            TraceEvent::SourceRead {
                path: "/private/repo/src/unrelated.rs".to_owned(),
                output_bytes: 0,
            },
        ];

        assert_eq!(
            classify_trace(&events),
            TraceReport {
                mcpls_calls: 2,
                semantic_calls: 2,
                lifecycle_calls: 0,
                duplicate_queries: 0,
                request_bytes: 0,
                post_semantic_same_file_reads: 1,
                pre_coordinate_source_reads: 1,
                coordinate_calls: 1,
                result_bytes: 1024,
                deferred_bytes: 0,
                deferred_resource_reads: 0,
                shell_output_bytes: 0,
                compactions: 0,
                latency: LatencyPercentiles {
                    p50_ms: 12,
                    p90_ms: 12,
                    p99_ms: 12,
                },
                truncated: 0,
                latency_ms: 24,
                unsupported: 0,
                errors: 0,
                post_semantic_same_file_read_rate: Rate {
                    numerator: 1,
                    denominator: 2
                },
                pre_coordinate_source_read_rate: Rate {
                    numerator: 1,
                    denominator: 1
                },
                truncation_rate: Rate {
                    numerator: 0,
                    denominator: 2
                },
                unsupported_rate: Rate {
                    numerator: 0,
                    denominator: 2
                },
                error_rate: Rate {
                    numerator: 0,
                    denominator: 2
                },
            }
        );
    }

    #[test]
    fn history_parser_emits_only_scrubbed_semantic_and_source_read_events() {
        let history = concat!(
            r#"{"type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"mcpls","tool":"get_definition","arguments":{"file_path":"/home/alice/private/src/lib.rs","line":2}},"duration":{"secs":0,"nanos":12000000},"result":{"Ok":{"content":[{"text":"{\"locations\":[{\"path\":\"/home/alice/private/src/lib.rs\",\"truncated\":false}]}"}]}}}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"const r = exec_command({cmd: \"sed -n 1,40p /home/alice/private/src/lib.rs\"});"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","content":"private prose"}}"#,
            "\n"
        );
        let events = match parse_history(history.as_bytes()) {
            Ok(events) => events,
            Err(error) => panic!("history fixture should parse: {error}"),
        };
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| !format!("{event:?}").contains("alice"))
        );
        assert_eq!(evaluate(&events).aggregate.post_semantic_same_file_reads, 1);
    }

    #[test]
    fn history_parser_reports_amplification_without_retaining_queries() {
        let history = concat!(
            r#"{"type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"mcpls","tool":"workspace_symbol_search","arguments":{"query":"private_type"}},"duration":{"secs":0,"nanos":12000000},"result":{"Ok":{"truncated":true,"deferred":[{"bytes":64}]}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"mcpls","tool":"workspace_symbol_search","arguments":{"query":"private_type"}},"duration":{"secs":0,"nanos":13000000},"result":{"Ok":{"truncated":false}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"mcpls","tool":"project_add","arguments":{"project_id":"fixture"}},"duration":{"secs":0,"nanos":1000000},"result":{"Ok":{}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"mcpls","tool":"read_semantic_resource","arguments":{"uri":"mcpls-deferred://opaque"}},"duration":{"secs":0,"nanos":1000000},"result":{"Ok":{}}}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"const r = exec_command({cmd: \"sed -n 1,40p /home/alice/private/src/lib.rs\"});"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","name":"exec","output":"0123456789"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_compacted"}}"#,
            "\n"
        );
        let events = parse_history(history.as_bytes())
            .unwrap_or_else(|error| panic!("history fixture should parse: {error}"));
        let report = evaluate(&events).aggregate;

        assert_eq!(report.mcpls_calls, 4);
        assert_eq!(report.semantic_calls, 2);
        assert_eq!(report.lifecycle_calls, 1);
        assert_eq!(report.duplicate_queries, 1);
        assert_eq!(report.deferred_bytes, 64);
        assert_eq!(report.deferred_resource_reads, 1);
        assert_eq!(report.shell_output_bytes, 10);
        assert_eq!(report.compactions, 1);
        assert_eq!(report.latency.p50_ms, 1);
        assert_eq!(report.latency.p90_ms, 13);
        assert!(
            events
                .iter()
                .all(|event| !format!("{event:?}").contains("private_type"))
        );
    }

    #[test]
    fn history_parser_counts_non_semantic_response_bytes() {
        let history = concat!(
            r#"{"type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"mcpls","tool":"workspace_edit_preview","arguments":{}},"duration":{"secs":0,"nanos":1000000},"result":{"Ok":{"plan_id":"opaque"}}}}"#,
            "\n",
        );

        let report = evaluate(&parse_history(history.as_bytes()).unwrap()).aggregate;

        assert_eq!(
            report.result_bytes,
            serialized_len(&serde_json::json!({"Ok": {"plan_id": "opaque"}}))
        );
    }

    #[test]
    fn checked_in_corpus_is_scrubbed_and_covers_every_enrichment_ticket() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let corpus = std::fs::read_to_string(root.join("benchmarks/no-reread-corpus.json"))
            .unwrap_or_else(|error| panic!("reading corpus: {error}"));
        assert!(!corpus.contains("/home/") && !corpus.contains("C:\\Users\\"));
        let corpus: Value =
            serde_json::from_str(&corpus).unwrap_or_else(|error| panic!("parsing corpus: {error}"));
        let cases = corpus["cases"]
            .as_array()
            .unwrap_or_else(|| panic!("corpus cases must be an array"));
        for ticket in [
            "MCPLS-54", "MCPLS-55", "MCPLS-56", "MCPLS-57", "MCPLS-58", "MCPLS-59", "MCPLS-60",
            "MCPLS-61", "MCPLS-62", "MCPLS-64",
        ] {
            assert!(cases.iter().any(|case| case["ticket"] == ticket));
        }
        assert!(cases.iter().all(|case| {
            case.get("prompt").is_none()
                && case["required_quality"]
                    .as_array()
                    .is_some_and(|quality| quality.iter().all(Value::is_string))
        }));

        let baseline = std::fs::read_to_string(root.join("benchmarks/no-reread-baseline.json"))
            .unwrap_or_else(|error| panic!("reading baseline: {error}"));
        assert!(!baseline.contains("/home/") && !baseline.contains("C:\\Users\\"));
        let baseline: Value = serde_json::from_str(&baseline)
            .unwrap_or_else(|error| panic!("parsing baseline: {error}"));
        for ticket in [
            "MCPLS-54", "MCPLS-55", "MCPLS-56", "MCPLS-57", "MCPLS-58", "MCPLS-59", "MCPLS-60",
            "MCPLS-61", "MCPLS-62", "MCPLS-64",
        ] {
            assert!(baseline["ticket_baselines"].get(ticket).is_some());
            assert!(baseline["targets"].get(ticket).is_some());
        }
    }
}
