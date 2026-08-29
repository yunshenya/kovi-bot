# Yunxi Conversation Concurrency v1

This document is the executable contract for concurrent inbound messages,
visible outbound delivery, and causally bound tool side effects. It applies to
every host adapter; QQ is only one adapter.

## Coordinator state machine

```text
Thinking
  +-> Cancelled
  `-> Prepared
        +-> Cancelled
        +-> Superseded
        `-> Committed
              +-> Sent
              +-> Cancelled (definite rejection only)
              `-> Unknown (acceptance uncertain after commit)
```

`Prepared` is reversible. Before it becomes `Committed`, the host must
revalidate the complete `ReplyTicket`, stop state, target route, authorization,
idempotency key, and effective envelope fingerprint. A stale or cancelled token
must not reach the platform send call.

`Committed` is the replay and decision point of no return. A later inbound
message may record a natural collision, but it must not revoke or resend the
committed action. A transport result known to have been rejected may finish as
`Cancelled`; once transport might have accepted the request, cancellation or a
dropped future produces `Unknown`, never `Cancelled`. `Unknown` remains a
duplicate-delivery and collision barrier.

## Reply ticket and erasure epoch

`ReplyTicket` carries all of:

- conversation scope;
- per-scope `scope_epoch`;
- generation;
- conversation version.

All four values participate in ticket matching. Generation and conversation
version protect normal turn ordering. `scope_epoch` protects destruction and
recreation of a scope: deleting user data removes the old reply state, and a
new state receives a new epoch. Late transport completion from the erased epoch
may finish its local send lifecycle, but it must not recreate recall history or
other deleted conversation data.

## Incoming Executive decisions

Hard failures are coordinator decisions, not semantic policy: stop, a stale
ticket, an invalid or changed route, revoked authorization, and duplicate
idempotency all fail closed.

For otherwise valid prepared content, production uses the existing semantic
understanding result and does not add a second model call:

- `Keep`: preserve the prepared ticket and content when the new turn has no
  material effect. An unrelated observation that does not need a reply may also
  keep prepared proactive content.
- `Rewrite`: supersede prepared content when the new turn invalidates it. An
  unclassified turn also rewrites non-proactive output conservatively.
- `Merge`: supersede the prepared output and regenerate once with compatible
  same-topic context; it does not send both versions.
- `Defer`: cancel the identified prepared proactive output and advance the
  incoming turn. Unknown early impact, or an unrelated turn requiring a direct
  reply, defers proactive output.

The same model call receives a bounded (at most 4096 characters), untrusted
JSON preview of the exact frozen `Prepared` envelope. The host may expose that
preview only while both its `ReplyTicket` and pending-inbound reservation still
match; it must never substitute a newer envelope. An `Unrelated` classification
for a turn that expects a direct reply therefore means `Defer` for proactive
content and `Rewrite` for reactive content. `Keep` is reserved for observations
that require no reply, so a newly asked question cannot be swallowed.

A slow semantic refinement is conditional on its original ingress ticket and
cannot reclassify a newer prepared output. A semantic stop converts retained
unsent superseded work to cancellation.

When ingress finds an existing `Prepared` envelope, it installs one bounded
pending-inbound reservation instead of destroying the envelope before semantic
understanding is available. The reservation freezes commit but does not hold a
conversation, route, or authorization lock across the model call. `Keep`
releases the original token; `Rewrite`, `Merge`, `Defer`, missing/invalid
classification, a second ingress, or reservation expiry resolves it fail
closed. An ingress with no existing Prepared envelope reserves its newly
advanced generation until the handler becomes active, so an idle proactive
fallback cannot overtake an event already accepted into an asynchronous queue.

When ingress observes a reply that is still being generated, it retains the
active reply's current ticket with a bounded reservation instead of advancing
the generation blindly. The existing semantic result then decides the outcome:
`None`, `Unrelated`, and `Unknown` keep the in-flight reply; `ExtendsPendingTopic`
merges by starting a replacement turn; `InvalidatesPendingContent` replaces it.
The active reservation blocks proactive work while classification is pending,
but it does not block the current Core turn's commit because Core processes
later events FIFO. Expiry is fail-closed and supersedes the stale generation.
If runtime submission closes or rejects capacity, the current collision and
the unsent tail are restored in original order unless erasure already removed
the scope. A Core `RejectedState` result releases the exact `MessageId`
admission reservation; it cannot strand a duplicate barrier for a message that
was never accepted.

## Linearization and lock lifetime

One conversation scope has one serialized preparation/commit point. Unrelated
scopes may use sharded locks. No scope, route, authorization, or owner guard may
be held while waiting for a model call or platform network operation.

Route identity and group-send authorization for Core actions are refreshed or
acquired immediately before commit and remain pinned through both the in-memory
and durable delivery commits. Host-side canonical-owner and group-send guards
remain pinned through their in-memory commit. Every guard is released before
the external network send. This closes lookup-to-commit races without making
identity deletion or authorization changes wait on QQ.

A sender waits for a pending-inbound reservation before acquiring those final
route/authorization guards, then pins a bounded precommit permit. Ingress that
wins before the permit may obtain a semantic decision; ingress that wins after
the permit supersedes the still-reversible token, so final commit never waits
while a security guard is held. Dropped reservations, precommit permits, cache
evictions, queue rejection, shutdown, and data erasure all release or expire
fail closed and wake blocked senders.

## Tool side-effect capability

Every non-due Core action receives the deterministic key
`event:{EventId}:intent:{index}`; due actions retain their stable
`open-loop:{OpenLoopId}:delivery:{index}` key. The current Kovi Host registers a
bounded, one-shot capability from that exact key plus a SHA-256 fingerprint of
the tool scope/name/input to the original `ReplyTicket`. Duplicate keys cannot
replace a ticket, a forged envelope cannot consume the legitimate binding, and
an evicted or missing binding fails closed. The adapter claims this capability
once and never creates a fresh ticket for an older tool plan.

Immediately before each mutating builtin and every MCP dispatch, the Host
revalidates the original ticket, canonical actor route, conversation
membership and route, main-admin/admin status, source-group authorization,
pause state, and current tool availability. MCP performs this check after
acquiring its client serialization lock. Route and authorization guards define
the pre-effect commit order, then are released before database/platform/MCP
I/O; no security guard is held across external I/O.

## Durable Core action delivery

Core `SendMessage` and `ReachOut` actions additionally use the PostgreSQL table
`yunxi_action_delivery_ledger`. A row binds one `delivery_key` to the complete
delivery envelope:

- SHA-256 envelope fingerprint;
- action and target kind plus canonical target UUID;
- canonical conversation UUID;
- destination kind and external destination ID;
- optional Core and external reply targets;
- `prepared | committed | sent | failed | unknown` status, attempt count,
  external message ID, bounded error category, and lifecycle timestamps.

The durable ledger transition is deliberately narrower than the coordinator
lifecycle:

```text
Prepared -> Committed -> Sent
                +-----> Failed  (definite rejection; identical retry only)
                `-----> Unknown (acceptance uncertain; permanent replay barrier)
```

