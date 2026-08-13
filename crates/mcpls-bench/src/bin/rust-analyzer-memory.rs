#![allow(
    missing_docs,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use mcpls_bench::{descendants, pss_kib};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use url::Url;

#[derive(Clone, Debug, ValueEnum)]
enum Profile {
    Default,
    Mcpls,
    McplsNoPriming,
    Lean,
}

impl Profile {
    const fn name(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Mcpls => "mcpls",
            Self::McplsNoPriming => "mcpls-no-priming",
            Self::Lean => "lean",
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Measure rust-analyzer readiness and retained PSS around workspace/symbol")]
struct Args {
    #[arg(long, env = "MCPLS_RUST_ANALYZER")]
    rust_analyzer: Option<PathBuf>,
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    #[arg(long, value_enum, default_value_t = Profile::Mcpls)]
    profile: Profile,
    #[arg(long, default_value = "workspace_symbol_search")]
    query: String,
    #[arg(long, default_value_t = 45.0)]
    settle_timeout: f64,
    #[arg(long, default_value_t = 60.0)]
    request_timeout: f64,
    #[arg(long)]
    max_before_mib: Option<f64>,
    #[arg(long)]
    max_query_delta_mib: Option<f64>,
    #[arg(long)]
    max_query_ms: Option<f64>,
    #[arg(long)]
    output: Option<PathBuf>,
}

struct LspClient {
    stdin: std::process::ChildStdin,
    messages: Receiver<Value>,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn send(stdin: &mut impl Write, message: &Value) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    write!(stdin, "Content-Length: {}\r\n\r\n", payload.len())?;
    stdin.write_all(&payload)?;
    stdin.flush()?;
    Ok(())
}

fn read_messages(stdout: impl Read + Send + 'static, sender: mpsc::Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut content_length = None;
        loop {
            let mut line = String::new();
            let Ok(bytes) = reader.read_line(&mut line) else {
                return;
            };
            if bytes == 0 {
                return;
            }
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = content_length else { return };
        let mut payload = vec![0; length];
        if reader.read_exact(&mut payload).is_err() {
            return;
        }
        let Ok(message) = serde_json::from_slice(&payload) else {
            return;
        };
        if sender.send(message).is_err() {
            return;
        }
    }
}

fn respond_to_server_request(client: &mut LspClient, message: &Value) -> Result<()> {
    let result = match message["method"].as_str() {
        Some("workspace/configuration") => {
            json!([])
        }
        Some("workspace/applyEdit") => json!({"applied": false}),
        _ => Value::Null,
    };
    send(
        &mut client.stdin,
        &json!({"jsonrpc": "2.0", "id": message["id"], "result": result}),
    )
}

fn handle_message(
    client: &mut LspClient,
    message: &Value,
    initial_load_complete: &mut bool,
    quiescent: &mut bool,
) -> Result<()> {
    if message.get("id").is_some() && message.get("method").is_some() {
        respond_to_server_request(client, message)?;
    }
    if message["method"] == "experimental/serverStatus" {
        *quiescent = message["params"]["quiescent"].as_bool().unwrap_or(false);
        *initial_load_complete |= *quiescent;
    }
    if message["method"] == "$/progress"
        && message["params"]["token"] == "rustAnalyzer/Indexing"
        && message["params"]["value"]["kind"] == "end"
    {
        *initial_load_complete = true;
    }
    Ok(())
}

fn wait_for_response(
    client: &mut LspClient,
    request_id: u64,
    timeout: Duration,
    initial_load_complete: &mut bool,
    quiescent: &mut bool,
) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for response {request_id}");
        }
        let message = match client
            .messages
            .recv_timeout(remaining.min(Duration::from_millis(250)))
        {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => bail!("rust-analyzer output closed"),
        };
        handle_message(client, &message, initial_load_complete, quiescent)?;
        if message["id"].as_u64() == Some(request_id) && message.get("method").is_none() {
            return Ok(message);
        }
    }
}

