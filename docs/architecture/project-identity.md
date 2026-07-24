# Project identity and path routing

Status: accepted

## Context

mcpls currently keeps workspace roots and language-server state in one
process-wide translator. The daemon needs to route many MCP sessions to
independent, persistent project environments without allowing path aliases or
nested workspaces to select the wrong owner.

## Decisions

### Stable IDs

Multi-project registration uses a caller-provided, non-empty `ProjectId`.
The single-project stdio compatibility path uses the reserved `default` ID.
The daemon does not derive IDs from display paths: paths can move, contain
secrets, or expose filesystem layout.

### Canonical roots

Registration canonicalizes an existing directory and stores both its canonical
comparison value and its original display path. Canonical roots are the
security boundary. A root that later disappears remains in persisted state as
degraded, can be inspected by explicit ID, and is not eligible for path-based
routing until revalidated.

### Duplicate registration

The registry treats an add of the same ID and canonical root as idempotent when
the effective project configuration is unchanged. The same ID with another
root, or the same root with another ID, is rejected rather than creating a
second language-server environment. The resolver itself rejects duplicate
identities so it cannot represent ambiguous state.

### Path routing

When no explicit ID is supplied, the daemon canonicalizes the existing file
path and chooses the registered root with the greatest number of path
components. This makes nested workspaces deterministic and avoids textual
prefix errors such as `project` matching `project-other`.

When both an ID and a path are supplied, both must resolve to the same project;
otherwise the request fails before any LSP call. A path outside all active
roots is rejected.

### Symlinks and missing paths

Existing paths are canonicalized before routing, so symlink aliases resolve to
the target project. Write-target validation will canonicalize existing
ancestors and revalidate immediately before commit; that policy is defined in
the WorkspaceEdit transaction ADR. A deleted or moved root is a project
lifecycle/status problem, not a reason to silently route to a different root.

### Removal and lifecycle

Project removal rejects new work, drains or cancels queued requests according
to the actor contract, waits for an active edit to reach a commit boundary,
shuts down that project's language servers, removes its live handle, and only
then persists the removal. Other projects remain available throughout.

## Consequences

- A project ID is safe to persist and use in MCP tool arguments.
- Display paths can change without changing the identity contract, but a move
  requires explicit re-registration or refresh.
- The registry, not `Translator`, owns duplicate and lifecycle decisions.
- No Cursor-specific current-working-directory assumptions are part of routing.

## Rejected alternatives

- Deriving IDs from raw paths: unstable across moves and leaks layout.
- Choosing the first matching root: ambiguous for monorepos and nested
  workspaces.
- Keeping one server per language at process scope: prevents two same-language
  projects from retaining independent LSP state.
