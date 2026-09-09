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
        #[serde(default)]
        deferred_resource_references: usize,
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
        #[serde(default)]
        failed: bool,
    },
    McpTool {
        tool: String,
        request_bytes: usize,
        #[serde(default)]
        result_bytes: usize,
        latency_ms: u64,
        #[serde(default)]
        failed: bool,
    },
    ResourceRead {
        deferred: bool,
        request_bytes: usize,
        #[serde(default)]
        result_bytes: usize,
        latency_ms: u64,
        #[serde(default)]
        failed: bool,
    },
    ShellOutput {
        bytes: usize,
    },
    TaskComplete,
    Compaction,
}

/// Nearest-rank latency percentile summary in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LatencyPercentiles {
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub p99_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
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
    pub deferred_resource_references: usize,
    pub deferred_resource_reads: usize,
    pub source_read_output_bytes: usize,
    pub shell_output_bytes: usize,
    pub shell_source_reads: usize,
    pub semantic_calls_followed_by_shell_read: usize,
    pub completed_tasks: usize,
    pub calls_per_completed_task: Rate,
    pub semantic_to_shell_read_rate: Rate,
    pub deferred_resource_follow_through_rate: Rate,
    pub compactions: usize,
    pub latency: LatencyPercentiles,
    pub truncated: usize,
    pub latency_ms: u64,
    pub unsupported: usize,
    pub errors: usize,
    pub failed_calls: usize,
    pub post_semantic_same_file_read_rate: Rate,
    pub pre_coordinate_source_read_rate: Rate,
    pub truncation_rate: Rate,
    pub unsupported_rate: Rate,
    pub error_rate: Rate,
    pub failure_rate: Rate,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
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

pub const EVALUATION_SCHEMA_VERSION: u32 = 5;

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
    let mut report = TraceReport::default();
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
            reset_query_fingerprints_at_task_boundary(event, &mut query_fingerprints);
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
    report.calls_per_completed_task = rate(report.mcpls_calls, report.completed_tasks);
    report.semantic_to_shell_read_rate = rate(
        report.semantic_calls_followed_by_shell_read,
        report.semantic_calls,
    );
    report.deferred_resource_follow_through_rate = rate(
        report.deferred_resource_reads,
        report.deferred_resource_references,
    );
    report.truncation_rate = rate(report.truncated, report.semantic_calls);
    report.unsupported_rate = rate(report.unsupported, report.semantic_calls);
    report.error_rate = rate(report.errors, report.semantic_calls);
    report.failure_rate = rate(report.failed_calls, report.mcpls_calls);
    report.latency = latency_percentiles(&mut latencies);
    report
}

fn record_non_semantic(report: &mut TraceReport, event: &TraceEvent, latencies: &mut Vec<u64>) {
    match event {
        TraceEvent::Lifecycle {
            request_bytes,
            result_bytes,
            latency_ms,
            failed,
            ..
        } => {
            report.mcpls_calls += 1;
            report.lifecycle_calls += 1;
            report.request_bytes += request_bytes;
            report.result_bytes += result_bytes;
            report.failed_calls += usize::from(*failed);
            latencies.push(*latency_ms);
        }
        TraceEvent::McpTool {
            request_bytes,
            result_bytes,
            latency_ms,
            failed,
            ..
        } => {
            report.mcpls_calls += 1;
            report.request_bytes += request_bytes;
            report.result_bytes += result_bytes;
            report.failed_calls += usize::from(*failed);
            latencies.push(*latency_ms);
        }
        TraceEvent::ResourceRead {
            deferred,
            request_bytes,
            result_bytes,
            latency_ms,
            failed,
        } => {
            report.mcpls_calls += 1;
            report.request_bytes += request_bytes;
            report.result_bytes += result_bytes;
            report.failed_calls += usize::from(*failed);
            report.deferred_resource_reads += usize::from(*deferred);
            latencies.push(*latency_ms);
        }
        TraceEvent::SourceRead { output_bytes, .. } => {
            report.shell_source_reads += 1;
            report.source_read_output_bytes += output_bytes;
        }
        TraceEvent::ShellOutput { bytes } => report.shell_output_bytes += bytes,
        TraceEvent::TaskComplete => report.completed_tasks += 1,
        TraceEvent::Compaction => report.compactions += 1,
        TraceEvent::Semantic { .. } => {}
    }
}

