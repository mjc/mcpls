#![cfg(feature = "transport-http")]
#![allow(missing_docs)]
#![allow(rustdoc::missing_crate_level_docs)]
#![doc = "Wire-level Streamable HTTP coverage for the long-lived daemon."]
#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{Value, json};
use tempfile::TempDir;

struct HttpClient {
    address: SocketAddr,
    session_id: Option<String>,
    next_id: u64,
}

impl HttpClient {
    const fn new(address: SocketAddr) -> Self {
        Self {
            address,
            session_id: None,
            next_id: 1,
        }
    }

    fn initialize(&mut self) {
        let request_id = self.next_request_id();
        let response = self.request(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "mcpls-http-e2e", "version": "0.1.0"}
            }
        }));
        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
        self.session_id = response["_session_id"].as_str().map(str::to_owned);
        assert!(
            self.session_id.is_some(),
            "initialize must return a session ID"
        );

        let response = self.request(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }));
        assert_eq!(response["_status"].as_u64(), Some(202));
    }

    #[allow(clippy::needless_pass_by_value)]
    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.call_tool_response(name, arguments);
        assert!(response.get("error").is_none(), "tool error: {response}");
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool response has no text: {response}"));
        serde_json::from_str(text).unwrap_or_else(|error| {
            panic!("tool response is not JSON: {error}; response={response}")
        })
    }

    #[allow(clippy::needless_pass_by_value)]
    fn call_tool_response(&mut self, name: &str, arguments: Value) -> Value {
        let request_id = self.next_request_id();
        self.request(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }))
    }

    fn request_method(&mut self, method: &str, params: &Value) -> Value {
        let request_id = self.next_request_id();
        let response = self.request(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params
        }));
        assert!(response.get("error").is_none(), "request error: {response}");
        response["result"].clone()
    }

    fn subscribe(&mut self, uri: &str) {
        let request_id = self.next_request_id();
        let response = self.request(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "resources/subscribe",
            "params": {"uri": uri}
        }));
        assert_eq!(
            response["_status"].as_u64(),
            Some(200),
            "subscribe failed: {response}"
        );
        assert!(
            response.get("error").is_none(),
            "subscribe failed: {response}"
        );
    }

    fn open_events(&self, last_event_id: Option<&str>) -> HttpEventStream {
        HttpEventStream::open(
            self.address,
            self.session_id.as_deref().unwrap(),
            last_event_id,
        )
    }

    const fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn request(&self, payload: &Value) -> Value {
        let body = serde_json::to_vec(&payload).unwrap();
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let session_header = self.session_id.as_deref().map_or(String::new(), |session| {
            format!("MCP-Session-Id: {session}\r\n")
        });
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n{}\r\n",
            self.address,
            body.len(),
            session_header,
        );
        stream.write_all(request.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        parse_response(&bytes)
    }
}

struct HttpEventStream {
    stream: TcpStream,
}

impl HttpEventStream {
    fn open(address: SocketAddr, session_id: &str, last_event_id: Option<&str>) -> Self {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let last_event_header =
            last_event_id.map_or(String::new(), |id| format!("Last-Event-ID: {id}\r\n"));
        let request = format!(
            "GET /mcp HTTP/1.1\r\nHost: {address}\r\nAccept: text/event-stream\r\nMCP-Session-Id: {session_id}\r\n{last_event_header}Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();

        let mut headers = Vec::new();
        let mut byte = [0; 1];
        while !headers.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
        }
        let status = headers
            .split(|byte| *byte == b'\n')
            .next()
            .and_then(|line| line.split(|byte| *byte == b' ').nth(1))
            .and_then(|code| std::str::from_utf8(code).ok())
            .and_then(|code| code.trim().parse::<u16>().ok())
            .unwrap();
        assert_eq!(
            status,
            200,
            "SSE GET failed: {}",
            String::from_utf8_lossy(&headers)
        );
        Self { stream }
    }