fn wait_for_initial_load(
    client: &mut LspClient,
    timeout: Duration,
    complete: &mut bool,
    quiescent: &mut bool,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !*complete && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let message = match client
            .messages
            .recv_timeout(remaining.min(Duration::from_millis(250)))
        {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => bail!("rust-analyzer output closed"),
        };
        handle_message(client, &message, complete, quiescent)?;
    }
    Ok(())
}

fn initialization_options(profile: &Profile, roots: &[PathBuf]) -> Value {
    let mut options = json!({
        "files": {"watcher": "client", "exclude": [".git", ".direnv", ".serena", "target"]},
        "workspace": {"symbol": {"search": {"kind": "all_symbols", "scope": "workspace"}}},
    });
    if roots.len() > 1 {
        options["linkedProjects"] = roots
            .iter()
            .map(|root| root.join("Cargo.toml"))
            .map(|path| path.to_string_lossy().to_string())
            .collect();
    }
    if !matches!(profile, Profile::Default) {
        options["cargo"] = json!({"allTargets": false});
        options["checkOnSave"] = false.into();
        options["lru"] = json!({"capacity": 32});
        options["cachePriming"] = json!({"enable": !matches!(profile, Profile::McplsNoPriming)});
    }
    if matches!(profile, Profile::Lean) {
        options["cargo"]["buildScripts"] = json!({"enable": false});
        options["procMacro"] = json!({"enable": false});
    }
    options
}

fn find_rust_analyzer(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if explicit.is_some() {
        return explicit;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("rust-analyzer"))
        .find(|candidate| candidate.is_file())
}

fn file_uri(path: &PathBuf) -> Result<String> {
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|()| anyhow::anyhow!("cannot create file URI for {}", path.display()))
}

fn process_summary(names: Vec<String>) -> Value {
    let rust_analyzer_count = names
        .iter()
        .filter(|name| mcpls_bench::RUST_ANALYZER_NAMES.contains(&name.as_str()))
        .count();
    json!({
        "process_count": names.len(),
        "process_names": names,
        "rust_analyzer_count": rust_analyzer_count,
    })
}