fn reset_query_fingerprints_at_task_boundary(
    event: &TraceEvent,
    query_fingerprints: &mut BTreeSet<String>,
) {
    if matches!(event, TraceEvent::TaskComplete) {
        query_fingerprints.clear();
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
        deferred_resource_references,
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
    report.deferred_resource_references += deferred_resource_references;
    report.latency_ms += latency_ms;
    report.truncated += usize::from(*truncated);
    report.unsupported += usize::from(*unsupported);
    report.errors += usize::from(*error);
    report.failed_calls += usize::from(*error);
    report.coordinate_calls += usize::from(*coordinate_input);
    latencies.push(*latency_ms);
    report.semantic_calls_followed_by_shell_read += usize::from(matches!(
        events.get(index + 1),
        Some(TraceEvent::SourceRead { .. })
    ));
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
        let (tool, semantic) = match event {
            TraceEvent::Semantic { tool, .. }
            | TraceEvent::Lifecycle { tool, .. }
            | TraceEvent::McpTool { tool, .. } => (tool.as_str(), true),
            TraceEvent::ResourceRead { .. } => ("read_semantic_resource", false),
            TraceEvent::SourceRead { .. }
            | TraceEvent::ShellOutput { .. }
            | TraceEvent::TaskComplete
            | TraceEvent::Compaction => continue,
        };
        let tool_events = by_tool_events.entry(tool.to_owned()).or_default();
        if semantic
            && let Some(TraceEvent::SourceRead { .. }) = index
                .checked_sub(1)
                .and_then(|previous| events.get(previous))
        {
            tool_events.push(events[index - 1].clone());
        }
        tool_events.push(event.clone());
        if semantic && let Some(TraceEvent::SourceRead { .. }) = events.get(index + 1) {
            tool_events.push(events[index + 1].clone());
        }
    }
    EvaluationReport {
        schema_version: EVALUATION_SCHEMA_VERSION,
        aggregate: classify_trace(events),
        by_tool: by_tool_events
            .into_iter()
            .map(|(tool, events)| (tool, classify_trace(&events)))
            .collect(),
    }
}

pub fn parse_history(reader: impl BufRead) -> Result<Vec<TraceEvent>> {
    let mut events = Vec::new();
    let mut pending_codex_mcp_calls = BTreeMap::new();
    let mut legacy_codex_event_indices = Vec::new();
    let mut current_item_format = false;
    for line in reader.lines() {
        let line = line?;
        let event: Value = serde_json::from_str(&line).context("parsing history JSONL")?;
        if let Some(call) = current_codex_mcp_event(&event) {
            if !current_item_format {
                current_item_format = true;
                for index in std::mem::take(&mut legacy_codex_event_indices)
                    .into_iter()
                    .rev()
                {
                    events.remove(index);
                }
                pending_codex_mcp_calls.clear();
            }
            events.push(call);
        } else if let Some(command_events) = current_codex_command_events(&event) {
            if !current_item_format {
                current_item_format = true;
                for index in std::mem::take(&mut legacy_codex_event_indices)
                    .into_iter()
                    .rev()
                {
                    events.remove(index);
                }
                pending_codex_mcp_calls.clear();
            }
            events.extend(command_events);
        } else if !current_item_format {
            if let Some((call_id, call)) = codex_mcp_call(&event) {
                pending_codex_mcp_calls.insert(call_id, call);
            } else if let Some((call_id, result)) = codex_mcp_result(&event) {
                if let Some(call) = pending_codex_mcp_calls.remove(&call_id) {
                    legacy_codex_event_indices.push(events.len());
                    events.push(mcp_trace_event(&call.tool, &call.arguments, &result, 0));
                }
            } else if let Some(read) = source_read_history_event(&event) {
                legacy_codex_event_indices.push(events.len());
                events.push(read);
            } else if let Some(output) = shell_output_history_event(&event) {
                legacy_codex_event_indices.push(events.len());
                events.push(output);
            }
        }
        if let Some(semantic) = semantic_history_event(&event) {
            events.push(semantic);
        } else if let Some(call) = other_mcpls_history_event(&event) {
            events.push(call);
        } else if task_complete_history_event(&event) {
            events.push(TraceEvent::TaskComplete);
        } else if compaction_history_event(&event) {
            events.push(TraceEvent::Compaction);
        }
    }
    Ok(events)
}