`Unknown` is reachable only after `Committed`; it is never a pre-commit
outcome. An identical retry of `Failed` transitions through a new `Committed`
attempt before another network request.

`commit_attempt` atomically inserts/reserves the key and publishes `committed`
before the network request. Its outcomes are:

- `Acquired`: this process owns a durable committed attempt;
- `AlreadyRecorded`: a matching prepared, committed, sent, or unknown envelope
  blocks replay;
- `EnvelopeConflict`: the key was reused for a different envelope and fails
  closed.

Only a definite `failed` row may be retried, and only with the identical
envelope. A committed guard exposes `mark_sent`, `mark_failed`, and
`mark_unknown`. Dropping the guard best-effort records `unknown`; if the runtime
or database is already unavailable, the existing `committed` row remains the
restart replay barrier.

The adapter maps a recorded `sent` row to `Delivered`. A first transport result
whose acceptance cannot be established, or a matching `prepared`, `committed`,
or `unknown` replay barrier, maps to `DeliveryIndeterminate`. This is terminal
for automatic replay but is not successful delivery: Core emits
`ActionFailed(delivery_indeterminate:*)`, never `ActionSucceeded` or
`MessageSent`.

For a claimed due OpenLoop, `DeliveryIndeterminate`, `ToolFailed`, a
non-retryable `ActionPortError`, `TargetUnavailable`, and
`DeliveryResolutionFailed` are all terminal non-success outcomes. Any one of
them makes the runtime call `defer(id, None, now)`, returning the loop to `Open`
without a due time. The runtime neither resolves the loop nor leaves it cycling
through lease recovery. A retryable `ActionPortError` is not terminal and
continues through the bounded retry path.

