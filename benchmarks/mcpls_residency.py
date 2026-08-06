#!/usr/bin/env python3
"""Measure MCPLS resident Rust groups and time to the first symbol result."""

import argparse
import http.client
import json
import pathlib
import subprocess
import time
import urllib.parse
from dataclasses import dataclass


@dataclass(frozen=True)
class ProjectGroup:
    project_id: str
    roots: tuple[pathlib.Path, ...]


def load_manifest(value):
    """Load and validate the required four-project/five-root matrix."""
    if isinstance(value, (str, pathlib.Path)):
        value = json.loads(pathlib.Path(value).read_text())
    projects = value.get("projects") if isinstance(value, dict) else None
    if not isinstance(projects, list) or len(projects) != 4:
        raise ValueError("manifest must contain exactly four projects")

    groups = []
    seen_roots = set()
    seen_ids = set()
    for project in projects:
        project_id = project.get("project_id") if isinstance(project, dict) else None
        roots = project.get("roots") if isinstance(project, dict) else None
        if not isinstance(project_id, str) or not project_id:
            raise ValueError("each project must have a non-empty project_id")
        if project_id in seen_ids:
            raise ValueError(f"duplicate project_id: {project_id}")
        seen_ids.add(project_id)
        if not isinstance(roots, list) or len(roots) != 5:
            raise ValueError(f"project {project_id} must contain exactly five roots")

        checked_roots = []
        common_dir = None
        for root in roots:
            path = pathlib.Path(root).expanduser().resolve()
            if path in seen_roots:
                raise ValueError(f"root appears more than once: {path}")
            if not (path / "Cargo.toml").is_file():
                raise ValueError(f"root has no Cargo.toml: {path}")
            root_common_dir = git_common_dir(path)
            if common_dir is None:
                common_dir = root_common_dir
            elif root_common_dir != common_dir:
                raise ValueError(
                    f"project {project_id} roots must be linked Git worktrees"
                )
            if not any(
                (path / filename).is_file()
                for filename in ("rust-toolchain", "rust-toolchain.toml")
            ):
                raise ValueError(f"root has no explicit Rust toolchain: {path}")
            seen_roots.add(path)
            checked_roots.append(path)
        groups.append(ProjectGroup(project_id, tuple(checked_roots)))
    return tuple(groups)


def decode_sse(body):
    """Return the last JSON data event from a streamable-HTTP response."""
    candidates = [
        line[5:].strip()
        for line in body.splitlines()
        if line.startswith("data:") and line[5:].strip()
    ]
    if not candidates and body.strip():
        candidates = [body.strip()]
    for candidate in reversed(candidates):
        try:
            return json.loads(candidate)
        except json.JSONDecodeError:
            continue
    return None


def git_common_dir(root):
    """Return the shared Git directory for a worktree, or reject the root."""
    try:
        result = subprocess.run(
            [
                "git",
                "-C",
                str(root),
                "rev-parse",
                "--show-toplevel",
                "--git-common-dir",
                "--is-inside-work-tree",
            ],
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise ValueError(f"root is not a Git worktree: {root}") from error
    top_level, common_dir, inside_worktree = result.stdout.splitlines()
    if inside_worktree != "true":
        raise ValueError(f"root is not a Git worktree: {root}")
    common_path = pathlib.Path(common_dir)
    if not common_path.is_absolute():
        common_path = pathlib.Path(top_level) / common_path
    return common_path.resolve()


class McpClient:
    def __init__(self, url):
        parsed = urllib.parse.urlparse(url)
        if parsed.scheme != "http" or not parsed.hostname:
            raise ValueError("--url must be an http URL")
        self.connection = http.client.HTTPConnection(
            parsed.hostname, parsed.port or 80, timeout=30
        )
        self.path = parsed.path or "/mcp"
        self.session_id = None
        self.request_id = 0

    def close(self):
        self.connection.close()

    def request(self, method, params=None, notification=False):
        payload = {"jsonrpc": "2.0", "method": method}
        if not notification:
            self.request_id += 1
            payload["id"] = self.request_id
        if params is not None:
            payload["params"] = params
        headers = {
            "Accept": "application/json, text/event-stream",
            "Content-Type": "application/json",
        }
        if self.session_id:
            headers["Mcp-Session-Id"] = self.session_id
        self.connection.request(
            "POST", self.path, json.dumps(payload, separators=(",", ":")), headers
        )
        response = self.connection.getresponse()
        body = response.read().decode()
        self.session_id = response.getheader("Mcp-Session-Id") or self.session_id
        if notification:
            if response.status not in (200, 202):
                raise RuntimeError(f"{method} failed with HTTP {response.status}")
            return None
        if response.status != 200:
            raise RuntimeError(f"{method} failed with HTTP {response.status}: {body}")
        result = decode_sse(body)
        if not result:
            raise RuntimeError(f"{method} returned no JSON response")
        if "error" in result:
            raise RuntimeError(f"{method} failed: {result['error']}")
        return result["result"]

    def initialize(self):
        self.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "mcpls-residency-benchmark", "version": "1"},
            },
        )
        self.request("notifications/initialized", notification=True)

    def tool(self, name, arguments):
        result = self.request(
            "tools/call", {"name": name, "arguments": arguments}
        )
        if result.get("isError"):
            raise RuntimeError(f"{name} returned an MCP error: {result}")
        text = next(item["text"] for item in result["content"] if item["type"] == "text")
        return json.loads(text)


def descendants(pid):
    pending = [pid]
    result = []
    while pending:
        current = pending.pop()
        result.append(current)
        try:
            children = pathlib.Path(f"/proc/{current}/task/{current}/children").read_text()
            pending.extend(int(child) for child in children.split())
        except (FileNotFoundError, ProcessLookupError):
            pass
    return result