fn current_codex_mcp_event(event: &Value) -> Option<TraceEvent> {
    let payload = event.get("payload")?;
    if payload.get("type")?.as_str()? != "item_completed" {
        return None;
    }
    let item = payload.get("item")?;
    if item.get("type")?.as_str()? != "McpToolCall" || item.get("server")?.as_str()? != "mcpls" {
        return None;
    }
    let mut trace = mcp_trace_event(
        item.get("tool")?.as_str()?,
        item.get("arguments").unwrap_or(&Value::Null),
        item.get("result").unwrap_or(&Value::Null),
        history_latency_ms(item),
    );
    if item_status_is_failure(item) {
        mark_trace_failure(&mut trace);
    }
    Some(trace)
}

const fn mark_trace_failure(trace: &mut TraceEvent) {
    match trace {
        TraceEvent::Semantic { error, .. } => *error = true,
        TraceEvent::Lifecycle { failed, .. }
        | TraceEvent::McpTool { failed, .. }
        | TraceEvent::ResourceRead { failed, .. } => *failed = true,
        TraceEvent::SourceRead { .. }
        | TraceEvent::ShellOutput { .. }
        | TraceEvent::TaskComplete
        | TraceEvent::Compaction => {}
    }
}

fn item_status_is_failure(item: &Value) -> bool {
    item.get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "failed" | "cancelled" | "error"))
}

fn current_codex_command_events(event: &Value) -> Option<Vec<TraceEvent>> {
    let payload = event.get("payload")?;
    if payload.get("type")?.as_str()? != "item_completed" {
        return None;
    }
    let item = payload.get("item")?;
    if item.get("type")?.as_str()? != "CommandExecution" {
        return None;
    }
    let command = item
        .get("command")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    let output_bytes = ["stdout", "stderr"]
        .into_iter()
        .filter_map(|key| item.get(key).and_then(Value::as_str))
        .map(str::len)
        .sum();
    let mut events = Vec::with_capacity(2);
    if let Some(path) = source_read_command_path(&command) {
        events.push(TraceEvent::SourceRead {
            path: scrub_path(path),
            output_bytes,
        });
    }
    events.push(TraceEvent::ShellOutput {
        bytes: output_bytes,
    });
    Some(events)
}

fn source_read_command_path(command: &str) -> Option<&str> {
    let lower = command.to_ascii_lowercase();
    if !["sed ", "rg ", "cat ", "bat ", "read_file", "view_image"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return None;
    }
    source_path_token(command)
}

struct PendingCodexMcpCall {
    tool: String,
    arguments: Value,
}

fn codex_mcp_call(event: &Value) -> Option<(String, PendingCodexMcpCall)> {
    let payload = event.get("payload")?;
    if event.get("type")?.as_str()? != "response_item"
        || payload.get("type")?.as_str()? != "custom_tool_call"
        || payload.get("name")?.as_str()? != "exec"
    {
        return None;
    }
    let input = payload.get("input")?.as_str()?;
    let marker = "tools.mcp__mcpls__";
    let start = input.find(marker)? + marker.len();
    let tool_end = input[start..].find('(')? + start;
    let tool = &input[start..tool_end];
    let arguments_start = input[tool_end..].find('{')? + tool_end;
    let arguments_end = balanced_json_object_end(&input[arguments_start..])? + arguments_start;
    let arguments = parse_codex_object_literal(&input[arguments_start..=arguments_end])?;
    Some((
        payload.get("call_id")?.as_str()?.to_owned(),
        PendingCodexMcpCall {
            tool: tool.to_owned(),
            arguments,
        },
    ))
}

