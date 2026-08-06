# Performance benchmarks

MCPLS keeps deterministic in-process benchmarks separate from language-server
system measurements. This prevents an improvement in one layer from hiding a
regression in another.

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
nix develop --command python3 benchmarks/rust_analyzer_memory.py \
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
