# CLAUDE.md - pipedream-rs

This file provides guidance for AI assistants working with this codebase.

## Overview

**pipedream-rs** is a typed, heterogeneous event relay library for Rust with:

- **Observable delivery** - if `send().await` returns Ok, message was sent (drops observable via `Dropped` events)
- Explicit completion tracking
- Error propagation from handlers back to senders
- Panic catching with error reporting
- Type-based filtering and transformation

## Architecture

### Core Types

| Type                | Purpose                                                 |
| ------------------- | ------------------------------------------------------- |
| `Relay`             | Main relay type, uses per-subscriber MPSC channels      |
| `Subscription<T>`   | Typed receiver, can be tracked or untracked             |
| `Envelope`          | Type-erased message wrapper with msg_id and tracker     |
| `CompletionTracker` | Tracks handler completion for a single message          |
| `DataStreamError`   | Error wrapper with msg_id, source, and underlying error |

### File Structure

```
src/
├── lib.rs          # Public exports
├── stream.rs       # Relay, handlers
├── envelope.rs     # Envelope type
├── subscription.rs # Subscription<T> with tracked/untracked modes
├── tracker.rs      # CompletionTracker
├── error.rs        # DataStreamError, PanicError
└── tests.rs        # All tests
```

## Key Design Decisions

### Lossless Delivery

- **No message dropping** - every message is delivered to all subscribers
- `send()` is async and authoritative - if it returns `Ok`, message was delivered
- Per-subscriber MPSC channels (not broadcast) ensure no message loss
- Slow consumers apply **backpressure** to senders
- Delivery happens **inside** `send()`, not in background tasks

### Tracked vs Untracked Subscriptions

- `subscribe()` returns **untracked** subscription - fire-and-forget, sender doesn't wait
- `subscribe_tracked()` returns **tracked** subscription - sender waits for `complete_one()`
- `sink()`/`tap()` use tracked subscriptions internally
- Subscriptions are immediately ready when created (channel exists and can buffer)
- Only tracked subscriptions participate in completion counting

```rust
// Untracked - fire and forget
let sub = relay.subscribe::<MyEvent>();

// Tracked - sender waits for completion, MUST call complete_one()
let (sub, handler_count) = relay.subscribe_tracked::<MyEvent>();

// ... process message ...
if let Some(tracker) = sub.current_tracker() {
    tracker.complete_one();
}
sub.clear_tracker();
// Decrement handler_count when done
handler_count.fetch_sub(1, Ordering::SeqCst);
```

### Tracking Boundaries

- `map()`, `filter()`, `filter_map()`, `batch()` create **tracking boundaries**
- Parent `send().await` does NOT wait for child relay handlers
- Each derived relay has its **own independent handler_count**
- Errors from child relay handlers don't propagate to parent

### Completion Flow

1. `send()` snapshots `handler_count` as `expected`
2. Message is delivered to all subscribers (lossless)
3. Each tracked handler calls `complete_one()` after processing
4. Wrong-type messages: tracked handlers complete immediately
5. `send().await` returns when `completed >= expected`

### Error Propagation

Errors flow two ways:

1. To sender: `send().await` returns `Err(SendError::Downstream(...))`
2. Through relay: subscribable via `subscribe::<DataStreamError>()`

### Close Propagation

- When parent relay is dropped/closed, all derived relays are closed
- Forwarder tasks detect parent closure and call `child.close()`
- Creating a transformation on a closed relay returns a closed child immediately

## Invariants

1. **Every tracked handler must discharge its completion** - either `complete_one()` or `fail()`
2. **Completion is explicit** - handlers call `complete_one()` immediately after processing
3. **Subscriptions are immediately ready** - channels exist and can buffer as soon as `subscribe_tracked()` returns
4. **send() waits for readiness** - ensures all subscriptions exist before delivering messages
5. **Lossless delivery** - no message dropping, backpressure is expected

## Testing

Run tests:

```bash
cargo test
```

Current test count: 87 tests (including 19 stress tests for race conditions)

## Common Patterns

### Adding a new transformation (like map/filter_map/batch)

1. Check if parent is closed first - return closed child if so
2. Create child relay with `Relay::with_channel_size(channel_size)`
3. Subscribe to parent with `self.subscribe::<T>()` (untracked)
4. Create ReadyGuard to signal readiness when forwarder is ready
5. Spawn forwarder task that:
   - Signals ready via guard
   - Loops receiving and forwarding/transforming messages
   - Calls `child_clone.close()` when parent closes

### Adding a new handler (like sink/tap)

1. Use `subscribe_tracked::<T>()` - returns `(Subscription, Arc<AtomicUsize>)`, auto-increments handler_count
2. Create `HandlerGuard` from handler_count for exception-safe decrement
3. Subscription is immediately ready (channel created, ready signaled automatically)
4. After processing: call `tracker.complete_one()` and `sub.clear_tracker()`
5. On error: create `DataStreamError`, call `relay.emit_untracked(error)` and `tracker.fail(error)`
6. HandlerGuard's Drop decrements `handler_count` when task exits

Example:
```rust
let (mut sub, handler_count) = relay.subscribe_tracked::<MyEvent>();
let handler_guard = HandlerGuard::new(handler_count);

tokio::spawn(async move {
    let _guard = handler_guard;

    while let Some(msg) = sub.recv().await {
        // ... process message ...
        if let Some(tracker) = sub.current_tracker() {
            tracker.complete_one();
        }
        sub.clear_tracker();
    }
});
```

## What NOT to Do

- Don't await sends in transformation tasks if you want fire-and-forget semantics
- Don't forget to call `clear_tracker()` after completing - prevents double-completion
- Don't use `subscribe()` when you need completion tracking - use `subscribe_tracked()`
- Don't modify `handler_count` without corresponding completion logic
- Don't share `handler_count` between parent and child relays (tracking boundaries)
