# Session subscriptions and notifications

Status: accepted

## Context

MCPLS serves multiple MCP clients from one daemon. Project actors own the
authoritative state and publish typed events, while each MCP session needs an
independent view of the resources it has chosen to follow. Notifications must
not turn a slow or disconnected client into backpressure on an actor.

## Decisions

### Explicit resource subscriptions

Subscriptions are explicit. A client calls `resources/subscribe` with a
resource URI and receives updates only for that URI. A project subscription
does not implicitly subscribe the client to every file in the project, and a
file subscription does not expose unrelated projects.

The supported resource scopes are:

- `file:///...` diagnostics for one canonical file;
- `mcpls-project-status:///PROJECT` lifecycle state for one project;
- `mcpls-project-events:///PROJECT` bounded ordered events for one project;
- `mcpls-project-events:///PROJECT?since=N` for the same event stream with a
  polling cursor.

Project events notify the project-event resource for every matching event.
Diagnostics additionally notify their file resource. Status changes, server
exit, and removal additionally notify project status. File changes and edit
completion remain available through the project event stream without creating
implicit file subscriptions.

### Session identity and ownership

Every HTTP factory invocation gets a fresh `HandlerContext` through
`McplsServer::for_session`. The context owns a new subscription set, event
sink, and edit-plan ownership set while sharing only the `ProjectRegistry` and
immutable transport/session metadata. Stdio has one server context and is
therefore one MCP session.

The peer used for `notifications/resources/updated` is attached to that
session's event sink after transport initialization. It is never stored in a
process-global subscription object.

### Ordering, buffering, and resync

Each project actor records typed events in a bounded ordered history with a
monotonic cursor. A session sink fans in actor broadcasts through a bounded
queue and filters each event against that session's explicit subscriptions.
Actor broadcasts are bounded; when a sink lags, it emits a wake-up for the
project-event resource instead of replaying an unbounded queue. The client
then reads the event resource with its last cursor. The resource reports
`resync_required` when the cursor has fallen out of the retained history.

Notification delivery is best-effort. A failed peer disconnects that session's
forwarding task and never blocks the actor or another session. Removing the
last resource for a project aborts that project's forwarding task. Project
removal also unsubscribes all tracked resources for that project.

### Polling authority and reconnects

Resource reads remain authoritative fallbacks. Reconnecting creates a new
session subscription set; clients must subscribe again and use the project
event cursor to detect and repair missed updates. No daemon-wide replay queue
is promised.

## Consequences

- Clients opt into exactly the project/file resources they need.
- Shared project actors do not share subscription state or peer handles.
- Slow-client behavior is bounded and recoverable through event-history polling.
- The event stream, rather than notification delivery timing, defines ordering.

## Rejected alternatives

- Inferring all file subscriptions from a project subscription: leaks scope and
  creates unbounded notification fan-out.
- A process-global peer/subscription cell: mixes HTTP sessions and makes
  disconnect cleanup nondeterministic.
- Blocking actors until every peer acknowledges a notification: lets one slow
  client stall unrelated projects.
