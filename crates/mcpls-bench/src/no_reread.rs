use serde::{Deserialize, Serialize};

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
}
