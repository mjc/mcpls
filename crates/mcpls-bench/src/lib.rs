#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value
)]

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

pub mod no_reread;

pub const RUST_ANALYZER_NAMES: [&str; 2] = ["rust-analyzer", "rust_analyzer"];

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub projects: Vec<ProjectGroupInput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectGroupInput {
    pub project_id: String,
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ProjectGroup {
    pub project_id: String,
    pub roots: Vec<PathBuf>,
}

pub fn load_manifest(path: &Path) -> Result<Vec<ProjectGroup>> {
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("reading manifest {}", path.display()))?,
    )
    .with_context(|| format!("parsing manifest {}", path.display()))?;
    if manifest.projects.len() != 4 {
        bail!("manifest must contain exactly four projects");
    }

    let mut seen_ids = HashSet::new();
    let mut seen_roots = HashSet::new();
    let mut groups = Vec::with_capacity(4);
    for project in manifest.projects {
        if project.project_id.is_empty() || !seen_ids.insert(project.project_id.clone()) {
            bail!("each project must have a unique non-empty project_id");
        }
        if project.roots.len() != 5 {
            bail!(
                "project {} must contain exactly five roots",
                project.project_id
            );
        }
        let mut roots = Vec::with_capacity(5);
        let mut common_dir = None;
        for root in project.roots {
            let root = fs::canonicalize(&root)
                .with_context(|| format!("canonicalizing root {}", root.display()))?;
            if !seen_roots.insert(root.clone()) {
                bail!("root appears more than once: {}", root.display());
            }
            if !root.join("Cargo.toml").is_file() {
                bail!("root has no Cargo.toml: {}", root.display());
            }
            let root_common_dir = git_common_dir(&root)?;
            if common_dir
                .as_ref()
                .is_some_and(|dir| dir != &root_common_dir)
            {
                bail!(
                    "project {} roots must be linked Git worktrees",
                    project.project_id
                );
            }
            common_dir = Some(root_common_dir);
            if !["rust-toolchain", "rust-toolchain.toml"]
                .iter()
                .any(|name| root.join(name).is_file())
            {
                bail!("root has no explicit Rust toolchain: {}", root.display());
            }
            roots.push(root);
        }
        groups.push(ProjectGroup {
            project_id: project.project_id,
            roots,
        });
    }
    Ok(groups)
}

pub fn git_common_dir(root: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["-C", root.to_str().unwrap_or_default()])
        .args([
            "rev-parse",
            "--show-toplevel",
            "--git-common-dir",
            "--is-inside-work-tree",
        ])
        .output()
        .with_context(|| format!("running git for {}", root.display()))?;
    if !output.status.success() {
        bail!("root is not a Git worktree: {}", root.display());
    }
    let lines = String::from_utf8(output.stdout)?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if lines.len() != 3 || lines[2] != "true" {
        bail!("root is not a Git worktree: {}", root.display());
    }
    let common = PathBuf::from(&lines[1]);
    if common.is_absolute() {
        Ok(common.canonicalize()?)
    } else {
        Path::new(&lines[0])
            .join(common)
            .canonicalize()
            .map_err(Into::into)
    }
}

pub fn descendants(pid: u32) -> Vec<u32> {
    let mut pending = vec![pid];
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    while let Some(current) = pending.pop() {
        if !seen.insert(current) {
            continue;
        }
        result.push(current);
        let Ok(tasks) = fs::read_dir(format!("/proc/{current}/task")) else {
            continue;
        };
        for task in tasks.flatten() {
            let Ok(children) = fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            pending.extend(
                children
                    .split_whitespace()
                    .filter_map(|value| value.parse::<u32>().ok()),
            );
        }
    }
    result
}

