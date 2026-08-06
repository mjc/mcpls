#!/usr/bin/env python3
"""Measure rust-analyzer readiness and retained PSS around workspace/symbol."""

import argparse
import atexit
import json
import os
import pathlib
import queue
import shutil
import subprocess
import sys
import threading
import time


def send(process, message):
    payload = json.dumps(message, separators=(",", ":")).encode()
    process.stdin.write(f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload)
    process.stdin.flush()


def read_messages(stream, messages):
    while True:
        headers = {}
        while True:
            line = stream.readline()
            if not line:
                return
            if line == b"\r\n":
                break
            name, value = line.decode().split(":", 1)
            headers[name.lower()] = value.strip()
        payload = stream.read(int(headers["content-length"]))
        messages.put(json.loads(payload))


def descendants(pid):
    pending = [pid]
    result = []
    while pending:
        current = pending.pop()
        result.append(current)
        try:
            children = pathlib.Path(
                f"/proc/{current}/task/{current}/children"
            ).read_text()
            pending.extend(int(child) for child in children.split())
        except (FileNotFoundError, ProcessLookupError):
            pass
    return result


def pss_kib(pid):
    total = 0
    for current in descendants(pid):
        try:
            with open(f"/proc/{current}/smaps_rollup", encoding="utf-8") as file:
                total += next(
                    int(line.split()[1]) for line in file if line.startswith("Pss:")
                )
        except (FileNotFoundError, ProcessLookupError, StopIteration):
            pass
    return total


def kill_process(process):
    if process.poll() is None:
        process.kill()
        process.wait()


def respond_to_server_request(process, message):
    method = message["method"]
    if method == "workspace/configuration":
        result = [None] * len(message.get("params", {}).get("items", []))
    elif method == "workspace/applyEdit":
        result = {"applied": False}
    else:
        result = None
    send(process, {"jsonrpc": "2.0", "id": message["id"], "result": result})


def handle_message(process, message, status):
    if "id" in message and "method" in message:
        respond_to_server_request(process, message)
    if message.get("method") == "experimental/serverStatus":
        status.update(message.get("params", {}))
        status["initial_load_complete"] = status.get(
            "initial_load_complete", False
        ) or status.get("quiescent", False)
    if (
        message.get("method") == "$/progress"
        and message.get("params", {}).get("token") == "rustAnalyzer/Indexing"
        and message.get("params", {}).get("value", {}).get("kind") == "end"
    ):
        status["initial_load_complete"] = True


def wait_for_response(messages, process, request_id, deadline, status):
    while time.monotonic() < deadline:
        try:
            message = messages.get(timeout=0.25)
        except queue.Empty:
            continue
        handle_message(process, message, status)
        if message.get("id") == request_id and "method" not in message:
            return message
    raise TimeoutError(f"timed out waiting for response {request_id}")


def wait_until_initial_load(messages, process, deadline, status):
    while time.monotonic() < deadline and not status.get(
        "initial_load_complete", False
    ):
        try:
            message = messages.get(timeout=0.25)
        except queue.Empty:
            continue
        handle_message(process, message, status)


def initialization_options(profile, roots):
    options = {
        "files": {
            "watcher": "client",
            "exclude": [".git", ".direnv", ".serena", "target"],
        },
        "workspace": {
            "symbol": {"search": {"kind": "all_symbols", "scope": "workspace"}}
        },
    }
    if len(roots) > 1:
        options["linkedProjects"] = [str(root / "Cargo.toml") for root in roots]
    if profile in ("mcpls", "lean"):
        options.update(
            {
                "cachePriming": {"enable": False},
                "cargo": {"allTargets": False},
                "checkOnSave": False,
            }
        )
    if profile == "lean":
        options["cargo"]["buildScripts"] = {"enable": False}
        options.update(
            {
                "lru": {"capacity": 32},
                "procMacro": {"enable": False},
            }
        )
    return options


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--rust-analyzer",
        default=os.environ.get("MCPLS_RUST_ANALYZER")
        or shutil.which("rust-analyzer"),
    )
    parser.add_argument(
        "--root",
        action="append",
        default=[],
        help="Rust project root; repeat to exercise linkedProjects",
    )
    parser.add_argument(
        "--profile", choices=("default", "mcpls", "lean"), default="lean"
    )
    parser.add_argument("--query", default="workspace_symbol_search")
    parser.add_argument("--settle-timeout", type=float, default=45.0)
    parser.add_argument("--request-timeout", type=float, default=60.0)
    parser.add_argument("--max-before-mib", type=float)
    parser.add_argument("--max-query-delta-mib", type=float)
    parser.add_argument("--max-query-ms", type=float)
    parser.add_argument("--output")
    return parser.parse_args()