def pss_kib(pid, process_ids=None):
    total = 0
    for current in descendants(pid) if process_ids is None else process_ids:
        try:
            with open(f"/proc/{current}/smaps_rollup", encoding="utf-8") as file:
                total += next(
                    int(line.split()[1]) for line in file if line.startswith("Pss:")
                )
        except (FileNotFoundError, ProcessLookupError, StopIteration):
            pass
    return total


def process_summary(pid, process_ids=None):
    """Summarize the daemon's live descendant process tree."""
    names = []
    for current in descendants(pid) if process_ids is None else process_ids:
        try:
            name = pathlib.Path(f"/proc/{current}/comm").read_text().strip()
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
        if name:
            names.append(name)
    return {"process_count": len(names), "process_names": sorted(names)}


def resource_snapshot(pid):
    """Capture PSS and process names from one daemon process-tree snapshot."""
    process_ids = descendants(pid)
    return {
        "pss_kib": pss_kib(pid, process_ids),
        "processes": process_summary(pid, process_ids),
    }


def register_groups(client, groups, prefix, ids=None):
    ids = [] if ids is None else ids
    for index, group in enumerate(groups):
        project_id = f"{prefix}-{group.project_id}"
        ids.append(project_id)
        for root in group.roots:
            client.tool(
                "project_add", {"project_id": project_id, "root": str(root)}
            )
    return ids


def remove_groups(client, project_ids):
    for project_id in reversed(project_ids):
        try:
            client.tool("project_remove", {"project_id": project_id})
        except RuntimeError:
            pass


def wait_until_ready(client, project_id, timeout, poll_interval=0.25):
    """Wait until activation is authoritative before measuring a result."""
    deadline = time.monotonic() + timeout
    while True:
        state = client.tool("project_status", {"project_id": project_id})
        status = state.get("status")
        if status in ("Ready", "Degraded"):
            return state
        if status in ("Failed", "Stopped"):
            raise RuntimeError(f"{project_id} entered {status}: {state}")
        if time.monotonic() >= deadline:
            raise TimeoutError(f"{project_id} did not become ready before activation timeout")
        time.sleep(poll_interval)


def validate_registered_groups(client, project_ids):
    states = []
    for project_id in project_ids:
        state = client.tool("project_status", {"project_id": project_id})
        if state.get("actor_group_count") != 1:
            raise RuntimeError(
                f"{project_id} registered {state.get('actor_group_count')} actor groups"
            )
        states.append(state)
    return states


def active_group_ids(client, project_ids):
    active = []
    for project_id in project_ids:
        state = client.tool("project_status", {"project_id": project_id})
        if state.get("active_language_servers"):
            active.append(project_id)
    return active


def symbol_count(result):
    if isinstance(result, list):
        return len(result)
    symbols = result.get("symbols") if isinstance(result, dict) else None
    return len(symbols) if isinstance(symbols, list) else None


def first_result(client, project_id, query):
    started = time.monotonic()
    result = client.tool(
        "workspace_symbol_search",
        {"project_id": project_id, "query": query, "limit": 100},
    )
    return {
        "time_to_first_result_ms": round((time.monotonic() - started) * 1000, 1),
        "result_count": symbol_count(result),
    }


def run(args):
    groups = load_manifest(args.manifest)
    client = McpClient(args.url)
    project_ids = []
    try:
        client.initialize()
        register_groups(client, groups, args.project_prefix, project_ids)
        registered_states = validate_registered_groups(client, project_ids)
        daemon_status = client.tool("server_status", {})
        report = {
            "manifest": str(args.manifest),
            "project_count": len(groups),
            "roots_per_project": 5,
            "daemon_only": {
                **resource_snapshot(args.pid),
                "server_status": daemon_status,
                "registered_projects": registered_states,
            },
            "switches": [],
        }
        sequence = list(range(len(project_ids))) + list(reversed(range(len(project_ids))))
        for index in sequence:
            project_id = project_ids[index]
            started = time.monotonic()
            client.tool("project_activate", {"project_id": project_id})
            activation_state = wait_until_ready(
                client, project_id, args.activation_timeout
            )
            result = first_result(client, project_id, args.query)
            state = client.tool("project_status", {"project_id": project_id})
            active_projects = active_group_ids(client, project_ids)
            if len(active_projects) > args.max_active_groups:
                raise RuntimeError(
                    f"resident group limit exceeded: {len(active_projects)} active groups"
                )
            report["switches"].append(
                {
                    "project_id": project_id,
                    "group_index": index,
                    "activation_to_result_ms": round(
                        (time.monotonic() - started) * 1000, 1
                    ),
                    "activation_status": activation_state.get("status"),
                    **result,
                    **resource_snapshot(args.pid),
                    "status": state.get("status"),
                    "active_language_servers": state.get("active_language_servers", []),
                    "active_group_count": len(active_projects),
                }
            )
        return report
    finally:
        remove_groups(client, project_ids)
        client.close()


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=pathlib.Path, required=True)
    parser.add_argument("--pid", type=int, required=True, help="MCPLS daemon PID")
    parser.add_argument("--url", default="http://127.0.0.1:8445/mcp")
    parser.add_argument("--query", default="main")
    parser.add_argument("--activation-timeout", type=float, default=180.0)
    parser.add_argument("--max-active-groups", type=int, default=1)
    parser.add_argument("--project-prefix", default="mcpls43-bench")
    parser.add_argument("--output", type=pathlib.Path)
    return parser.parse_args()


def main():
    args = parse_args()
    report = run(args)
    rendered = json.dumps(report, indent=2, sort_keys=True)
    print(rendered)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n")


if __name__ == "__main__":
    main()