    fn wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        self.stream
            .set_read_timeout(Some(timeout.min(Duration::from_secs(10))))
            .unwrap();
        let needle = needle.as_bytes();
        let mut bytes = Vec::new();
        while Instant::now() < deadline {
            let mut chunk = [0; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return false,
                Ok(length) => {
                    bytes.extend_from_slice(&chunk[..length]);
                    if bytes.windows(needle.len()).any(|window| window == needle) {
                        return true;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return false;
                }
                Err(error) => panic!("reading SSE stream failed: {error}"),
            }
        }
        false
    }
}

struct HttpDaemon {
    child: Child,
    address: SocketAddr,
}

impl HttpDaemon {
    fn spawn(config: &Path) -> Self {
        let address = free_address();
        let mut child = Command::cargo_bin("mcpls")
            .unwrap()
            .args([
                "--config",
                config.to_str().unwrap(),
                "--listen",
                &address.to_string(),
            ])
            .env("MCPLS_LOG", "error")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if TcpStream::connect(address).is_ok() {
                return Self { child, address };
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "mcpls exited before binding HTTP listener"
            );
            thread::sleep(Duration::from_millis(50));
        }
        panic!("mcpls did not bind HTTP listener");
    }

    fn terminate(&mut self) {
        let result = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(result.success());
        for _ in 0..100 {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("mcpls did not shut down after SIGTERM");
    }
}

impl Drop for HttpDaemon {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let _ = self
                .child
                .kill()
                .and_then(|()| self.child.wait().map(|_| ()));
        }
    }
}

fn free_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn parse_response(bytes: &[u8]) -> Value {
    let separator = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let (headers, body) = bytes.split_at(separator + 4);
    let status = headers
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| line.split(|byte| *byte == b' ').nth(1))
        .and_then(|code| std::str::from_utf8(code).ok())
        .and_then(|code| code.trim().parse::<u16>().ok())
        .unwrap();
    let header_text = std::str::from_utf8(headers).unwrap();
    let session_id = header_text.lines().find_map(|line| {
        line.strip_prefix("mcp-session-id:")
            .or_else(|| line.strip_prefix("MCP-Session-Id:"))
            .map(str::trim)
    });
    let decoded = if header_text
        .lines()
        .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked\r"))
    {
        decode_chunked(body)
    } else {
        body.to_vec()
    };
    let mut response = decoded
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"data: "))
        .filter(|line| !line.is_empty())
        .find_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            serde_json::from_slice::<Value>(line).ok()
        })
        .unwrap_or_else(|| json!({}));
    response["_status"] = json!(status);
    if let Some(session_id) = session_id {
        response["_session_id"] = json!(session_id);
    }
    response
}

fn decode_chunked(body: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    let mut cursor = 0;
    while cursor < body.len() {
        let line_end = body[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .unwrap();
        let size = usize::from_str_radix(
            std::str::from_utf8(&body[cursor..cursor + line_end])
                .unwrap()
                .trim(),
            16,
        )
        .unwrap();
        cursor += line_end + 2;
        if size == 0 {
            break;
        }
        decoded.extend_from_slice(&body[cursor..cursor + size]);
        cursor += size + 2;
    }
    decoded
}

fn write_mock_lsp(directory: &TempDir) -> (PathBuf, PathBuf) {
    let source = directory.path().join("mock-lsp.rs");
    let command = directory.path().join("mock-lsp");
    let counter = directory.path().join("lsp-spawns");
    std::fs::write(&source, MOCK_LSP_SOURCE).unwrap();
    let status = Command::new("rustc")
        .args([
            "--edition=2021",
            source.to_str().unwrap(),
            "-o",
            command.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "failed to compile mock LSP");
    (command, counter)
}

const MOCK_LSP_SOURCE: &str = r#"
use std::env;
use std::fs;
use std::io::{self, Read, Write};

fn read_message() -> Option<String> {
    let mut headers = Vec::new();
    let mut byte = [0; 1];
    while !headers.ends_with(b"\r\n\r\n") {
        if io::stdin().read_exact(&mut byte).is_err() { return None; }
        headers.push(byte[0]);
    }
    let headers = String::from_utf8(headers).ok()?;
    let length = headers.lines().find_map(|line| {
        line.strip_prefix("Content-Length:")?.trim().parse::<usize>().ok()
    })?;
    let mut body = vec![0; length];
    io::stdin().read_exact(&mut body).ok()?;
    String::from_utf8(body).ok()
}

fn request_id(body: &str) -> Option<&str> {
    let start = body.find("\"id\"")?;
    let value = body[start..].split_once(':')?.1.trim_start();
    let end = value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len());
    Some(&value[..end])
}

fn send(body: &str) {
    print!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    io::stdout().flush().unwrap();
}

fn main() {
    let counter = env::var("MCPLS_SPAWN_COUNTER").unwrap();
    let value = fs::read_to_string(&counter).ok().and_then(|value| value.parse::<u32>().ok()).unwrap_or(0);
    fs::write(counter, (value + 1).to_string()).unwrap();
    if env::var("MCPLS_BLOCK_READY").ok().as_deref() == Some("1") {
        while read_message().is_some() {}
        return;
    }
    while let Some(message) = read_message() {
        if message.contains("\"method\":\"initialize\"") {
            let id = request_id(&message).unwrap();
            send(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"capabilities\":{{\"positionEncoding\":\"utf-8\"}}}}}}", id));
            send("{\"jsonrpc\":\"2.0\",\"method\":\"experimental/serverStatus\",\"params\":{\"health\":\"ok\",\"quiescent\":true}}");
        } else if message.contains("\"method\":\"shutdown\"") {
            let id = request_id(&message).unwrap();
            send(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":null}}", id));
        } else if message.contains("\"method\":\"exit\"") {
            break;
        }
    }
}
"#;

