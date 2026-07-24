# WorkspaceEdit safety contract

This is the contract for previewing and applying LSP `WorkspaceEdit` values. It
is intentionally narrower than the protocol: unsupported operations fail closed
and are never silently converted into writes.

## Modes and visible operations

| Mode | Read-only queries | Preview | Apply | File operations | Commands |
| --- | --- | --- | --- | --- | --- |
| `read_only` | yes | no | no | no | no |
| `refactor` | yes | yes | no | no | no |
| `write` | yes | yes | yes, with a plan | yes, with a plan | no |

The daemon defaults to `refactor`. Preview is the normal response for rename,
formatting, and code actions; an apply request must name a plan created by the
same MCP session. A plan is single-use and expires after a short bounded
period. The plan records the project ID, canonical roots, operation list,
preconditions, and policy mode.

## Preconditions and authority

* Every target is canonicalized and checked against the registered project root.
  Symlink containment is revalidated immediately before the first commit.
* Each text edit records the file hash and the LSP document version (when one
  exists). A mismatch is stale and fails before any file is changed.
* An open dirty document is authoritative in memory. Until a later document
  synchronization contract exists, applying a plan that disagrees with its
  dirty contents fails closed.
* Overlapping edits, ambiguous ranges, unsupported resource annotations, and
  command-only code actions are rejected during preview.
* Create, rename, and delete operations require explicit write mode and are
  prevalidated along with every text edit. Cross-device rename is rejected;
  callers must preview a copy-plus-delete operation explicitly if supported in
  a future policy.

## Commit and failure semantics

Preview performs no writes. Apply validates the complete plan and resource
limits first, stages new contents in the target filesystem, then atomically
replaces each supported file. “Atomic” means each file replacement is atomic;
the whole multi-file operation is not a cross-filesystem transaction.

If a later replacement fails, the response identifies committed and uncommitted
paths and retains rollback metadata. The daemon never claims full rollback when
the filesystem cannot provide it. It stops before the first replacement for
validation failures and reports partial commit after an unavoidable runtime
failure.

Plans are session-scoped to avoid a confused-deputy apply across MCP clients.
Reconnects must create a new preview. The project actor owns the plan store and
serializes applies with other project mutations.

## Limits and unsupported protocol features

Plans have bounded file count, byte count, operation count, and lifetime.
Annotations that cannot be validated, resource operations without explicit
policy support, and command execution are rejected rather than guessed.