fn parse_codex_object_literal(input: &str) -> Option<Value> {
    let mut json = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
    while index < input.len() {
        let character = input[index..].chars().next()?;
        if in_string {
            json.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            index += character.len_utf8();
            continue;
        }
        if character == '"' {
            in_string = true;
            json.push(character);
            index += 1;
            continue;
        }
        if character.is_ascii_alphabetic() || character == '_' {
            let key_start = index;
            let mut key_end = index + character.len_utf8();
            while key_end < input.len() {
                let next = input[key_end..].chars().next()?;
                if next.is_ascii_alphanumeric() || next == '_' {
                    key_end += next.len_utf8();
                } else {
                    break;
                }
            }
            let previous = input[..key_start]
                .chars()
                .rev()
                .find(|c| !c.is_whitespace());
            let next = input[key_end..].chars().find(|c| !c.is_whitespace());
            if matches!(previous, Some('{' | ',')) && next == Some(':') {
                json.push('"');
                json.push_str(&input[key_start..key_end]);
                json.push('"');
                index = key_end;
                continue;
            }
        }
        json.push(character);
        index += character.len_utf8();
    }
    serde_json::from_str(&json).ok()
}

fn codex_mcp_result(event: &Value) -> Option<(String, Value)> {
    let payload = event.get("payload")?;
    if event.get("type")?.as_str()? != "response_item"
        || payload.get("type")?.as_str()? != "custom_tool_call_output"
    {
        return None;
    }
    let output = payload.get("output")?.as_array()?;
    let text = output
        .iter()
        .rev()
        .find_map(|item| item.get("text").and_then(Value::as_str))?;
    let result = serde_json::from_str(text).ok()?;
    Some((payload.get("call_id")?.as_str()?.to_owned(), result))
}