fn run(args: &Args) -> Result<Value> {
    if !cfg!(target_os = "linux") {
        bail!("PSS measurement requires Linux /proc");
    }
    let analyzer =
        find_rust_analyzer(args.rust_analyzer.clone()).context("rust-analyzer not found")?;
    let roots = if args.roots.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        args.roots.clone()
    };
    let roots = roots
        .into_iter()
        .map(fs::canonicalize)
        .collect::<std::io::Result<Vec<_>>>()?;
    for root in &roots {
        if !root.join("Cargo.toml").is_file() {
            bail!("no Cargo.toml under {}", root.display());
        }
    }
    fs::create_dir_all("target/benchmarks")?;
    let stderr_path = PathBuf::from("target/benchmarks/rust-analyzer-stderr.log");
    let stderr = File::create(&stderr_path)?;
    let mut child = ChildGuard(
        Command::new(analyzer)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()?,
    );
    let stdin = child
        .0
        .stdin
        .take()
        .context("rust-analyzer stdin unavailable")?;
    let stdout = child
        .0
        .stdout
        .take()
        .context("rust-analyzer stdout unavailable")?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || read_messages(stdout, sender));
    let mut client = LspClient {
        stdin,
        messages: receiver,
    };
    let mut initial_load_complete = false;
    let mut quiescent = false;
    let started = Instant::now();
    let root_uri = file_uri(&roots[0])?;
    send(
        &mut client.stdin,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "processId": std::process::id(), "rootUri": root_uri,
                "workspaceFolders": roots.iter().map(|root| json!({"uri": file_uri(root).unwrap_or_default(), "name": root.file_name().unwrap_or_default().to_string_lossy()})).collect::<Vec<_>>(),
                "capabilities": {"workspace": {"workspaceFolders": true, "configuration": true, "didChangeWatchedFiles": {"dynamicRegistration": true}, "symbol": {"symbolKind": {"valueSet": (1..=26).collect::<Vec<_>>()}}}, "window": {"workDoneProgress": true}, "experimental": {"serverStatusNotification": true}},
                "initializationOptions": initialization_options(&args.profile, &roots),
            }
        }),
    )?;
    let initialized = wait_for_response(
        &mut client,
        1,
        Duration::from_secs_f64(args.request_timeout),
        &mut initial_load_complete,
        &mut quiescent,
    )?;
    if initialized.get("error").is_some() {
        bail!("initialize failed: {}", initialized["error"]);
    }
    let initialized_ms = started.elapsed().as_secs_f64() * 1000.0;
    send(
        &mut client.stdin,
        &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    )?;
    wait_for_initial_load(
        &mut client,
        Duration::from_secs_f64(args.settle_timeout),
        &mut initial_load_complete,
        &mut quiescent,
    )?;
    let pre_query_wait_ms = started.elapsed().as_secs_f64() * 1000.0;
    let before_ids = descendants(child.0.id());
    let before_kib = pss_kib(&before_ids);
    let query_started = Instant::now();
    send(
        &mut client.stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "workspace/symbol", "params": {"query": args.query}}),
    )?;
    let response = wait_for_response(
        &mut client,
        2,
        Duration::from_secs_f64(args.request_timeout),
        &mut initial_load_complete,
        &mut quiescent,
    )?;
    let query_ms = query_started.elapsed().as_secs_f64() * 1000.0;
    let after_ids = descendants(child.0.id());
    let after_kib = pss_kib(&after_ids);
    let processes = process_summary(mcpls_bench::process_names(&after_ids));
    let result_count = response["result"].as_array().map_or(0, Vec::len);
    let report = json!({
        "profile": args.profile.name(), "roots": roots, "query": args.query,
        "initialized_ms": (initialized_ms * 10.0).round() / 10.0, "pre_query_wait_ms": (pre_query_wait_ms * 10.0).round() / 10.0,
        "initial_load_complete": initial_load_complete, "quiescent": quiescent,
        "process_count": processes["process_count"],
        "process_names": processes["process_names"],
        "rust_analyzer_count": processes["rust_analyzer_count"],
        "pss_before_query_kib": before_kib, "pss_after_query_kib": after_kib, "pss_query_delta_kib": after_kib as i64 - before_kib as i64,
        "query_ms": (query_ms * 10.0).round() / 10.0, "result_count": result_count, "stderr_path": stderr_path,
    });
    send(
        &mut client.stdin,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": null}),
    )?;
    let _ = wait_for_response(
        &mut client,
        3,
        Duration::from_secs(10),
        &mut initial_load_complete,
        &mut quiescent,
    );
    let _ = send(
        &mut client.stdin,
        &json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
    );
    let _ = child.wait_timeout(Duration::from_secs(10));
    if args
        .max_before_mib
        .is_some_and(|limit| before_kib as f64 / 1024.0 > limit)
        || args
            .max_query_delta_mib
            .is_some_and(|limit| (after_kib as i64 - before_kib as i64) as f64 / 1024.0 > limit)
        || args.max_query_ms.is_some_and(|limit| query_ms > limit)
    {
        bail!("benchmark limits exceeded");
    }
    Ok(report)
}

trait ChildWaitTimeout {
    fn wait_timeout(&mut self, timeout: Duration) -> Result<()>;
}

impl ChildWaitTimeout for ChildGuard {
    fn wait_timeout(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.0.try_wait()?.is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.0.kill()?;
                self.0.wait()?;
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let report = run(&args)?;
    let rendered = serde_json::to_string_pretty(&report)?;
    println!("{rendered}");
    if let Some(output) = &args.output {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, format!("{rendered}\n"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::process_summary;

    #[test]
    fn process_summary_identifies_analyzer_and_helpers() {
        let summary = process_summary(vec!["rust-analyzer".to_owned(), "proc-macro".to_owned()]);

        assert_eq!(summary["process_count"], 2);
        assert_eq!(summary["rust_analyzer_count"], 1);
        assert_eq!(summary["process_names"][0], "rust-analyzer");
    }
}