#[cfg(unix)]
fn write_config(
    directory: &TempDir,
    state_file: &Path,
    lsp_command: &Path,
    spawn_counter: &Path,
) -> PathBuf {
    let path = directory.path().join("mcpls.toml");
    std::fs::write(
        &path,
        format!(
            "[workspace]\nroots = [\"/definitely/missing/mcpls-http-e2e\"]\n\n[[lsp_servers]]\nlanguage_id = \"rust\"\ncommand = \"{}\"\nargs = []\nfile_patterns = [\"**/*.rs\"]\ntimeout_seconds = 5\nenv = {{ MCPLS_SPAWN_COUNTER = \"{}\" }}\n\n[daemon]\nstate_file = \"{}\"\n",
            lsp_command.display(),
            spawn_counter.display(),
            state_file.display(),
        ),
    )
    .unwrap();
    path
}

struct HttpFixture {
    directory: TempDir,
    config: PathBuf,
    lsp_command: PathBuf,
    spawn_counter: PathBuf,
}

impl HttpFixture {
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let state_file = directory.path().join("projects.json");
        let (lsp_command, spawn_counter) = write_mock_lsp(&directory);
        let config = write_config(&directory, &state_file, &lsp_command, &spawn_counter);
        Self {
            directory,
            config,
            lsp_command,
            spawn_counter,
        }
    }

    fn project_root(&self, name: &str) -> PathBuf {
        let root = self.directory.path().join(name);
        std::fs::create_dir(&root).unwrap();
        root
    }
}

struct RealRaHttpFixture {
    _directory: TempDir,
    config: PathBuf,
    spawn_counter: PathBuf,
    root_a: PathBuf,
    root_b: PathBuf,
    functions_b: PathBuf,
    lib_b: PathBuf,
    broken_b: PathBuf,
    bad_format_b: PathBuf,
}

impl RealRaHttpFixture {
    fn new(rust_analyzer: &Path) -> Self {
        let directory = TempDir::new().unwrap();
        let spawn_counter = directory.path().join("rust-analyzer-spawns");
        std::fs::write(&spawn_counter, "0").unwrap();
        let wrapper = write_counting_rust_analyzer(&directory, rust_analyzer);
        let state_file = directory.path().join("projects.json");
        let config = write_real_ra_config(&directory, &state_file, &wrapper, &spawn_counter);
        let root_a = directory.path().join("project-a");
        let root_b = directory.path().join("project-b");
        write_rust_project(&root_a, "only_a", false);
        write_rust_project(&root_b, "only_b", true);

        Self {
            _directory: directory,
            config,
            spawn_counter,
            functions_b: root_b.join("src/functions.rs"),
            lib_b: root_b.join("src/lib.rs"),
            broken_b: root_b.join("src/broken.rs"),
            bad_format_b: root_b.join("src/bad_format.rs"),
            root_a,
            root_b,
        }
    }
}

