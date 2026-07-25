//! LSP notification storage and management.
//!
//! Stores diagnostics, log messages, and server messages received from LSP servers.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use lsp_types::{Diagnostic as LspDiagnostic, Uri};
use serde::{Deserialize, Serialize};

/// Maximum number of log entries to store.
const MAX_LOG_ENTRIES: usize = 100;

/// Normalize a URI string to a stable cache key.
///
/// On Windows, URI comparisons must be case-insensitive: the filesystem is
/// case-insensitive and different tools (e.g. rust-analyzer vs std) may
/// produce drive letters in different cases (`C:` vs `c:`).
/// Lowercasing the entire URI is safe for `file://` URIs because they have
/// no case-sensitive query or fragment components.
fn uri_cache_key(uri: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(windows) {
        std::borrow::Cow::Owned(uri.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(uri)
    }
}

/// Maximum number of server messages to store.
const MAX_SERVER_MESSAGES: usize = 50;

fn push_bounded<T>(queue: &mut VecDeque<T>, value: T, capacity: usize) {
    if queue.len() >= capacity {
        queue.pop_front();
    }
    queue.push_back(value);
}

/// Redacts configured secret values from server output.
#[derive(Debug, Clone, Default)]
pub struct RedactionPolicy {
    secrets: Vec<String>,
}

impl RedactionPolicy {
    /// Build a policy from configured non-empty secret values.
    #[must_use]
    pub fn from_secrets<I>(secrets: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        Self {
            secrets: secrets
                .into_iter()
                .filter(|secret| !secret.is_empty())
                .collect(),
        }
    }

    /// Replace every configured secret in one server message.
    #[must_use]
    pub fn redact(&self, message: &str) -> String {
        let message = redact_bearer_tokens(message);
        let message = redact_sensitive_assignments(&message);
        self.secrets.iter().fold(message, |message, secret| {
            message.replace(secret, "[REDACTED]")
        })
    }
}

const SENSITIVE_KEYS: &[&str] = &[
    "access_key",
    "api_key",
    "apikey",
    "authorization",
    "credential",
    "password",
    "private_key",
    "secret",
    "token",
];

fn is_boundary_before(bytes: &[u8], index: usize) -> bool {
    index == 0 || !bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b'_'
}

fn is_boundary_after(bytes: &[u8], index: usize) -> bool {
    index == bytes.len() || !bytes[index].is_ascii_alphanumeric() && bytes[index] != b'_'
}

fn value_end(bytes: &[u8], start: usize) -> usize {
    if let Some(quote) = bytes
        .get(start)
        .copied()
        .filter(|byte| *byte == b'"' || *byte == b'\'')
    {
        return bytes[start + 1..]
            .iter()
            .position(|byte| *byte == quote)
            .map_or(bytes.len(), |offset| start + 1 + offset);
    }

    bytes[start..]
        .iter()
        .position(|byte| byte.is_ascii_whitespace() || b",;}]\")".contains(byte))
        .map_or(bytes.len(), |offset| start + offset)
}

fn redact_sensitive_assignments(message: &str) -> String {
    let lowercase = message.to_ascii_lowercase();
    let bytes = message.as_bytes();
    let lowercase_bytes = lowercase.as_bytes();
    let mut output = String::with_capacity(message.len());
    let mut copied_until = 0;
    let mut cursor = 0;

    while cursor < message.len() {
        let Some((key_start, key_end)) = SENSITIVE_KEYS
            .iter()
            .filter_map(|key| {
                lowercase_bytes[cursor..]
                    .windows(key.len())
                    .position(|window| window == key.as_bytes())
                    .map(|offset| {
                        let start = cursor + offset;
                        (start, start + key.len())
                    })
            })
            .min_by_key(|(start, _)| *start)
        else {
            break;
        };
        if !is_boundary_before(bytes, key_start)
            || !is_boundary_after(bytes, key_end)
            || !bytes[key_end..]
                .iter()
                .copied()
                .find(|byte| !byte.is_ascii_whitespace())
                .is_some_and(|byte| byte == b'=' || byte == b':')
        {
            cursor = key_end;
            continue;
        }

        let Some(separator) = bytes[key_end..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map(|offset| key_end + offset)
        else {
            cursor = key_end;
            continue;
        };
        let value_start = bytes[separator + 1..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map_or(bytes.len(), |offset| separator + 1 + offset);
        let value_end = value_end(bytes, value_start);
        if value_start >= value_end {
            cursor = value_start;
            continue;
        }

        output.push_str(&message[copied_until..value_start]);
        output.push_str("[REDACTED]");
        copied_until = value_end;
        cursor = value_end;
    }

    output.push_str(&message[copied_until..]);
    output
}

fn redact_bearer_tokens(message: &str) -> String {
    let lowercase = message.to_ascii_lowercase();
    let bytes = message.as_bytes();
    let lowercase_bytes = lowercase.as_bytes();
    let mut output = String::with_capacity(message.len());
    let mut copied_until = 0;
    let mut cursor = 0;

    while cursor < message.len() {
        let Some(offset) = lowercase_bytes[cursor..]
            .windows("bearer".len())
            .position(|window| window == b"bearer")
        else {
            break;
        };
        let key_start = cursor + offset;
        let key_end = key_start + "bearer".len();
        if !is_boundary_before(bytes, key_start) || !is_boundary_after(bytes, key_end) {
            cursor = key_end;
            continue;
        }

        let value_start = bytes[key_end..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map_or(key_end, |offset| key_end + offset);
        let value_end = bytes[value_start..]
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || b",;}]\")".contains(byte))
            .map_or(bytes.len(), |offset| value_start + offset);
        if value_start >= value_end {
            cursor = value_start;
            continue;
        }

        output.push_str(&message[copied_until..value_start]);
        output.push_str("[REDACTED]");
        copied_until = value_end;
        cursor = value_end;
    }

    output.push_str(&message[copied_until..]);
    output
}

/// Information about diagnostics for a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticInfo {
    /// URI of the document.
    pub uri: Uri,
    /// Document version when diagnostics were received.
    pub version: Option<i32>,
    /// List of diagnostics.
    pub diagnostics: Vec<LspDiagnostic>,
}

/// A log entry from the LSP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Actor/LSP lifecycle generation that produced the entry.
    pub generation: u64,
    /// Log level.
    pub level: LogLevel,
    /// Log message.
    pub message: String,
    /// Timestamp when the log was received.
    pub timestamp: DateTime<Utc>,
}

/// Log severity level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Error log level.
    Error,
    /// Warning log level.
    Warning,
    /// Info log level.
    Info,
    /// Debug log level.
    Debug,
}