pub fn pss_kib(process_ids: &[u32]) -> u64 {
    process_ids
        .iter()
        .filter_map(|pid| fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok())
        .filter_map(|contents| {
            contents
                .lines()
                .find(|line| line.starts_with("Pss:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u64>().ok())
        })
        .sum()
}

pub fn process_names(process_ids: &[u32]) -> Vec<String> {
    let mut names = process_ids
        .iter()
        .filter_map(|pid| fs::read_to_string(format!("/proc/{pid}/comm")).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    names.sort();
    names
}

pub fn rust_analyzer_count(names: &[String]) -> usize {
    names
        .iter()
        .filter(|name| RUST_ANALYZER_NAMES.contains(&name.as_str()))
        .count()
}

pub fn resource_snapshot(pid: u32) -> Value {
    let ids = descendants(pid);
    let names = process_names(&ids);
    serde_json::json!({
        "pss_kib": pss_kib(&ids),
        "processes": {
            "process_count": names.len(),
            "process_names": names,
            "rust_analyzer_count": rust_analyzer_count(&names),
        }
    })
}

pub fn validate_process_snapshot(
    analyzer_count: usize,
    active_group_count: usize,
    max_active_groups: usize,
) -> Result<()> {
    if active_group_count > max_active_groups {
        bail!("resident group limit exceeded: {active_group_count} active groups");
    }
    if analyzer_count > max_active_groups {
        bail!("rust-analyzer process limit exceeded: {analyzer_count} processes");
    }
    if active_group_count > 0 && analyzer_count == 0 {
        bail!("active Rust group has no rust-analyzer process in the sampled tree");
    }
    Ok(())
}

pub fn symbol_count(result: &Value) -> Option<usize> {
    result.as_array().map(Vec::len).or_else(|| {
        result
            .get("symbols")
            .and_then(Value::as_array)
            .map(Vec::len)
    })
}

pub fn wait_until_ready<F>(mut status: F, timeout: Duration) -> Result<Value>
where
    F: FnMut() -> Result<Value>,
{
    let deadline = Instant::now() + timeout;
    loop {
        let state = status()?;
        match state.get("status").and_then(Value::as_str) {
            Some("Ready" | "Degraded") => return Ok(state),
            Some("Failed" | "Stopped") => bail!("project entered terminal state: {state}"),
            _ if Instant::now() >= deadline => {
                bail!("project did not become ready before activation timeout")
            }
            _ => thread::sleep(Duration::from_millis(250)),
        }
    }
}

pub fn decode_sse(body: &str) -> Option<Value> {
    let mut last = None;
    let mut event = String::new();
    for line in body.lines().chain(std::iter::once("")) {
        if let Some(data) = sse_data_field(line) {
            if !event.is_empty() {
                event.push('\n');
            }
            event.push_str(data);
        } else if line.is_empty() && !event.is_empty() {
            last = serde_json::from_str(&event).ok().or(last);
            event.clear();
        }
    }
    last.or_else(|| serde_json::from_str(body.trim()).ok())
}

/// Return an SSE `data` field after the one optional separator space.
///
/// The SSE grammar discards at most one U+0020 after the colon; other leading
/// whitespace belongs to the event payload.
fn sse_data_field(line: &str) -> Option<&str> {
    line.strip_prefix("data:")
        .map(|data| data.strip_prefix(' ').unwrap_or(data))
}

pub struct McpClient {
    host: String,
    port: u16,
    path: String,
    session_id: Option<String>,
    request_id: u64,
}

impl McpClient {
    pub fn new(url: &str) -> Result<Self> {
        let parsed = url::Url::parse(url).with_context(|| format!("parsing MCP URL {url}"))?;
        if parsed.scheme() != "http" {
            bail!("MCP URL must use http");
        }
        Ok(Self {
            host: parsed.host_str().context("MCP URL has no host")?.to_owned(),
            port: parsed.port().unwrap_or(80),
            path: if parsed.path().is_empty() {
                "/".to_owned()
            } else {
                parsed.path().to_owned()
            },
            session_id: None,
            request_id: 0,
        })
    }

    pub fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "mcpls-rust-benchmark", "version": "1"}
            })),
            false,
        )?;
        self.request("notifications/initialized", None, true)?;
        Ok(())
    }

    pub fn tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let result = self.request(
            "tools/call",
            Some(serde_json::json!({"name": name, "arguments": arguments})),
            false,
        )?;
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bail!("{name} returned an MCP error: {result}");
        }
        let text = result["content"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["type"] == "text"))
            .and_then(|item| item["text"].as_str())
            .map(str::trim);
        if let Some(text) = text
            && let Ok(value) = serde_json::from_str(text)
        {
            return Ok(value);
        }
        result
            .get("structuredContent")
            .cloned()
            .filter(|value| !value.is_null())
            .with_context(|| format!("parsing {name} result"))
    }

    fn request(
        &mut self,
        method: &str,
        params: Option<Value>,
        notification: bool,
    ) -> Result<Value> {
        let mut payload = serde_json::json!({"jsonrpc": "2.0", "method": method});
        if !notification {
            self.request_id += 1;
            payload["id"] = self.request_id.into();
        }
        if let Some(params) = params {
            payload["params"] = params;
        }
        let body = serde_json::to_vec(&payload)?;
        let address = (self.host.as_str(), self.port)
            .to_socket_addrs()?
            .next()
            .context("MCP host has no addresses")?;
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(30))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let session = self
            .session_id
            .as_ref()
            .map_or(String::new(), |id| format!("Mcp-Session-Id: {id}\r\n"));
        write!(
            stream,
            "POST {} HTTP/1.1\r\nHost: {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
            self.path,
            self.host,
            body.len(),
            session
        )?;
        stream.write_all(&body)?;
        stream.flush()?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response)?;
        let response = String::from_utf8(response)?;
        let (headers, body) = response
            .split_once("\r\n\r\n")
            .context("invalid HTTP response")?;
        let status_line = headers
            .lines()
            .next()
            .context("HTTP response has no status")?;
        let status_code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u16>().ok());
        let accepted = status_code.is_some_and(|code| {
            if notification {
                (200..300).contains(&code)
            } else {
                code == 200
            }
        });
        if !accepted {
            bail!("{method} failed: {headers}");
        }
        for header in headers.lines().skip(1) {
            if header
                .split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id"))
            {
                let value = header.split_once(':').map_or("", |(_, value)| value);
                self.session_id = Some(value.trim().to_owned());
            }
        }
        if notification {
            return Ok(Value::Null);
        }
        let response = decode_sse(body).context("MCP response contained no JSON")?;
        if let Some(error) = response.get("error") {
            bail!("{method} failed: {error}");
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn decodes_last_sse_json_event() {
        let body = "data: stale\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7}\n";
        assert_eq!(decode_sse(body).and_then(|v| v["id"].as_u64()), Some(7));
    }

    #[test]
    fn decodes_multiline_sse_json_event() {
        let body = "data: stale\n\ndata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":7}\n";
        assert_eq!(decode_sse(body).and_then(|v| v["id"].as_u64()), Some(7));
    }

    #[test]
    fn sse_data_field_preserves_payload_whitespace_after_the_separator() {
        assert_eq!(sse_data_field("data: \t  payload"), Some("\t  payload"));
    }

    #[test]
    fn rejects_fallback_only_samples() {
        let error = match validate_process_snapshot(0, 1, 1) {
            Ok(()) => panic!("fallback-only sample was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("no rust-analyzer process"));
    }

    #[test]
    fn counts_analyzers() {
        let names = vec![
            "mcpls".to_owned(),
            "rust-analyzer".to_owned(),
            "rust-analyzer-p".to_owned(),
        ];
        assert_eq!(rust_analyzer_count(&names), 1);
    }

    #[test]
    fn mcp_client_handles_streamable_http_sse() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            for request in 0..3 {
                let (mut stream, _) = listener.accept().expect("accept test request");
                let mut request_bytes = Vec::new();
                loop {
                    let mut byte = [0; 1];
                    stream.read_exact(&mut byte).expect("read test request");
                    request_bytes.push(byte[0]);
                    if request_bytes.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&request_bytes);
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length:")?.trim().parse().ok())
                    .unwrap_or(0);
                let mut request_body = vec![0; content_length];
                stream
                    .read_exact(&mut request_body)
                    .expect("read test body");
                let result = if request == 2 {
                    r#"{"content":[{"type":"text","text":"Structured result available in structuredContent."}],"structuredContent":{"ok":true}}"#
                } else {
                    "{}"
                };
                let body = if request == 1 {
                    String::new()
                } else if request == 2 {
                    format!("data: {{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\ndata: {result}}}\n")
                } else {
                    format!("data: {{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{result}}}\n")
                };
                let status = if request == 1 {
                    "202 Accepted"
                } else {
                    "200 OK"
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nMcp-Session-Id: test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write test response");
            }
        });
        let mut client = McpClient::new(&format!("http://{address}/mcp")).expect("client");
        client.initialize().expect("initialize");
        assert_eq!(client.tool("test", json!({})).expect("tool")["ok"], true);
        server.join().expect("server thread");
    }
}