fn write_counting_rust_analyzer(directory: &TempDir, rust_analyzer: &Path) -> PathBuf {
    let source = directory.path().join("counting-rust-analyzer.rs");
    let command = directory.path().join("counting-rust-analyzer");
    let analyzer = serde_json::to_string(&rust_analyzer.to_string_lossy()).unwrap();
    let source_text = format!(
        r#"
use std::env;
use std::fs;
use std::process::Command;

const RUST_ANALYZER: &str = {analyzer};

fn main() {{
    let counter = env::var("MCPLS_SPAWN_COUNTER").unwrap();
    let value = fs::read_to_string(&counter)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    fs::write(&counter, (value + 1).to_string()).unwrap();
    let status = Command::new(RUST_ANALYZER)
        .args(env::args().skip(1))
        .status()
        .unwrap();
    std::process::exit(status.code().unwrap_or(1));
}}
"#
    );
    std::fs::write(&source, source_text).unwrap();
    let status = Command::new("rustc")
        .args([
            "--edition=2021",
            source.to_str().unwrap(),
            "-o",
            command.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "failed to compile counting rust-analyzer");
    command
}

fn write_real_ra_config(
    directory: &TempDir,
    state_file: &Path,
    lsp_command: &Path,
    spawn_counter: &Path,
) -> PathBuf {
    let path = directory.path().join("mcpls-real-ra.toml");
    std::fs::write(
        &path,
        format!(
            "[workspace]\nroots = [\"/definitely/missing/mcpls-real-ra\"]\n\n[[lsp_servers]]\nlanguage_id = \"rust\"\ncommand = \"{}\"\nargs = []\nfile_patterns = [\"**/*.rs\"]\ntimeout_seconds = 15\nenv = {{ MCPLS_SPAWN_COUNTER = \"{}\" }}\n\n[daemon]\nstate_file = \"{}\"\n",
            lsp_command.display(),
            spawn_counter.display(),
            state_file.display(),
        ),
    )
    .unwrap();
    path
}

fn write_rust_project(root: &Path, marker: &str, with_actions: bool) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .replace('-', "_")
        ),
    )
    .unwrap();

    let functions = format!("pub fn {marker}() -> i32 {{ 7 }}\n");
    std::fs::write(root.join("src/functions.rs"), functions).unwrap();
    let mut lib = format!(
        "pub mod functions;\npub fn caller() -> i32 {{ let _emoji = \"🙂\"; crate::functions::{marker}() }}\n"
    );
    if with_actions {
        lib.push_str(
            "pub trait Greet { fn greet(&self) -> &'static str; }\npub struct CodeActionTarget;\nimpl Greet for CodeActionTarget {}\n",
        );
    }
    std::fs::write(root.join("src/lib.rs"), lib).unwrap();
    std::fs::write(
        root.join("src/broken.rs"),
        "pub fn broken() -> i32 { \"not an integer\" }\n",
    )
    .unwrap();
    let mut lib = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    lib.push_str("pub mod broken;\n");
    std::fs::write(root.join("src/lib.rs"), lib).unwrap();
    std::fs::write(
        root.join("src/bad_format.rs"),
        "pub fn badly_formatted()->i32{crate::functions::".to_owned() + marker + "()}\n",
    )
    .unwrap();
}

fn resolve_rust_analyzer_for_http() -> Option<PathBuf> {
    if std::env::var("MCPLS_SKIP_RA").ok().as_deref() == Some("1") {
        return None;
    }
    let candidate = std::env::var_os("MCPLS_RUST_ANALYZER")
        .map_or_else(|| PathBuf::from("rust-analyzer"), PathBuf::from);
    Command::new(&candidate)
        .arg("--version")
        .output()
        .ok()
        .map(|_| candidate)
}

fn find_line(path: &Path, needle: &str) -> u32 {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .position(|line| line.contains(needle))
        .map(|line| u32::try_from(line).unwrap() + 1)
        .unwrap()
}