Person-domain erasure removes attributable ledger rows in the same PostgreSQL
transaction as canonical Person and direct-conversation data. Delivery commits
take the same durable owner advisory locks and recheck that canonical owners
still exist before inserting. Portable Person export/import deliberately omits
the ledger because delivery evidence belongs to the Host, while identity unlink
retains existing replay barriers. Person import, direct-route creation, and
deletion serialize in `Person -> external route -> Conversation` lock order.
Deletion enumerates canonical direct memberships across platforms, retains
shared Group conversations, and rejects a route owned by another Person even
when the requested external identity has no current mapping.

Group-domain erasure removes the complete Core group domain, its attributable
ledger rows, and the canonical Conversation in one PostgreSQL transaction.
Before either the success receipt or a best-effort failure receipt is sent, the
Host rotates the conversation `scope_epoch`; both receipts use the tracked but
unrecorded path so a late completion cannot recreate deleted recall or ledger
state. The erasure window is a three-level barrier: the Core FIFO command purges
`WorkingState` and rejects events for the blocked Conversation, the Host clears
reference/route caches and drains the legacy group handler behind an epoch
write gate, and PostgreSQL deletion holds the Conversation owner lock. Every
database exit runs barrier cleanup; if cleanup itself fails, the scope remains
blocked fail closed.

## Visible-send boundary

The OneBot `send_msg` call is private to `MessageTransport`. Model reply
bubbles, Core actions, proactive messages, reminders, Agent Runs, Agent Tasks,
commands, status/error notices, and data-erasure receipts must all cross the
shared tracked `Prepared -> Committed` lifecycle before transport. Host
schedulers may retain their own durable claim/idempotency records, but those do
not replace conversation pre-commit validation.

Successful sends are recorded only while their ticket still belongs to the
current scope epoch. Data-erasure receipts deliberately use the tracked but
unrecorded path.

## Collision reporting

An inbound event after `Committed`, `Sent`, or `Unknown` within the bounded
collision window does not revoke delivery. It records one bounded collision
containing the committed envelope fingerprint, outgoing generation,
conversation version, and source. The Kovi bridge reliably projects it as
`WorldEvent::MessageCollisionDetected`, including when a legacy handler owns
the inbound event. If runtime submission closes or rejects capacity, the
current collision and all unsubmitted collisions are returned to the bounded
queue in their original order; a scope erased meanwhile is not recreated.

## Proactive grace

Proactive messages may wait for a bounded, configurable grace period before
commit. The current Kovi host constrains it to 300-1000 ms when enabled and
allows zero to disable it. It is not a typing simulator or an unbounded retry
delay.

## Review checklist

Any new adapter or visible-send path must prove:

- ticket epoch, stale generation, and conversation-version rejection;
- `Keep`, `Rewrite`, `Merge`, `Defer`, and stop behavior;
- duplicate key and different-envelope rejection;
- definite failure versus uncertain `Unknown` handling, including restart;
- terminal non-success unscheduling for due OpenLoops, while retryable port
  failures retain bounded retry;
- route deletion/retargeting and authorization revocation before commit;
- exact one-shot tool capability, newer-ingress rejection, and pre-effect tool
  authorization revalidation;
- absence of security or conversation locks across network I/O;
- post-commit collision projection;
- late completion after data erasure cannot rebuild deleted history;
- the platform send API cannot bypass the tracked lifecycle.
