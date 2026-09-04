//! MCP client simulator for end-to-end testing.
//!
//! This module provides a synchronous MCP client that spawns the mcpls binary
//! and communicates via stdio using the JSON-RPC 2.0 protocol.
//!
//! The authoritative command for the `mcpls-core` rust-analyzer E2E suite is:
//!
//! ```text
//! cargo build -p mcpls && MCPLS_E2E_BINARY="$PWD/target/debug/mcpls" \
//!   cargo nextest run -p mcpls-core --test ra_e2e --run-ignored ignored-only ra_e2e_suite
//! ```
//!
//! The explicit binary path is required because this test belongs to
//! `mcpls-core`, so Cargo does not provide `CARGO_BIN_EXE_mcpls` for it. This
//! keeps the suite tied to the binary built from the current workspace rather
//! than an arbitrary pre-existing `target/debug/mcpls`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Simulates an MCP client (like Claude Code) for E2E testing.
///
/// This client spawns the mcpls binary as a child process and communicates
/// with it via stdio using JSON-RPC 2.0 protocol.
///
/// # Examples
///
/// ```no_run
/// use mcpls_core::tests::e2e::mcp_client::McpClient;
///
/// let mut client = McpClient::spawn()?;
/// let response = client.initialize()?;
/// assert!(response.get("result").is_some());
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct McpClient {
    process: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_id: i64,
    /// Server-pushed notifications (no matching request `id`) collected while
    /// waiting for a request/response round-trip. Drained via `take_notifications`.
    pending_notifications: Vec<Value>,
}

impl McpClient {
    /// Spawn mcpls process and connect via stdio.
    ///
    /// Uses an empty configuration file for testing the MCP protocol layer only.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The mcpls binary cannot be found or spawned
    /// - stdin or stdout cannot be captured
    pub fn spawn() -> Result<Self> {
        // Use empty config to avoid LSP server initialization timeouts
        let config_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/empty_config.toml");

        Self::spawn_with_args(&[
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid config path"))?,
        ])
    }

    /// Spawn mcpls process with custom arguments.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The mcpls binary cannot be found or spawned
    /// - stdin or stdout cannot be captured
    pub fn spawn_with_args(args: &[&str]) -> Result<Self> {
        let binary_path = configured_binary_path(
            std::env::var_os("CARGO_BIN_EXE_mcpls").as_deref(),
            std::env::var_os("MCPLS_E2E_BINARY").as_deref(),
        )?;

        let mut process = Command::new(binary_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn mcpls binary")?;

        let stdin = process
            .stdin
            .take()
            .context("failed to capture stdin of mcpls process")?;

        let stdout = process
            .stdout
            .take()
            .context("failed to capture stdout of mcpls process")?;

        Ok(Self {
            process,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: 0,
            pending_notifications: Vec::new(),
        })
    }

    /// Drain and return server-pushed notifications collected so far (e.g.
    /// `notifications/resources/updated`).
    ///
    /// Notifications have no JSON-RPC `id` and may arrive interleaved with
    /// request/response traffic on the same stdout stream; `send_request` queues
    /// them here instead of misinterpreting them as the response it is waiting for.
    #[allow(dead_code)]
    pub fn take_notifications(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending_notifications)
    }