impl From<lsp_types::MessageType> for LogLevel {
    fn from(msg_type: lsp_types::MessageType) -> Self {
        match msg_type {
            lsp_types::MessageType::ERROR => Self::Error,
            lsp_types::MessageType::WARNING => Self::Warning,
            lsp_types::MessageType::INFO => Self::Info,
            // LOG and unknown message types default to Debug
            _ => Self::Debug,
        }
    }
}

/// A message from the LSP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMessage {
    /// Actor/LSP lifecycle generation that produced the message.
    pub generation: u64,
    /// Message type.
    pub message_type: MessageType,
    /// Message content.
    pub message: String,
    /// Timestamp when the message was received.
    pub timestamp: DateTime<Utc>,
}

/// Server message type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageType {
    /// Error message.
    Error,
    /// Warning message.
    Warning,
    /// Info message.
    Info,
    /// Log message.
    Log,
}

impl From<lsp_types::MessageType> for MessageType {
    fn from(msg_type: lsp_types::MessageType) -> Self {
        match msg_type {
            lsp_types::MessageType::ERROR => Self::Error,
            lsp_types::MessageType::WARNING => Self::Warning,
            lsp_types::MessageType::INFO => Self::Info,
            // LOG and unknown message types default to Log
            _ => Self::Log,
        }
    }
}

/// Cache for LSP server notifications.
#[derive(Debug)]
pub struct NotificationCache {
    /// Diagnostics indexed by document URI.
    diagnostics: HashMap<String, DiagnosticInfo>,
    /// Recent log entries (FIFO queue with max size).
    logs: VecDeque<LogEntry>,
    /// Recent server messages (FIFO queue with max size).
    messages: VecDeque<ServerMessage>,
}

