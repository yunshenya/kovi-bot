# Yunxi Conversation Concurrency v1

This document is the executable contract for concurrent inbound messages and
outbound replies. It applies to every host adapter; QQ is only one adapter.

## State machine

```text
Thinking -> Prepared -> Committed -> Sent
                         |             |
                         v             v
                      Cancelled     (collision event)
Prepared -> Superseded
```

`Prepared` is still reversible. Before `Committed`, the host must revalidate
the latest reply ticket, generation, conversation version, stop state, target
route, authorization, idempotency key, and message fingerprint. A stale or
cancelled token must not reach the platform send call.

`Committed` is the irreversible side-effect boundary. A later message may
record a natural collision, but it must not silently resend or retract the
already committed action.

## Linearization

One conversation scope has one serialized commit point. The implementation may
use sharded locks across unrelated scopes, but it must not hold a scope lock
while waiting for a model call or external network operation. A new inbound
event advances the generation and conversation version, superseding prepared
outgoing messages and cancelling active work when the host policy requires it.

The current Kovi adapter implements this contract in
`plugins/model/src/model/interrupt.rs`:

- `ReplyTicket` carries generation and conversation version;
- `PendingOutgoing` tracks `Prepared`, `Committed`, `Sent`, `Cancelled`, and
  `Superseded`;
- `commit_outgoing` is the pre-send linearization point;
- collision records become `WorldEvent::MessageCollisionDetected`.

## Proactive grace

Proactive messages may wait for a bounded, configurable grace period before
commit. The period is short and may be disabled; it is not a typing simulator
and must never be an unbounded retry delay.

## Review checklist

Any new adapter or action path must test stale-generation rejection, duplicate
idempotency keys, cancellation before commit, send failure after commit, and
post-commit collision reporting. It must also prove that a route lookup cannot
send to an ambiguous or deleted identity.
