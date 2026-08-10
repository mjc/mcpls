# Performance benchmarks

MCPLS keeps deterministic in-process benchmarks separate from language-server
system measurements. This prevents an improvement in one layer from hiding a
regression in another.

## No-reread evaluation

`benchmarks/no-reread-corpus.json` contains scrubbed query shapes for workspace
lookup, a large bounded outline, definition/hover, handle follow-ups,
references/call hierarchy, diagnostics, structured results, and
`inspect_symbol`. It contains fixture-relative identifiers and assertion names,
not user prompts, chat prose, home paths, or proprietary source. The ordinary
`mcpls-bench` tests validate that privacy and coverage contract:

```sh
cargo nextest run -p mcpls-bench no_reread
```

The ignored real-server lane executes the same corpus against the checked-in
Rust fixture and verifies exact-first ranking, declaration source, coherent
paths/ranges, byte/item limits, and complete highlighted code:

```sh
cargo build -p mcpls
MCPLS_RA_FILTER=sc_no_reread_corpus \
  cargo nextest run -p mcpls-core --test ra_e2e --run-ignored all \
  -E 'test(/ra_e2e_suite/)'
```

For a repeatable agent/task evaluation, aggregate local Codex histories without
copying or exporting them:

```sh
cargo run -p mcpls-bench --bin no-reread-eval -- \
  --history ~/.codex/sessions \
  --output target/benchmarks/no-reread-history.json
```

The evaluator reads histories locally and emits only counts grouped by MCPLS
tool. It ignores messages and prompts, reduces paths to a `src/...` suffix or
basename, and never emits commands, result bodies, or source. Its adjacency
classifier reports post-semantic same-file reads and pre-coordinate source
reads as exact numerator/denominator rates, plus total result bytes and latency,
and truncation, unsupported, and error rates. These are workflow heuristics,
not a model-quality score; model selection remains outside required CI.

To evaluate another instrumented runner, serialize its scrubbed events as a JSON
array of `semantic` and `source_read` records and replace `--history` with
`--trace`. `benchmarks/no-reread-baseline.json` preserves the original
privacy-safe history counts and a target for every result-enrichment ticket.
Compare like-for-like agent, model, repository fixture, and task corpus runs;
tool-only fixture success is necessary quality evidence but is not evidence that
an agent stopped rereading files.

## Gungraun hot paths

Enter the repository's pinned development shell and run:

```sh
nix develop --command \
  cargo bench -p mcpls-core --features bench --bench core_hot_paths
```

The suite records Callgrind instruction/cache metrics and DHAT heap metrics for:

- ignore-aware native watch directory discovery;
- rust-analyzer option generation for one project and five linked roots;
- longest-root project routing;
- recursive nested-project marker discovery.

Setup builds the fixture outside the measured function. Keep the one-root and
five-root cases together: changes that make a single project cheaper must not
make linked worktrees scale badly.

## Real rust-analyzer memory

Gungraun cannot describe latency or retained memory in an external
rust-analyzer process. On Linux, run:

```sh
nix develop --command cargo run -p mcpls-bench --bin rust-analyzer-memory -- \
  --profile mcpls \
  --root "$PWD" \
  --output target/benchmarks/rust-analyzer-memory.json
```

The report includes initialization latency, the initial-indexing wait and
quiescence state, process count, PSS before and after `workspace/symbol`, query
latency, and result count. Repeat `--root` for compatible worktrees to exercise
`linkedProjects`. Use `--profile mcpls` for the deployed low-memory settings
while keeping cache priming, proc macros, and build scripts enabled. Use
`--profile mcpls-no-priming` to isolate the cost of disabling cache priming. The
`lean` profile is an intentionally aggressive stress profile that also disables
proc macros and build scripts; it is not the deployed MCPLS configuration. Initial
indexing gates the query; rust-analyzer's user-facing quiescence flag is
reported but does not.

Optional `--max-before-mib`, `--max-query-delta-mib`, and `--max-query-ms`
limits turn a recorded measurement into a failing regression guard. Establish
limits on the same host and rust-analyzer version; PSS and wall time are not
portable enough for a shared hosted-runner threshold.

## Regression matrix

| Change area | Gungraun guard | System guard |
| --- | --- | --- |
| watcher exclusions/events | watch scan instructions and DHAT allocations | inotify count during a live soak |
| project/worktree sharing | five-root option generation and root routing | one RA process for compatible roots |
| rust-analyzer defaults | option-generation cost | pre-query PSS and readiness |
| symbol search | none; work occurs in RA | query latency, result count, and retained PSS delta |
| resident-project policy | project routing | total RA process/PSS budget across 20 chats |

The final two live-daemon checks belong in the resident-budget benchmark tracked
by MCPLS-43; the direct rust-analyzer benchmark is the reproducible lower layer.

### Resident-budget matrix

The daemon-level measurement requires a manifest containing four real Git
repositories, with five existing linked-worktree roots for each repository:

```json
{
  "projects": [
    {
      "project_id": "repository-a",
      "roots": ["/path/a-1", "/path/a-2", "/path/a-3", "/path/a-4", "/path/a-5"]
    },
    {
      "project_id": "repository-b",
      "roots": ["/path/b-1", "/path/b-2", "/path/b-3", "/path/b-4", "/path/b-5"]
    },
    {
      "project_id": "repository-c",
      "roots": ["/path/c-1", "/path/c-2", "/path/c-3", "/path/c-4", "/path/c-5"]
    },
    {
      "project_id": "repository-d",
      "roots": ["/path/d-1", "/path/d-2", "/path/d-3", "/path/d-4", "/path/d-5"]
    }
  ]
}
```

The runner refuses fewer than four projects, fewer than five roots per project,
duplicate roots, non-Git roots, or roots from different Git common directories
within one project, or roots without explicit `rust-toolchain` metadata; this
keeps an incomplete or fabricated local checkout from being reported as the
acceptance matrix. Run it against the live MCPLS daemon with its PID:

```sh
cargo run -p mcpls-bench --bin mcpls-residency -- \
  --manifest target/benchmarks/mcpls-residency.json \
  --pid "$(systemctl --user show mcpls.service -p MainPID --value)" \
  --activation-timeout 180 \
  --max-active-groups 1 \
  --output target/benchmarks/mcpls-residency-report.json
```

It records daemon-only PSS and the daemon descendant process count/names after
registration, then the same PSS/process snapshot plus activation-to-first
authoritative `Ready`/`Degraded` `workspace_symbol_search` result time across a
forward and reverse group-switch sequence. The process snapshot is what lets
the report distinguish one rust-analyzer process plus its children from a
second resident analyzer. Symbol counts are read from MCPLS's
`{"symbols": [...]}` result object. Registration fails if a logical project
does not collapse to one actor group, and every switch fails if more than the
configured active-group limit is observed. Registrations are removed in a
`finally` cleanup block. The manifest must be assembled from existing
worktrees; do not manufacture repeated paths to make the matrix pass. A
switch also fails if the sampled process tree contains more `rust-analyzer`
processes than the configured active-group limit, or if an active Rust group
has no sampled `rust-analyzer` process. The latter rejects fallback-only
results: the benchmark must measure a resident analyzer, not merely a
successful lexical or degraded response.
