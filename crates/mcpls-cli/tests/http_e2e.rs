#![cfg(feature = "transport-http")]

//! Wire-level Streamable HTTP coverage for the long-lived daemon.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

struct HttpClient {
    address: SocketAddr,
    session_id: Option<String>,
    next_id: u64,
}

impl HttpClient {
    fn new(address: SocketAddr) -> Self {
        Self {
            address,
            session_id: None,
            next_id: 1,
        }
    }

    fn initialize(&mut self) {
        let request_id = self.next_request_id();
        let response = self.request(json!({
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

        let response = self.request(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }));
        assert!(response["_status"].as_u64() == Some(202));
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let request_id = self.next_request_id();
        let response = self.request(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }));
        assert!(response.get("error").is_none(), "tool error: {response}");
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool response has no text: {response}"));
        serde_json::from_str(text).unwrap_or_else(|error| {
            panic!("tool response is not JSON: {error}; response={response}")
        })
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn request(&self, payload: Value) -> Value {
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
            if child.try_wait().unwrap().is_some() {
                panic!("mcpls exited before binding HTTP listener");
            }
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

#[cfg(unix)]
fn write_mock_lsp(directory: &TempDir) -> (PathBuf, PathBuf) {
    let command = directory.path().join("mock-lsp.py");
    let counter = directory.path().join("lsp-spawns");
    std::fs::write(
        &command,
        r##"#!/usr/bin/env python3
import json
import os
import pathlib
import sys

counter = pathlib.Path(os.environ["MCPLS_SPAWN_COUNTER"])
value = int(counter.read_text()) if counter.exists() else 0
counter.write_text(str(value + 1))

def read_message():
    headers = b""
    while b"\r\n\r\n" not in headers:
        chunk = sys.stdin.buffer.read(1)
        if not chunk:
            return None
        headers += chunk
    length = next(
        int(line.split(b":", 1)[1].strip())
        for line in headers.split(b"\r\n")
        if line.lower().startswith(b"content-length:")
    )
    return json.loads(sys.stdin.buffer.read(length))

def send(message):
    body = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    if message.get("method") == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {"positionEncoding": "utf-8"}}})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"health": "ok", "quiescent": True}})
    elif message.get("method") == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        break
"##,
    )
    .unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
    (command, counter)
}

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
    _directory: TempDir,
    config: PathBuf,
    spawn_counter: PathBuf,
}

impl HttpFixture {
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let state_file = directory.path().join("projects.json");
        let (lsp_command, spawn_counter) = write_mock_lsp(&directory);
        let config = write_config(&directory, &state_file, &lsp_command, &spawn_counter);
        Self {
            _directory: directory,
            config,
            spawn_counter,
        }
    }

    fn project_root(&self, name: &str) -> PathBuf {
        let root = self._directory.path().join(name);
        std::fs::create_dir(&root).unwrap();
        root
    }
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

#[cfg(unix)]
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
    assert_eq!(restarted["status"], "Ready");
    assert_eq!(
        std::fs::read_to_string(&fixture.spawn_counter).unwrap(),
        "2"
    );

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