impl Default for NotificationCache {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationCache {
    /// Create a new notification cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            diagnostics: HashMap::with_capacity(32),
            logs: VecDeque::with_capacity(MAX_LOG_ENTRIES),
            messages: VecDeque::with_capacity(MAX_SERVER_MESSAGES),
        }
    }

    /// Store diagnostics for a document.
    ///
    /// If diagnostics already exist for the URI, they are replaced.
    pub fn store_diagnostics(
        &mut self,
        uri: &Uri,
        version: Option<i32>,
        diagnostics: Vec<LspDiagnostic>,
    ) {
        let info = DiagnosticInfo {
            uri: uri.clone(),
            version,
            diagnostics,
        };
        self.diagnostics
            .insert(uri_cache_key(uri.as_str()).into_owned(), info);
    }

    /// Store a log entry.
    ///
    /// Maintains a maximum of `MAX_LOG_ENTRIES` entries, removing oldest when full.
    pub fn store_log(&mut self, level: LogLevel, message: String) {
        self.store_log_with_generation(0, level, message);
    }

    /// Store a log entry associated with an actor/LSP lifecycle generation.
    pub fn store_log_with_generation(&mut self, generation: u64, level: LogLevel, message: String) {
        let entry = LogEntry {
            generation,
            level,
            message,
            timestamp: Utc::now(),
        };

        push_bounded(&mut self.logs, entry, MAX_LOG_ENTRIES);
    }

    /// Store a server message.
    ///
    /// Maintains a maximum of `MAX_SERVER_MESSAGES` entries, removing oldest when full.
    pub fn store_message(&mut self, message_type: MessageType, message: String) {
        self.store_message_with_generation(0, message_type, message);
    }

    /// Store a server message associated with an actor/LSP lifecycle generation.
    pub fn store_message_with_generation(
        &mut self,
        generation: u64,
        message_type: MessageType,
        message: String,
    ) {
        let msg = ServerMessage {
            generation,
            message_type,
            message,
            timestamp: Utc::now(),
        };

        push_bounded(&mut self.messages, msg, MAX_SERVER_MESSAGES);
    }

    /// Get diagnostics for a document URI.
    #[inline]
    #[must_use]
    pub fn get_diagnostics(&self, uri: &str) -> Option<&DiagnosticInfo> {
        self.diagnostics.get(uri_cache_key(uri).as_ref())
    }

    /// Return whether diagnostics have been received for a document URI.
    #[must_use]
    pub fn contains_diagnostics(&self, uri: &str) -> bool {
        self.diagnostics.contains_key(uri_cache_key(uri).as_ref())
    }

    /// Get all stored log entries.
    #[inline]
    #[must_use]
    pub const fn get_logs(&self) -> &VecDeque<LogEntry> {
        &self.logs
    }

    /// Get all stored server messages.
    #[inline]
    #[must_use]
    pub const fn get_messages(&self) -> &VecDeque<ServerMessage> {
        &self.messages
    }

    /// Clear diagnostics for a specific document URI.
    ///
    /// Returns the cleared diagnostics if they existed.
    pub fn clear_diagnostics(&mut self, uri: &str) -> Option<DiagnosticInfo> {
        self.diagnostics.remove(uri_cache_key(uri).as_ref())
    }

    /// Clear all diagnostics.
    pub fn clear_all_diagnostics(&mut self) {
        self.diagnostics.clear();
    }

    /// Clear all logs.
    pub fn clear_logs(&mut self) {
        self.logs.clear();
    }

    /// Clear all messages.
    pub fn clear_messages(&mut self) {
        self.messages.clear();
    }

    /// Get the number of documents with stored diagnostics.
    #[inline]
    #[must_use]
    pub fn diagnostics_count(&self) -> usize {
        self.diagnostics.len()
    }

    /// Get the number of stored log entries.
    #[inline]
    #[must_use]
    pub fn logs_count(&self) -> usize {
        self.logs.len()
    }

    /// Get the number of stored server messages.
    #[inline]
    #[must_use]
    pub fn messages_count(&self) -> usize {
        self.messages.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use lsp_types::{Position, Range};

    use super::*;

    #[test]
    fn test_notification_cache_new() {
        let cache = NotificationCache::new();
        assert_eq!(cache.diagnostics_count(), 0);
        assert_eq!(cache.logs_count(), 0);
        assert_eq!(cache.messages_count(), 0);
    }

    #[test]
    fn test_store_and_get_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let diagnostic = LspDiagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&uri, Some(1), vec![diagnostic]);

        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.uri, uri);
        assert_eq!(stored.version, Some(1));
        assert_eq!(stored.diagnostics.len(), 1);
        assert_eq!(stored.diagnostics[0].message, "test error");
    }

    #[test]
    fn test_store_diagnostics_replaces_existing() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&uri, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        cache.store_diagnostics(&uri, Some(2), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.version, Some(2));
    }

    #[test]
    fn test_clear_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&uri, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        let cleared = cache.clear_diagnostics(uri.as_str());
        assert!(cleared.is_some());
        assert_eq!(cache.diagnostics_count(), 0);
    }

    #[test]
    fn test_clear_all_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri1: Uri = "file:///test1.rs".parse().unwrap();
        let uri2: Uri = "file:///test2.rs".parse().unwrap();

        cache.store_diagnostics(&uri1, Some(1), vec![]);
        cache.store_diagnostics(&uri2, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 2);

        cache.clear_all_diagnostics();
        assert_eq!(cache.diagnostics_count(), 0);
    }

    #[test]
    fn test_store_and_get_logs() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error message".to_string());
        cache.store_log(LogLevel::Info, "info message".to_string());

        let logs = cache.get_logs();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].level, LogLevel::Error);
        assert_eq!(logs[0].message, "error message");
        assert_eq!(logs[1].level, LogLevel::Info);
        assert_eq!(logs[1].message, "info message");
    }

    #[test]
    fn test_store_log_preserves_generation() {
        let mut cache = NotificationCache::new();
        cache.store_log_with_generation(7, LogLevel::Info, "generation".to_string());
        assert_eq!(cache.get_logs().front().unwrap().generation, 7);
    }

    #[test]
    fn test_logs_max_capacity() {
        let mut cache = NotificationCache::new();

        // Add more than MAX_LOG_ENTRIES
        for i in 0..MAX_LOG_ENTRIES + 10 {
            cache.store_log(LogLevel::Info, format!("message {i}"));
        }

        assert_eq!(cache.logs_count(), MAX_LOG_ENTRIES);

        // Oldest entries should be removed (FIFO)
        let logs = cache.get_logs();
        assert_eq!(logs.front().unwrap().message, "message 10");
        assert_eq!(
            logs.back().unwrap().message,
            format!("message {}", MAX_LOG_ENTRIES + 9)
        );
    }

    #[test]
    fn test_clear_logs() {
        let mut cache = NotificationCache::new();
        cache.store_log(LogLevel::Info, "test".to_string());
        assert_eq!(cache.logs_count(), 1);

        cache.clear_logs();
        assert_eq!(cache.logs_count(), 0);
    }

    #[test]
    fn test_store_and_get_messages() {
        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Error, "error msg".to_string());
        cache.store_message(MessageType::Warning, "warning msg".to_string());

        let messages = cache.get_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message_type, MessageType::Error);
        assert_eq!(messages[0].message, "error msg");
        assert_eq!(messages[1].message_type, MessageType::Warning);
        assert_eq!(messages[1].message, "warning msg");
    }

    #[test]
    fn test_store_message_preserves_generation() {
        let mut cache = NotificationCache::new();
        cache.store_message_with_generation(9, MessageType::Info, "generation".to_string());
        assert_eq!(cache.get_messages().front().unwrap().generation, 9);
    }

    #[test]
    fn test_messages_max_capacity() {
        let mut cache = NotificationCache::new();

        // Add more than MAX_SERVER_MESSAGES
        for i in 0..MAX_SERVER_MESSAGES + 10 {
            cache.store_message(MessageType::Info, format!("message {i}"));
        }

        assert_eq!(cache.messages_count(), MAX_SERVER_MESSAGES);

        // Oldest entries should be removed (FIFO)
        let messages = cache.get_messages();
        assert_eq!(messages.front().unwrap().message, "message 10");
        assert_eq!(
            messages.back().unwrap().message,
            format!("message {}", MAX_SERVER_MESSAGES + 9)
        );
    }

    #[test]
    fn test_clear_messages() {
        let mut cache = NotificationCache::new();
        cache.store_message(MessageType::Info, "test".to_string());
        assert_eq!(cache.messages_count(), 1);

        cache.clear_messages();
        assert_eq!(cache.messages_count(), 0);
    }

    #[test]
    fn test_log_levels() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error".to_string());
        cache.store_log(LogLevel::Warning, "warning".to_string());
        cache.store_log(LogLevel::Info, "info".to_string());
        cache.store_log(LogLevel::Debug, "debug".to_string());

        let logs = cache.get_logs();
        assert_eq!(logs[0].level, LogLevel::Error);
        assert_eq!(logs[1].level, LogLevel::Warning);
        assert_eq!(logs[2].level, LogLevel::Info);
        assert_eq!(logs[3].level, LogLevel::Debug);
    }

    #[test]
    fn test_message_types() {
        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Error, "error".to_string());
        cache.store_message(MessageType::Warning, "warning".to_string());
        cache.store_message(MessageType::Info, "info".to_string());
        cache.store_message(MessageType::Log, "log".to_string());

        let messages = cache.get_messages();
        assert_eq!(messages[0].message_type, MessageType::Error);
        assert_eq!(messages[1].message_type, MessageType::Warning);
        assert_eq!(messages[2].message_type, MessageType::Info);
        assert_eq!(messages[3].message_type, MessageType::Log);
    }

    #[test]
    fn test_timestamp_ordering() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Info, "first".to_string());
        std::thread::sleep(std::time::Duration::from_millis(10));
        cache.store_log(LogLevel::Info, "second".to_string());

        let logs = cache.get_logs();
        assert!(logs[0].timestamp < logs[1].timestamp);
    }

    #[test]
    fn test_store_diagnostics_empty_list() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let diagnostic = LspDiagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&uri, Some(1), vec![diagnostic]);
        assert_eq!(
            cache
                .get_diagnostics(uri.as_str())
                .unwrap()
                .diagnostics
                .len(),
            1
        );

        cache.store_diagnostics(&uri, Some(2), vec![]);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.diagnostics.len(), 0);
        assert_eq!(stored.version, Some(2));
    }

    #[test]
    fn test_store_many_diagnostics_single_file() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let diagnostics: Vec<LspDiagnostic> = (0..100)
            .map(|i| LspDiagnostic {
                range: Range {
                    start: Position {
                        line: i,
                        character: 0,
                    },
                    end: Position {
                        line: i,
                        character: 10,
                    },
                },
                message: format!("Error {i}"),
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            })
            .collect();

        cache.store_diagnostics(&uri, Some(1), diagnostics);

        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.diagnostics.len(), 100);
    }

    #[test]
    fn test_logs_exact_capacity_boundary() {
        let mut cache = NotificationCache::new();

        for i in 0..MAX_LOG_ENTRIES {
            cache.store_log(LogLevel::Info, format!("message {i}"));
        }
        assert_eq!(cache.logs_count(), MAX_LOG_ENTRIES);

        cache.store_log(LogLevel::Info, "overflow".to_string());
        assert_eq!(cache.logs_count(), MAX_LOG_ENTRIES);
        assert_eq!(cache.get_logs().front().unwrap().message, "message 1");
    }

    #[test]
    fn test_messages_exact_capacity_boundary() {
        let mut cache = NotificationCache::new();

        for i in 0..MAX_SERVER_MESSAGES {
            cache.store_message(MessageType::Info, format!("message {i}"));
        }
        assert_eq!(cache.messages_count(), MAX_SERVER_MESSAGES);

        cache.store_message(MessageType::Info, "overflow".to_string());
        assert_eq!(cache.messages_count(), MAX_SERVER_MESSAGES);
        assert_eq!(cache.get_messages().front().unwrap().message, "message 1");
    }

    #[test]
    fn test_clear_diagnostics_nonexistent() {
        let mut cache = NotificationCache::new();
        let result = cache.clear_diagnostics("file:///nonexistent.rs");
        assert!(result.is_none());
    }

    #[test]
    fn test_store_diagnostics_no_version() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&uri, None, vec![]);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.version, None);
    }
}