def over_limit(value, limit):
    return limit is not None and value > limit


def main():
    args = parse_args()
    if sys.platform != "linux":
        raise SystemExit("PSS measurement requires Linux /proc")
    if not args.rust_analyzer:
        raise SystemExit("rust-analyzer not found; set --rust-analyzer")
    roots = [
        pathlib.Path(root).resolve()
        for root in (args.root or [pathlib.Path.cwd()])
    ]
    for root in roots:
        if not (root / "Cargo.toml").is_file():
            raise SystemExit(f"no Cargo.toml under {root}")

    stderr_path = pathlib.Path("target/benchmarks/rust-analyzer-stderr.log")
    stderr_path.parent.mkdir(parents=True, exist_ok=True)
    with stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            [args.rust_analyzer],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
        )
        atexit.register(kill_process, process)
        messages = queue.Queue()
        threading.Thread(
            target=read_messages, args=(process.stdout, messages), daemon=True
        ).start()
        status = {}
        started = time.monotonic()
        root_uri = roots[0].as_uri()
        send(
            process,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "processId": os.getpid(),
                    "rootUri": root_uri,
                    "workspaceFolders": [
                        {"uri": root.as_uri(), "name": root.name} for root in roots
                    ],
                    "capabilities": {
                        "workspace": {
                            "workspaceFolders": True,
                            "configuration": True,
                            "didChangeWatchedFiles": {"dynamicRegistration": True},
                            "symbol": {"symbolKind": {"valueSet": list(range(1, 27))}},
                        },
                        "window": {"workDoneProgress": True},
                        "experimental": {"serverStatusNotification": True},
                    },
                    "initializationOptions": initialization_options(
                        args.profile, roots
                    ),
                },
            },
        )
        initialized = wait_for_response(
            messages,
            process,
            1,
            time.monotonic() + args.request_timeout,
            status,
        )
        if "error" in initialized:
            raise RuntimeError(initialized["error"])
        initialized_ms = (time.monotonic() - started) * 1000
        send(process, {"jsonrpc": "2.0", "method": "initialized", "params": {}})
        wait_until_initial_load(
            messages,
            process,
            time.monotonic() + args.settle_timeout,
            status,
        )
        pre_query_wait_ms = (time.monotonic() - started) * 1000

        before_kib = pss_kib(process.pid)
        query_started = time.monotonic()
        send(
            process,
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "workspace/symbol",
                "params": {"query": args.query},
            },
        )
        symbol_response = wait_for_response(
            messages,
            process,
            2,
            time.monotonic() + args.request_timeout,
            status,
        )
        query_ms = (time.monotonic() - query_started) * 1000
        after_kib = pss_kib(process.pid)
        symbols = symbol_response.get("result") or []
        report = {
            "profile": args.profile,
            "roots": [str(root) for root in roots],
            "query": args.query,
            "initialized_ms": round(initialized_ms, 1),
            "pre_query_wait_ms": round(pre_query_wait_ms, 1),
            "initial_load_complete": status.get("initial_load_complete", False),
            "quiescent": status.get("quiescent", False),
            "process_count": len(descendants(process.pid)),
            "pss_before_query_kib": before_kib,
            "pss_after_query_kib": after_kib,
            "pss_query_delta_kib": after_kib - before_kib,
            "query_ms": round(query_ms, 1),
            "result_count": len(symbols),
            "stderr_path": str(stderr_path),
        }
        rendered = json.dumps(report, indent=2, sort_keys=True)
        print(rendered)
        if args.output:
            output = pathlib.Path(args.output)
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(rendered + "\n")

        send(
            process,
            {"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": None},
        )
        try:
            wait_for_response(
                messages, process, 3, time.monotonic() + 10, status
            )
            send(process, {"jsonrpc": "2.0", "method": "exit", "params": None})
            process.wait(timeout=10)
        except (TimeoutError, subprocess.TimeoutExpired):
            process.kill()
            process.wait()
        atexit.unregister(kill_process)

    failures = []
    if over_limit(before_kib / 1024, args.max_before_mib):
        failures.append("pre-query PSS")
    if over_limit((after_kib - before_kib) / 1024, args.max_query_delta_mib):
        failures.append("workspace/symbol PSS delta")
    if over_limit(query_ms, args.max_query_ms):
        failures.append("workspace/symbol latency")
    if failures:
        raise SystemExit("benchmark limits exceeded: " + ", ".join(failures))


if __name__ == "__main__":
    main()