fn wait_project_ready(client: &mut HttpClient, project_id: &str) {
    for _ in 0..120 {
        let status = client.call_tool("project_status", json!({"project_id": project_id}));
        match status["status"].as_str() {
            Some("Ready") => return,
            Some("Failed") => panic!("project {project_id} failed: {status}"),
            _ => thread::sleep(Duration::from_millis(250)),
        }
    }
    panic!("project {project_id} did not become ready")
}

fn project_ids(client: &mut HttpClient) -> Vec<String> {
    client
        .call_tool("project_list", json!({}))
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|project| project["project_id"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn streamable_http_sessions_share_state_and_restore_projects_after_restart() {
    let fixture = HttpFixture::new();
    let root_a = fixture.project_root("project-a");
    let root_b = fixture.project_root("project-b");

    let mut daemon = HttpDaemon::spawn(&fixture.config);
    let mut first = HttpClient::new(daemon.address);
    let mut second = HttpClient::new(daemon.address);
    first.initialize();
    second.initialize();

    let added_a = first.call_tool(
        "project_add",
        json!({"project_id": "project-a", "root": root_a}),
    );
    assert_eq!(added_a["project_id"], "project-a");
    let added_b = second.call_tool(
        "project_add",
        json!({"project_id": "project-b", "root": root_b}),
    );
    assert_eq!(added_b["project_id"], "project-b");

    for client in [&mut first, &mut second] {
        assert_eq!(project_ids(client), ["project-a", "project-b"]);
        for id in ["project-a", "project-b"] {
            let status = client.call_tool("project_status", json!({"project_id": id}));
            assert_eq!(status["project_id"], id);
        }
    }

    let activated = first.call_tool("project_activate", json!({"project_id": "project-a"}));
    assert_eq!(activated["status"], "Ready");
    let restarted = second.call_tool("project_restart_lsp", json!({"project_id": "project-a"}));
    assert_eq!(
        restarted["status"], "Ready",
        "restart response: {restarted}"
    );
    assert_eq!(
        std::fs::read_to_string(&fixture.spawn_counter).unwrap(),
        "2"
    );

    let events_uri = "mcpls-project-events:///project-a";
    first.subscribe(events_uri);
    let mut events = first.open_events(None);
    second.call_tool("project_remove", json!({"project_id": "project-b"}));
    assert!(
        !events.wait_for("project-b", Duration::from_millis(100)),
        "project-A subscription received an event for project B"
    );
    first.call_tool(
        "project_add",
        json!({"project_id": "project-b", "root": root_b}),
    );
    second.call_tool("project_restart_lsp", json!({"project_id": "project-a"}));
    assert!(
        events.wait_for("notifications/resources/updated", Duration::from_secs(10)),
        "subscribed session did not receive a project event"
    );
    drop(events);

    let mut resumed_events = first.open_events(Some("0"));
    assert!(
        resumed_events.wait_for("notifications/resources/updated", Duration::from_secs(10)),
        "reconnected session did not resume project events"
    );
    let polled = first.request_method(
        "resources/read",
        &json!({"uri": "mcpls-project-events:///project-a?since=0"}),
    );
    let polled_text = polled["contents"][0]["text"].as_str().unwrap();
    let polled_events: Value = serde_json::from_str(polled_text).unwrap();
    assert_eq!(polled_events["resync_required"], false);
    assert!(!polled_events["events"].as_array().unwrap().is_empty());

    let removed = second.call_tool("project_remove", json!({"project_id": "project-b"}));
    assert_eq!(removed["project_id"], "project-b");
    assert_eq!(removed["removed"], true);
    assert_eq!(project_ids(&mut first), ["project-a"]);
    let restored_b = first.call_tool(
        "project_add",
        json!({"project_id": "project-b", "root": root_b}),
    );
    assert_eq!(restored_b["project_id"], "project-b");
    daemon.terminate();

    let mut restarted_daemon = HttpDaemon::spawn(&fixture.config);
    let mut restored = HttpClient::new(restarted_daemon.address);
    restored.initialize();
    assert_eq!(project_ids(&mut restored), ["project-a", "project-b"]);
    restarted_daemon.terminate();
}

#[test]
fn http_project_blockage_is_isolated_and_mutations_are_serialized() {
    let fixture = HttpFixture::new();
    let root_a = fixture.project_root("blocked-project");
    let root_b = fixture.project_root("ready-project");
    let mut daemon = HttpDaemon::spawn(&fixture.config);
    let mut first = HttpClient::new(daemon.address);
    let mut second = HttpClient::new(daemon.address);
    first.initialize();
    second.initialize();

    first.call_tool(
        "project_add",
        json!({
            "project_id": "blocked",
            "root": root_a,
            "config": {
                "lsp_servers": [{
                    "language_id": "rust",
                    "command": fixture.lsp_command,
                    "args": [],
                    "env": {
                        "MCPLS_BLOCK_READY": "1",
                        "MCPLS_SPAWN_COUNTER": fixture.spawn_counter
                    },
                    "file_patterns": ["**/*.rs"],
                    "timeout_seconds": 1
                }]
            }
        }),
    );
    second.call_tool(
        "project_add",
        json!({"project_id": "ready", "root": root_b}),
    );

    let blocked = first.call_tool_response("project_activate", json!({"project_id": "blocked"}));
    let blocked_status = blocked["result"]["content"][0]["text"]
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|value| value["status"].as_str().map(str::to_owned));
    assert!(
        blocked.get("error").is_some() || blocked_status.as_deref() == Some("Failed"),
        "blocked project unexpectedly activated: {blocked}"
    );

    let ready = second.call_tool("project_activate", json!({"project_id": "ready"}));
    assert_eq!(ready["status"], "Ready");
    assert_eq!(
        std::fs::read_to_string(&fixture.spawn_counter).unwrap(),
        "2"
    );

    for client in [&mut first, &mut second] {
        assert_eq!(
            client.call_tool("project_status", json!({"project_id": "ready"}))["status"],
            "Ready"
        );
    }
    let restart_one = second.call_tool("project_restart_lsp", json!({"project_id": "ready"}));
    assert_eq!(
        restart_one["status"], "Ready",
        "restart response: {restart_one}"
    );
    let restart_two = first.call_tool("project_restart_lsp", json!({"project_id": "ready"}));
    assert_eq!(
        restart_two["status"], "Ready",
        "restart response: {restart_two}"
    );
    assert_eq!(
        std::fs::read_to_string(&fixture.spawn_counter).unwrap(),
        "4"
    );
    daemon.terminate();
}

#[test]
fn http_rejects_non_loopback_listener_without_binding() {
    let fixture = HttpFixture::new();
    let output = Command::cargo_bin("mcpls")
        .unwrap()
        .args([
            "--config",
            fixture.config.to_str().unwrap(),
            "--listen",
            "0.0.0.0:0",
        ])
        .env("MCPLS_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("loopback"),
        "unexpected validation error: {stderr}"
    );
}

#[test]
#[ignore = "Requires rust-analyzer in PATH; set MCPLS_RUST_ANALYZER=<path>"]
fn real_rust_analyzer_http_sessions_and_safe_refactor_e2e() {
    if std::env::var("MCPLS_SKIP_RA").ok().as_deref() == Some("1") {
        println!("real rust-analyzer HTTP suite skipped by MCPLS_SKIP_RA=1");
        return;
    }
    let Some(rust_analyzer) = resolve_rust_analyzer_for_http() else {
        panic!(
            "rust-analyzer is required for this suite; set MCPLS_RUST_ANALYZER or MCPLS_SKIP_RA=1"
        );
    };
    let mut harness = RealRaHttpHarness::new(&rust_analyzer);
    harness.register_projects();
    harness.assert_navigation();
    harness.apply_rename_and_format();
    harness.apply_code_action_and_restart();
    harness.shutdown();
}

struct RealRaHttpHarness {
    fixture: RealRaHttpFixture,
    daemon: HttpDaemon,
    first: HttpClient,
    second: HttpClient,
}

impl RealRaHttpHarness {
    fn new(rust_analyzer: &Path) -> Self {
        let fixture = RealRaHttpFixture::new(rust_analyzer);
        let daemon = HttpDaemon::spawn(&fixture.config);
        let mut first = HttpClient::new(daemon.address);
        let mut second = HttpClient::new(daemon.address);
        first.initialize();
        second.initialize();
        Self {
            fixture,
            daemon,
            first,
            second,
        }
    }

    fn register_projects(&mut self) {
        register_real_ra_projects(&mut self.first, &mut self.second, &self.fixture);
    }

    fn assert_navigation(&mut self) {
        let target_line = find_line(&self.fixture.functions_b, "pub fn only_b(");
        assert_real_ra_navigation(
            &mut self.first,
            &mut self.second,
            &self.fixture,
            target_line,
        );
    }

    fn apply_rename_and_format(&mut self) {
        let target_line = find_line(&self.fixture.functions_b, "pub fn only_b(");
        apply_real_ra_rename_and_format(&mut self.first, &self.fixture, target_line);
    }

    fn apply_code_action_and_restart(&mut self) {
        let target_line = find_line(&self.fixture.functions_b, "pub fn renamed_b(");
        apply_real_ra_code_action_and_restart(
            &mut self.first,
            &mut self.second,
            &self.fixture,
            target_line,
        );
    }

    fn shutdown(&mut self) {
        self.daemon.terminate();
        assert!(self.daemon.child.try_wait().unwrap().is_some());
    }
}

fn register_real_ra_projects(
    first: &mut HttpClient,
    second: &mut HttpClient,
    fixture: &RealRaHttpFixture,
) {
    first.call_tool(
        "project_add",
        json!({"project_id": "project-a", "root": fixture.root_a}),
    );
    second.call_tool(
        "project_add",
        json!({"project_id": "project-b", "root": fixture.root_b}),
    );
    first.call_tool("project_activate", json!({"project_id": "project-a"}));
    second.call_tool("project_activate", json!({"project_id": "project-b"}));
    wait_project_ready(first, "project-a");
    wait_project_ready(second, "project-b");
    assert_eq!(
        std::fs::read_to_string(&fixture.spawn_counter).unwrap(),
        "2"
    );
    assert_eq!(project_ids(first), ["project-a", "project-b"]);
    assert_eq!(project_ids(second), ["project-a", "project-b"]);
    let capabilities = second.call_tool(
        "project_lsp_capabilities",
        json!({"project_id": "project-b", "language_id": "rust"}),
    );
    assert!(
        capabilities.to_string().contains("position_encoding")
            || capabilities.to_string().contains("positionEncoding"),
        "negotiated position encoding missing: {capabilities}"
    );
}

fn assert_real_ra_navigation(
    first: &mut HttpClient,
    second: &mut HttpClient,
    fixture: &RealRaHttpFixture,
    target_line: u32,
) {
    let symbols_a = first.call_tool(
        "workspace_symbol_search",
        json!({"project_id": "project-a", "query": "only_b"}),
    );
    assert!(symbols_a["symbols"].as_array().unwrap().is_empty());
    let symbols_b = second.call_tool(
        "workspace_symbol_search",
        json!({"project_id": "project-b", "query": "only_b"}),
    );
    assert!(!symbols_b["symbols"].as_array().unwrap().is_empty());

    let hover = second.call_tool(
        "get_hover",
        json!({"file_path": fixture.functions_b, "line": target_line, "character": 8}),
    );
    assert!(hover.to_string().contains("only_b"));
    let caller_line = find_line(&fixture.lib_b, "crate::functions::only_b");
    let definition = second.call_tool(
        "get_definition",
        json!({"file_path": fixture.lib_b, "line": caller_line, "character": 60}),
    );
    assert!(definition.to_string().contains("functions.rs"));
    let references = second.call_tool(
        "get_references",
        json!({
            "file_path": fixture.functions_b,
            "line": target_line,
            "character": 8,
            "include_declaration": true
        }),
    );
    assert!(references["locations"].as_array().unwrap().len() >= 2);

    let diagnostics = second.call_tool("get_diagnostics", json!({"file_path": fixture.broken_b}));
    assert!(!diagnostics["diagnostics"].as_array().unwrap().is_empty());
}

fn apply_real_ra_rename_and_format(
    client: &mut HttpClient,
    fixture: &RealRaHttpFixture,
    target_line: u32,
) {
    client.subscribe("mcpls-project-events:///project-b");
    let mut events = client.open_events(None);
    let rename = client.call_tool(
        "rename_preview",
        json!({
            "project_id": "project-b",
            "file_path": fixture.functions_b,
            "line": target_line,
            "character": 8,
            "new_name": "renamed_b",
            "position_encoding": "utf-16"
        }),
    );
    assert_eq!(rename["safe_to_apply"], true);
    assert!(
        rename["unified_diff"]
            .as_str()
            .unwrap()
            .contains("renamed_b")
    );
    let preconditions = rename["preconditions"].as_array().unwrap();
    assert!(preconditions.len() >= 2);
    for precondition in preconditions {
        assert!(
            precondition["sha256"]
                .as_str()
                .is_some_and(|hash| !hash.is_empty())
        );
        assert!(precondition.get("version").is_some());
    }
    let rename_plan = rename["plan_id"].as_str().unwrap().to_owned();
    let applied = client.call_tool(
        "workspace_edit_apply",
        json!({"project_id": "project-b", "plan_id": rename_plan}),
    );
    assert!(applied["committed_files"].as_array().unwrap().len() >= 2);
    assert!(
        std::fs::read_to_string(&fixture.functions_b)
            .unwrap()
            .contains("renamed_b")
    );
    assert!(
        std::fs::read_to_string(&fixture.lib_b)
            .unwrap()
            .contains("renamed_b")
    );
    assert!(
        events.wait_for("notifications/resources/updated", Duration::from_secs(10)),
        "rename did not produce a project resource notification"
    );
    drop(events);
    let stale = client.call_tool_response(
        "workspace_edit_apply",
        json!({"project_id": "project-b", "plan_id": rename_plan}),
    );
    assert!(stale.get("error").is_some());

    let formatted = client.call_tool(
        "format_preview",
        json!({
            "project_id": "project-b",
            "file_path": fixture.bad_format_b,
            "tab_size": 4,
            "insert_spaces": true,
            "position_encoding": "utf-16"
        }),
    );
    let format_plan = formatted["plan_id"].as_str().unwrap().to_owned();
    client.call_tool(
        "workspace_edit_apply",
        json!({"project_id": "project-b", "plan_id": format_plan}),
    );
    assert!(
        std::fs::read_to_string(&fixture.bad_format_b)
            .unwrap()
            .contains("pub fn badly_formatted()")
    );
}

fn apply_real_ra_code_action_and_restart(
    first: &mut HttpClient,
    second: &mut HttpClient,
    fixture: &RealRaHttpFixture,
    target_line: u32,
) {
    let code_actions = second.call_tool(
        "code_action_list",
        json!({
            "project_id": "project-b",
            "file_path": fixture.lib_b,
            "start_line": find_line(&fixture.lib_b, "impl Greet for CodeActionTarget"),
            "start_character": 6,
            "end_line": find_line(&fixture.lib_b, "impl Greet for CodeActionTarget"),
            "end_character": 6
        }),
    );
    let action = code_actions["actions"]
        .as_array()
        .and_then(|actions| actions.first())
        .unwrap_or_else(|| panic!("expected a real rust-analyzer code action: {code_actions}"));
    let action_id = action["action_id"]
        .as_str()
        .unwrap_or_else(|| panic!("code action has no action_id: {action}"));
    let action_plan = second.call_tool(
        "code_action_preview",
        json!({"project_id": "project-b", "action_id": action_id, "position_encoding": "utf-16"}),
    );
    let action_plan_id = action_plan["plan_id"].as_str().unwrap();
    second.call_tool(
        "code_action_apply",
        json!({"project_id": "project-b", "plan_id": action_plan_id}),
    );
    assert!(
        std::fs::read_to_string(&fixture.lib_b)
            .unwrap()
            .contains("fn greet(&self)")
    );

    second.call_tool("project_restart_lsp", json!({"project_id": "project-b"}));
    wait_project_ready(first, "project-b");
    let post_restart = first.call_tool(
        "get_hover",
        json!({
            "file_path": fixture.functions_b,
            "line": target_line,
            "character": 8
        }),
    );
    assert!(post_restart.to_string().contains("renamed_b"));
    assert_eq!(
        std::fs::read_to_string(&fixture.spawn_counter).unwrap(),
        "3"
    );
}