    /// Send MCP initialize request.
    ///
    /// This establishes the MCP connection and negotiates protocol version.
    /// After receiving the initialize response, sends the initialized notification
    /// as required by the MCP protocol.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    pub fn initialize(&mut self) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "mcpls-e2e-test",
                    "version": "0.1.0"
                }
            }
        });

        let response = self.send_request(&request)?;

        // Send initialized notification as required by MCP protocol
        self.send_notification("notifications/initialized", &json!({}))?;

        Ok(response)
    }

    /// Discover the server without creating a session.
    #[allow(dead_code)]
    pub fn discover(&mut self) -> Result<Value> {
        let id = self.next_id();
        self.send_request(&inline_request(id, "server/discover", &json!({})))
    }

    /// Send a self-describing 2026 request without initializing a session.
    #[allow(dead_code)]
    pub fn stateless_request(&mut self, method: &str, params: &Value) -> Result<Value> {
        let id = self.next_id();
        self.send_request(&inline_request(id, method, params))
    }

    /// Exchange an arbitrary JSON-RPC request, preserving error responses.
    #[allow(dead_code)]
    pub fn raw_request(&mut self, request: &Value) -> Result<Value> {
        self.exchange(request)
    }

    /// List available MCP tools.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    #[allow(dead_code)]
    pub fn list_tools(&mut self) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "tools/list",
            "params": {}
        });

        self.send_request(&request)
    }

    /// Call a tool by name with parameters.
    ///
    /// # Parameters
    ///
    /// - `name`: The name of the tool to call
    /// - `arguments`: JSON object with tool-specific parameters
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    /// - The tool does not exist
    /// - The parameters are invalid
    pub fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments
            }
        });

        self.send_request(&request)
    }

    /// List MCP resources (`resources/list`).
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be sent or the server returns an error.
    #[allow(dead_code)]
    pub fn list_resources(&mut self) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "resources/list",
            "params": {}
        });
        self.send_request(&request)
    }

    /// Read an MCP resource by URI (`resources/read`).
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be sent or the server returns an error.
    #[allow(dead_code)]
    pub fn read_resource(&mut self, uri: &str) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "resources/read",
            "params": { "uri": uri }
        });
        self.send_request(&request)
    }

    /// Subscribe to a resource (`resources/subscribe`).
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be sent or the server returns an error.
    #[allow(dead_code)]
    pub fn subscribe_resource(&mut self, uri: &str) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "resources/subscribe",
            "params": { "uri": uri }
        });
        self.send_request(&request)
    }

    /// Unsubscribe from a resource (`resources/unsubscribe`).
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be sent or the server returns an error.
    #[allow(dead_code)]
    pub fn unsubscribe_resource(&mut self, uri: &str) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "resources/unsubscribe",
            "params": { "uri": uri }
        });
        self.send_request(&request)
    }

    /// Send a raw JSON-RPC request and return the response.
    ///
    /// The server may push notifications (e.g. `notifications/resources/updated`)
    /// on the same stdout stream before writing the response; those are queued into
    /// `pending_notifications` rather than being mistaken for the response.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be serialized or sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    fn send_request(&mut self, request: &Value) -> Result<Value> {
        let response = self.exchange(request)?;

        if let Some(error) = response.get("error") {
            anyhow::bail!("MCP error: {error:?}");
        }

        // rmcp 1.8.0+: deserialization failures return isError=true inside a successful
        // tools/call result instead of a JSON-RPC error (PR #894).
        if response
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let content = response["result"]["content"].to_string();
            anyhow::bail!("MCP tool error (isError=true): {content}");
        }

        Ok(response)
    }

    fn exchange(&mut self, request: &Value) -> Result<Value> {
        let request_str = serde_json::to_string(request)?;
        writeln!(self.stdin, "{request_str}")?;
        self.stdin.flush()?;

        let expected_id = request.get("id").cloned();

        let response = loop {
            let mut line = String::new();
            self.stdout
                .read_line(&mut line)
                .context("failed to read response from mcpls")?;

            let value: Value =
                serde_json::from_str(&line).context("failed to parse JSON-RPC message")?;

            if value.get("id") == expected_id.as_ref() {
                break value;
            }
            self.pending_notifications.push(value);
        };

        Ok(response)
    }

    /// Send a notification (request without expecting a response).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The notification cannot be serialized or sent
    fn send_notification(&mut self, method: &str, params: &Value) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let notification_str = serde_json::to_string(&notification)?;
        writeln!(self.stdin, "{notification_str}")?;
        self.stdin.flush()?;

        Ok(())
    }

    /// Get the next request ID and increment the counter.
    // False positive: clippy suggests const fn, but const fn cannot mutate self
    #[allow(clippy::missing_const_for_fn)]
    fn next_id(&mut self) -> i64 {
        self.request_id += 1;
        self.request_id
    }

    /// Return the OS process ID of the spawned mcpls process.
    #[allow(dead_code)]
    pub(crate) fn pid(&self) -> u32 {
        self.process.id()
    }

    /// Non-blocking check for whether the process has exited.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS query for the process status fails.
    #[allow(dead_code)]
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.process.try_wait()
    }
}

fn configured_binary_path(
    cargo_binary: Option<&std::ffi::OsStr>,
    explicit_binary: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    let configured = cargo_binary.or(explicit_binary).ok_or_else(|| {
        anyhow::anyhow!(
            "MCPLS E2E binary is not configured; run `cargo build -p mcpls` and set MCPLS_E2E_BINARY to the resulting binary"
        )
    })?;
    let path = Path::new(configured);
    if !path.is_file() {
        anyhow::bail!(
            "configured MCPLS E2E binary does not exist: {}",
            path.display()
        );
    }
    Ok(path.to_path_buf())
}

#[allow(dead_code)]
fn inline_request(id: i64, method: &str, params: &Value) -> Value {
    let mut params = params.as_object().cloned().unwrap_or_default();
    params.insert(
        "_meta".to_owned(),
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {}
        }),
    );
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    })
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_selection_requires_cargo_or_explicit_binary() {
        let error = configured_binary_path(None, None).unwrap_err();
        assert!(error.to_string().contains("MCPLS_E2E_BINARY"));
        assert!(!error.to_string().contains("target/debug/mcpls"));
    }

    #[test]
    fn binary_selection_prefers_cargo_binary_and_validates_it() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_binary = dir.path().join("cargo-mcpls");
        let explicit_binary = dir.path().join("explicit-mcpls");
        std::fs::write(&cargo_binary, b"cargo").unwrap();
        std::fs::write(&explicit_binary, b"explicit").unwrap();

        assert_eq!(
            configured_binary_path(
                Some(cargo_binary.as_os_str()),
                Some(explicit_binary.as_os_str())
            )
            .unwrap(),
            cargo_binary
        );
    }

    #[test]
    #[ignore = "Requires mcpls binary built"]
    fn test_mcp_client_spawn() {
        let client = McpClient::spawn();
        assert!(client.is_ok(), "Should successfully spawn mcpls binary");
    }

    #[test]
    #[ignore = "Requires mcpls binary built"]
    fn test_request_id_increment() -> Result<()> {
        let mut client = McpClient::spawn()?;
        assert_eq!(client.next_id(), 1);
        assert_eq!(client.next_id(), 2);
        assert_eq!(client.next_id(), 3);
        Ok(())
    }
}
