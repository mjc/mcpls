use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::BufRead;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TraceEvent {
    Semantic {
        tool: String,
        source_path: Option<String>,
        coordinate_input: bool,
        result_bytes: usize,
        truncated: bool,
        latency_ms: u64,
        unsupported: bool,
        error: bool,
    },
    SourceRead {
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceReport {
    pub semantic_calls: usize,
    pub post_semantic_same_file_reads: usize,
    pub pre_coordinate_source_reads: usize,
    pub coordinate_calls: usize,
    pub result_bytes: usize,
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
        semantic_calls: 0,
        post_semantic_same_file_reads: 0,
        pre_coordinate_source_reads: 0,
        coordinate_calls: 0,
        result_bytes: 0,
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

    for (index, event) in events.iter().enumerate() {
        let TraceEvent::Semantic {
            source_path,
            coordinate_input,
            result_bytes,
            truncated,
            latency_ms,
            unsupported,
            error,
            ..
        } = event
        else {
            continue;
        };
        report.semantic_calls += 1;
        report.result_bytes += result_bytes;
        report.latency_ms += latency_ms;
        report.truncated += usize::from(*truncated);
        report.unsupported += usize::from(*unsupported);
        report.errors += usize::from(*error);
        report.coordinate_calls += usize::from(*coordinate_input);

        let same_path = |event: Option<&TraceEvent>| {
            matches!(
                (source_path.as_deref(), event),
                (Some(source), Some(TraceEvent::SourceRead { path }))
                    if scrub_path(source) == scrub_path(path)
            )
        };
        report.post_semantic_same_file_reads += usize::from(same_path(events.get(index + 1)));
        report.pre_coordinate_source_reads += usize::from(
            *coordinate_input && same_path(index.checked_sub(1).and_then(|i| events.get(i))),
        );
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
    report
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
        } else if let Some(read) = source_read_history_event(&event) {
            events.push(read);
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
        result_bytes: serialized.len(),
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
    })
}

fn source_path_token(input: &str) -> Option<&str> {
    input
        .split(|character: char| {
            character.is_whitespace()
                || matches!(character, '"' | '\'' | '`' | ',' | ';' | ')' | '(')
        })
        .map(|token| token.trim_end_matches([':', '\\', 'n']))
        .find(|token| {
            [".rs", ".py", ".go", ".ts", ".js", ".c", ".cpp"]
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
            result_bytes: 512,
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
            },
            semantic("/private/repo/src/lib.rs", true),
            TraceEvent::SourceRead {
                path: "/private/repo/src/lib.rs".to_owned(),
            },
            semantic("/private/repo/src/other.rs", false),
            TraceEvent::SourceRead {
                path: "/private/repo/src/unrelated.rs".to_owned(),
            },
        ];

        assert_eq!(
            classify_trace(&events),
            TraceReport {
                semantic_calls: 2,
                post_semantic_same_file_reads: 1,
                pre_coordinate_source_reads: 1,
                coordinate_calls: 1,
                result_bytes: 1024,
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
