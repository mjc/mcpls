#![allow(missing_docs)]

use anyhow::{Result, bail};
use clap::Parser;
use mcpls_bench::{
    McpClient, ProjectGroup, load_manifest, resource_snapshot, symbol_count,
    validate_process_snapshot, wait_until_ready,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(about = "Measure MCPLS resident Rust groups and time to the first symbol result")]
struct Args {
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    pid: u32,
    #[arg(long, default_value = "http://127.0.0.1:8445/mcp")]
    url: String,
    #[arg(long, default_value = "main")]
    query: String,
    #[arg(long, default_value_t = 180.0)]
    activation_timeout: f64,
    #[arg(long, default_value_t = 1)]
    max_active_groups: usize,
    #[arg(long, default_value = "mcpls43-bench")]
    project_prefix: String,
    #[arg(long)]
    output: Option<PathBuf>,
}

fn register_groups(
    client: &mut McpClient,
    groups: &[ProjectGroup],
    prefix: &str,
    registered_ids: &mut Vec<String>,
) -> Result<()> {
    for group in groups {
        let project_id = format!("{prefix}-{}", group.project_id);
        registered_ids.push(project_id.clone());
        for root in &group.roots {
            client.tool(
                "project_add",
                json!({"project_id": project_id, "root": root}),
            )?;
        }
    }
    Ok(())
}

fn remove_groups(client: &mut McpClient, project_ids: &[String]) {
    for project_id in project_ids.iter().rev() {
        let _ = client.tool("project_remove", json!({"project_id": project_id}));
    }
}

fn registered_states(client: &mut McpClient, project_ids: &[String]) -> Result<Vec<Value>> {
    project_ids
        .iter()
        .map(|project_id| {
            let state = client.tool("project_status", json!({"project_id": project_id}))?;
            if state["actor_group_count"] != 1 {
                bail!(
                    "{project_id} registered {} actor groups",
                    state["actor_group_count"]
                );
            }
            Ok(state)
        })
        .collect()
}

fn active_group_count(client: &mut McpClient, project_ids: &[String]) -> Result<usize> {
    Ok(project_ids
        .iter()
        .map(|project_id| client.tool("project_status", json!({"project_id": project_id})))
        .collect::<Result<Vec<_>>>()?
        .iter()
        .filter(|state| {
            state["active_language_servers"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        })
        .count())
}

fn first_result(client: &mut McpClient, project_id: &str, query: &str) -> Result<Value> {
    let started = Instant::now();
    let result = client.tool(
        "workspace_symbol_search",
        json!({"project_id": project_id, "query": query, "limit": 100}),
    )?;
    Ok(json!({
        "time_to_first_result_ms": (started.elapsed().as_secs_f64() * 1000.0 * 10.0).round() / 10.0,
        "result_count": symbol_count(&result),
    }))
}

fn run(args: &Args) -> Result<Value> {
    let groups = load_manifest(&args.manifest)?;
    let mut client = McpClient::new(&args.url)?;
    let mut project_ids = Vec::new();
    let result = (|| {
        client.initialize()?;
        register_groups(&mut client, &groups, &args.project_prefix, &mut project_ids)?;
        let registered = registered_states(&mut client, &project_ids)?;
        let daemon_status = client.tool("server_status", json!({}))?;
        let mut report = json!({
            "manifest": args.manifest,
            "project_count": groups.len(),
            "roots_per_project": 5,
            "daemon_only": {
                "server_status": daemon_status,
                "registered_projects": registered,
                "snapshot": resource_snapshot(args.pid),
            },
            "switches": [],
        });
        let mut sequence = (0..project_ids.len()).collect::<Vec<_>>();
        sequence.extend((0..project_ids.len()).rev());
        for index in sequence {
            let project_id = &project_ids[index];
            let started = Instant::now();
            client.tool("project_activate", json!({"project_id": project_id}))?;
            let activation_state = wait_until_ready(
                || client.tool("project_status", json!({"project_id": project_id})),
                Duration::from_secs_f64(args.activation_timeout),
            )?;
            let first = first_result(&mut client, project_id, &args.query)?;
            let state = client.tool("project_status", json!({"project_id": project_id}))?;
            let active_count = active_group_count(&mut client, &project_ids)?;
            let snapshot = resource_snapshot(args.pid);
            validate_process_snapshot(
                usize::try_from(
                    snapshot["processes"]["rust_analyzer_count"]
                        .as_u64()
                        .unwrap_or(0),
                )
                .unwrap_or_default(),
                active_count,
                args.max_active_groups,
            )?;
            let switch = json!({
                "project_id": project_id,
                "group_index": index,
                "activation_to_result_ms": (started.elapsed().as_secs_f64() * 1000.0 * 10.0).round() / 10.0,
                "activation_status": activation_state["status"],
                "status": state["status"],
                "active_language_servers": state["active_language_servers"],
                "active_group_count": active_count,
                "processes": snapshot["processes"],
                "pss_kib": snapshot["pss_kib"],
                "time_to_first_result_ms": first["time_to_first_result_ms"],
                "result_count": first["result_count"],
            });
            let Some(switches) = report["switches"].as_array_mut() else {
                bail!("report switches field is not an array");
            };
            switches.push(switch);
        }
        Ok(report)
    })();
    remove_groups(&mut client, &project_ids);
    result
}

fn main() -> Result<()> {
    let args = Args::parse();
    let report = run(&args)?;
    let rendered = serde_json::to_string_pretty(&report)?;
    println!("{rendered}");
    if let Some(output) = args.output {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(output, format!("{rendered}\n"))?;
    }
    Ok(())
}