fn balanced_json_object_end(input: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut escaped = false;
    let mut in_string = false;
    for (index, character) in input.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn mcp_trace_event(tool: &str, arguments: &Value, result: &Value, latency_ms: u64) -> TraceEvent {
    if is_semantic_tool(tool) {
        let source_path = find_path(arguments)
            .or_else(|| find_path(result))
            .map(|path| scrub_path(&path));
        let error = mcp_result_is_error(result);
        return TraceEvent::Semantic {
            tool: tool.to_owned(),
            source_path,
            coordinate_input: arguments.get("line").is_some()
                && arguments.get("symbol_handle").is_none(),
            request_bytes: serialized_len(arguments),
            result_bytes: serialized_len(result),
            query_fingerprint: Some(query_fingerprint(tool, arguments)),
            deferred_bytes: deferred_bytes(result),
            deferred_resource_references: deferred_resource_references(result),
            truncated: contains_true(result, "truncated"),
            latency_ms,
            unsupported: error
                && result
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("unsupported"),
            error,
        };
    }

    let request_bytes = serialized_len(arguments);
    let result_bytes = serialized_len(result);
    if is_lifecycle_tool(tool) {
        return TraceEvent::Lifecycle {
            tool: tool.to_owned(),
            request_bytes,
            result_bytes,
            latency_ms,
            failed: mcp_result_is_error(result),
        };
    }
    if tool == "read_semantic_resource" {
        return TraceEvent::ResourceRead {
            deferred: arguments
                .get("uri")
                .and_then(Value::as_str)
                .is_some_and(|uri| uri.starts_with("mcpls-deferred://")),
            request_bytes,
            result_bytes,
            latency_ms,
            failed: mcp_result_is_error(result),
        };
    }
    TraceEvent::McpTool {
        tool: tool.to_owned(),
        request_bytes,
        result_bytes,
        latency_ms,
        failed: mcp_result_is_error(result),
    }
}

fn mcp_result_is_error(result: &Value) -> bool {
    result.get("Err").is_some()
        || result.get("isError").and_then(Value::as_bool) == Some(true)
        || result.get("error").is_some_and(|error| !error.is_null())
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
    Some(mcp_trace_event(
        tool,
        invocation.get("arguments").unwrap_or(&Value::Null),
        payload.get("result").unwrap_or(&Value::Null),
        history_latency_ms(payload),
    ))
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
    Some(mcp_trace_event(
        tool,
        invocation.get("arguments").unwrap_or(&Value::Null),
        payload.get("result").unwrap_or(&Value::Null),
        history_latency_ms(payload),
    ))
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
    event.get("type").and_then(Value::as_str) == Some("compacted")
        || (event.get("type").and_then(Value::as_str) == Some("event_msg")
            && event
                .get("payload")
                .and_then(|payload| payload.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.contains("compact")))
}

fn task_complete_history_event(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some("event_msg")
        && event
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            == Some("task_complete")
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
    collect_deferred_bytes(value, false)
}

fn deferred_resource_references(value: &Value) -> usize {
    match value {
        Value::Object(object) => {
            let reference = object
                .get("uri")
                .and_then(Value::as_str)
                .is_some_and(|uri| uri.starts_with("mcpls-deferred://"));
            usize::from(reference)
                + object
                    .values()
                    .map(deferred_resource_references)
                    .sum::<usize>()
        }
        Value::Array(values) => values.iter().map(deferred_resource_references).sum(),
        Value::String(text)
            if matches!(text.trim_start().as_bytes().first(), Some(b'{' | b'[')) =>
        {
            serde_json::from_str(text.trim_start())
                .map_or(0, |value| deferred_resource_references(&value))
        }
        _ => 0,
    }
}

fn collect_deferred_bytes(value: &Value, legacy_deferred_entry: bool) -> usize {
    match value {
        Value::Object(object) => {
            let bytes = object
                .get("uri")
                .and_then(Value::as_str)
                .and_then(|_| object.get("total_bytes"))
                .and_then(Value::as_u64)
                .or_else(|| {
                    legacy_deferred_entry
                        .then(|| object.get("bytes"))
                        .flatten()
                        .and_then(Value::as_u64)
                })
                .and_then(|bytes| usize::try_from(bytes).ok())
                .unwrap_or_default();
            object.iter().fold(bytes, |total, (key, value)| {
                total.saturating_add(collect_deferred_bytes(value, key == "deferred"))
            })
        }
        Value::Array(values) => values
            .iter()
            .map(|value| collect_deferred_bytes(value, legacy_deferred_entry))
            .sum(),
        Value::String(text)
            if matches!(text.trim_start().as_bytes().first(), Some(b'{' | b'[')) =>
        {
            serde_json::from_str(text.trim_start()).map_or(0, |value| deferred_bytes(&value))
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
        Value::String(text)
            if matches!(text.trim_start().as_bytes().first(), Some(b'{' | b'[')) =>
        {
            serde_json::from_str(text.trim_start())
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
        Value::String(text)
            if matches!(text.trim_start().as_bytes().first(), Some(b'{' | b'[')) =>
        {
            serde_json::from_str(text.trim_start()).is_ok_and(|value| contains_true(&value, key))
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
            deferred_resource_references: 0,
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
                output_bytes: 7,
            },
            semantic("/private/repo/src/lib.rs", true),
            TraceEvent::SourceRead {
                path: "/private/repo/src/lib.rs".to_owned(),
                output_bytes: 11,
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
                deferred_resource_references: 0,
                deferred_resource_reads: 0,
                source_read_output_bytes: 18,
                shell_output_bytes: 0,
                shell_source_reads: 3,
                semantic_calls_followed_by_shell_read: 2,
                completed_tasks: 0,
                calls_per_completed_task: Rate {
                    numerator: 2,
                    denominator: 0,
                },
                semantic_to_shell_read_rate: Rate {
                    numerator: 2,
                    denominator: 2,
                },
                deferred_resource_follow_through_rate: Rate {
                    numerator: 0,
                    denominator: 0,
                },
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
                failed_calls: 0,
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
                failure_rate: Rate {
                    numerator: 0,
                    denominator: 2
                },
            }
        );
    }

    #[test]
    fn history_parser_emits_only_scrubbed_semantic_and_source_read_events() {
        let history = concat!(
            r#"{"type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"mcpls","tool":"workspace_symbol_search","arguments":{"query":"private"}},"duration":{"secs":0,"nanos":12000000},"result":{"Ok":{"content":[{"text":" \n{\"locations\":[{\"path\":\"/home/alice/private/src/lib.rs\",\"truncated\":true}]}"}]}}}}"#,
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
        assert_eq!(evaluate(&events).aggregate.truncated, 1);
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
    fn history_parser_reads_current_codex_mcp_exec_records() {
        let call = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "call-search",
                "input": "const result = await tools.mcp__mcpls__workspace_symbol_search({project_id: \"fixture\", query: \"private_type\", max_bytes: 4096}); text(JSON.stringify(result));"
            }
        });
        let output = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": "call-search",
                "output": [
                    {"type": "input_text", "text": "Script completed"},
                    {"type": "input_text", "text": serde_json::json!({
                        "structuredContent": {
                            "truncated": true,
                            "resource": {"uri": "mcpls-source://fixture", "total_bytes": 55}
                        },
                        "isError": false
                    }).to_string()}
                ]
            }
        });
        let lifecycle_call = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "call-add",
                "input": "const result = await tools.mcp__mcpls__project_add({root: \"/home/alice/fixture\"}); text(JSON.stringify(result));"
            }
        });
        let lifecycle_output = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": "call-add",
                "output": [{"type": "input_text", "text": "{}"}]
            }
        });
        let history = [call, output, lifecycle_call, lifecycle_output]
            .into_iter()
            .map(|event| event.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        let report = evaluate(&parse_history(history.as_bytes()).unwrap()).aggregate;

        assert_eq!(report.mcpls_calls, 2);
        assert_eq!(report.semantic_calls, 1);
        assert_eq!(report.lifecycle_calls, 1);
        assert_eq!(report.truncated, 1);
        assert_eq!(report.deferred_bytes, 55);
        assert!(report.request_bytes > 0);
        assert!(report.result_bytes > 0);
    }

    #[test]
    fn history_parser_reads_current_completed_items_with_timing_and_shell_bytes() {
        let mcp = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "item_completed",
                "item": {
                    "type": "McpToolCall",
                    "server": "mcpls",
                    "tool": "workspace_symbol_search",
                    "arguments": {
                        "project_id": "fixture",
                        "file_path": "/home/alice/fixture/src/lib.rs",
                        "query": "private_type",
                        "max_bytes": 4096
                    },
                    "duration": {"secs": 1, "nanos": 250000000},
                    "status": "failed",
                    "result": {
                        "structuredContent": {
                            "truncated": true,
                            "resource": {"uri": "mcpls-source://fixture", "total_bytes": 55}
                        },
                        "isError": false
                    }
                }
            }
        });
        let command = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "item_completed",
                "item": {
                    "type": "CommandExecution",
                    "command": ["zsh", "-lc", "sed -n 1,10p /home/alice/fixture/src/lib.rs"],
                    "stdout": "source\n",
                    "stderr": ""
                }
            }
        });
        let history = [mcp, command]
            .into_iter()
            .map(|event| event.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        let events = parse_history(history.as_bytes()).unwrap();
        let report = evaluate(&events).aggregate;

        assert_eq!(report.mcpls_calls, 1);
        assert_eq!(report.semantic_calls, 1);
        assert_eq!(report.result_bytes, 117);
        assert_eq!(report.deferred_bytes, 55);
        assert_eq!(report.latency_ms, 1_250);
        assert_eq!(report.latency.p50_ms, 1_250);
        assert_eq!(report.errors, 1);
        assert_eq!(report.shell_output_bytes, 7);
        assert_eq!(report.source_read_output_bytes, 7);
        assert_eq!(report.post_semantic_same_file_reads, 1);
        assert!(
            events
                .iter()
                .all(|event| !format!("{event:?}").contains("alice"))
        );
    }

    #[test]
    fn history_parser_counts_failed_non_semantic_mcpls_items() {
        let history = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "item_completed",
                "item": {
                    "type": "McpToolCall",
                    "server": "mcpls",
                    "tool": "project_status",
                    "arguments": {"project_id": "fixture"},
                    "duration": {"secs": 0, "nanos": 1000000},
                    "status": "failed",
                    "result": {"isError": false}
                }
            }
        });

        let report = evaluate(&parse_history(history.to_string().as_bytes()).unwrap()).aggregate;

        assert_eq!(report.mcpls_calls, 1);
        assert_eq!(report.semantic_calls, 0);
        assert_eq!(report.errors, 0);
        assert_eq!(report.failed_calls, 1);
        assert_eq!(
            report.failure_rate,
            Rate {
                numerator: 1,
                denominator: 1,
            }
        );
    }

    #[test]
    fn history_parser_counts_current_top_level_compaction_records() {
        let history = r#"{"type":"compacted","thread_id":"opaque"}"#;

        let events = parse_history(history.as_bytes()).unwrap();

        assert_eq!(evaluate(&events).aggregate.compactions, 1);
    }

    #[test]
    fn history_parser_counts_completed_tasks_without_retaining_task_data() {
        let history =
            r#"{"type":"event_msg","payload":{"type":"task_complete","thread_id":"opaque"}}"#;

        let events = parse_history(history.as_bytes()).unwrap();
        let report = evaluate(&events).aggregate;

        assert_eq!(evaluate(&events).schema_version, EVALUATION_SCHEMA_VERSION);
        assert_eq!(report.completed_tasks, 1);
        assert_eq!(report.calls_per_completed_task.numerator, 0);
        assert_eq!(report.calls_per_completed_task.denominator, 1);
        assert!(
            events
                .iter()
                .all(|event| !format!("{event:?}").contains("opaque"))
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
    fn deferred_bytes_counts_snapshot_resource_references() {
        let reference = serde_json::json!({
            "result": {
                "resource": {
                    "uri": "mcpls-source://opaque",
                    "total_bytes": 55
                }
            }
        });

        assert_eq!(deferred_bytes(&reference), 55);
        assert_eq!(
            deferred_bytes(&Value::String(format!(" \n{reference}"))),
            55
        );
    }

    #[test]
    fn deferred_resource_follow_through_is_reported_as_a_rate() {
        let result = serde_json::json!({
            "content": [
                {"resource": {"uri": "mcpls-deferred:///first", "total_bytes": 10}},
                {"resource": {"uri": "mcpls-deferred:///second", "total_bytes": 20}}
            ]
        });
        let events = [
            mcp_trace_event("inspect_symbol", &Value::Null, &result, 10),
            mcp_trace_event(
                "read_semantic_resource",
                &serde_json::json!({"uri": "mcpls-deferred:///first"}),
                &Value::Null,
                2,
            ),
        ];

        let report = classify_trace(&events);

        assert_eq!(report.deferred_resource_references, 2);
        assert_eq!(report.deferred_resource_reads, 1);
        assert_eq!(
            report.deferred_resource_follow_through_rate,
            Rate {
                numerator: 1,
                denominator: 2,
            }
        );
    }

    #[test]
    fn evaluation_groups_lifecycle_and_resource_calls_by_tool() {
        let events = [
            mcp_trace_event("project_add", &Value::Null, &Value::Null, 3),
            mcp_trace_event(
                "read_semantic_resource",
                &serde_json::json!({"uri": "mcpls-deferred:///opaque"}),
                &Value::Null,
                2,
            ),
        ];

        let report = evaluate(&events);

        assert_eq!(report.by_tool["project_add"].lifecycle_calls, 1);
        assert_eq!(report.by_tool["read_semantic_resource"].mcpls_calls, 1);
        assert_eq!(
            report.by_tool["read_semantic_resource"].deferred_resource_reads,
            1
        );
    }

    #[test]
    fn duplicate_queries_are_scoped_to_a_completed_task() {
        let first = mcp_trace_event(
            "workspace_symbol_search",
            &serde_json::json!({"query": "same"}),
            &Value::Null,
            1,
        );
        let second = mcp_trace_event(
            "workspace_symbol_search",
            &serde_json::json!({"query": "same"}),
            &Value::Null,
            1,
        );

        let report = classify_trace(&[first, TraceEvent::TaskComplete, second]);

        assert_eq!(report.duplicate_queries, 0);
        assert_eq!(report.completed_tasks, 1);
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
