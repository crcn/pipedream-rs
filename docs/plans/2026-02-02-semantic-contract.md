---
title: Pipedream Semantic Contract
date: 2026-02-02
status: Draft
---

# Pipedream Semantic Contract

**One-page reference for pipedream-rs semantics.**

## Identity

**Pipedream is:** Observable in-process event streaming with typed heterogeneous messages.

**Pipedream is NOT:** A distributed system, message queue, or guaranteed delivery system.

---

## Core Guarantees

### 1. Observable Delivery

```rust
let (tx, rx) = Relay::channel();
tx.send(event).await?;  // Returns Ok = message was attempted
```

**What this means:**
- Messages delivered via `try_send` to all active subscribers
- If subscriber buffer is full (default: 65536), **message is dropped**
- Drops are observable via `rx.subscribe::<Dropped>()`
- `send().await` returns `Ok` even if messages were dropped
- No backpressure - senders never block waiting for slow subscribers

**Not guaranteed:**
- ❌ All subscribers receive all messages (slow subscribers miss messages)
- ❌ `send()` waits for delivery (it waits for `try_send` to complete)
- ❌ Backpressure from slow consumers

**Guaranteed:**
- ✅ Fast subscribers receive all messages they can keep up with
- ✅ Drops are observable (not silent)
- ✅ High throughput (no blocking)

### 2. Completion Tracking

```rust
relay.sink::<Event, _, _>(|event| async move {
    process(event).await?;
    Ok(())  // Must complete or fail
});

tx.send(event).await?;  // Waits for sink to complete
```

**What this means:**
- `sink()` and `tap()` use **tracked subscriptions**
- `send().await` snapshots handler count at send time
- `send().await` returns when all tracked handlers call `complete_one()`
- Raw `subscribe()` is **untracked** (fire-and-forget)

**Invariants:**
- Every tracked handler must discharge completion: `complete_one()` or `fail(error)`
- Handler count is snapshot-based (best-effort, concurrent registration may not be counted)
- Wrong-type messages auto-complete immediately

**Tracking Boundaries:**
- Transformations (`map`, `filter`, `batch`) create **independent tracking**
- Parent `send()` does NOT wait for child relay handlers
- Each relay has its own handler count

### 3. Error Propagation (Dual-Path)

```rust
relay.sink::<Event, _, _>(|event| async move {
    Err(MyError::Failed)?  // Error flows two ways
});

// Path 1: To sender
match tx.send(event).await {
    Err(SendError::Downstream(errors)) => { /* handle */ }
}

// Path 2: Through relay
let mut errors = rx.subscribe::<RelayError>();
while let Some(error) = errors.recv().await {
    log::error!("Handler failed: {:?}", error);
}
```

**What this means:**
- Handler errors returned from `send().await` via `tracker.fail(error)`
- Handler errors also emitted to relay as subscribable `RelayError` events
- First error wins (no aggregation)
- Panics caught with `catch_unwind`, converted to `PanicError`

### 4. Panic Resilience

```rust
relay.sink::<Event, _, _>(|event| async move {
    panic!("oops");  // Caught and converted to error
});
```

**What this means:**
- Panics in `sink()` and `tap()` are caught
- Panics converted to `PanicError` wrapped in `RelayError`
- Error propagates via dual-path (to sender AND through relay)
- Handler task exits cleanly

**Forwarder panics (Tier 0 fix):**
- Panics in `map()`, `filter()`, etc. should also be caught
- Emit `RelayError::HandlerPanic` and close child relay

---

## Anti-Patterns

### 1. Circular Dependency (DEADLOCK)

**❌ WRONG:**
```rust
relay.sink::<Event, _, _>(|event| async move {
    relay.send(derived).await?;  // DEADLOCK!
    Ok(())
});
```

**Why:** Handler waits for `send()` to complete, but `send()` waits for handler to complete.

**✅ CORRECT:**
```rust
let child = relay.filter::<Event, _>(|_| true);
relay.sink::<Event, _, _>(move |event| async move {
    child.send(derived).await?;  // Different relay, OK
    Ok(())
});
```

**Rule:** **Tracked handlers must only send forward (downstream), never upstream or to self.**

### 2. Forgetting `complete_one()`

**❌ WRONG:**
```rust
let (sub, count) = relay.subscribe_tracked::<Event>();
loop {
    let msg = sub.recv().await;
    process(msg);
    // Forgot to call complete_one()!
}
```

**Result:** `send().await` hangs forever.

**✅ CORRECT:**
```rust
if let Some(tracker) = sub.current_tracker() {
    tracker.complete_one();
}
sub.clear_tracker();
```

### 3. Bidirectional Cycles

**❌ WRONG:**
```rust
relay1.forward(&relay2);
relay2.forward(&relay1);  // Infinite loop!
```

**Protection:** Echo prevention via `origin` field detects and breaks cycles.

---

## Type System

### Message Types

```rust
// Any Send + Sync + 'static type
struct MyEvent { data: String }

tx.send(MyEvent { data: "hello".into() }).await?;
```

**Type filtering:**
- Envelope stores `TypeId` for O(1) type checking
- Subscribers filter locally via `TypeId::of::<T>()`
- Wrong-type messages skipped by subscriber

**Heterogeneous:**
```rust
tx.send(EventA { ... }).await?;
tx.send(EventB { ... }).await?;  // Same relay, different types
```

### Subscriptions

```rust
// Untracked - fire and forget
let mut sub = rx.subscribe::<Event>();

// Tracked - sender waits (internal use only)
let (sub, handler_count) = rx.subscribe_tracked::<Event>();
```

**When to use:**
- `subscribe()` - Most cases, no completion tracking needed
- `subscribe_tracked()` - Internal use by `sink()` and `tap()` only

---

## Lifecycle

### Channel Ownership

```rust
let (tx, rx) = Relay::channel();
// tx is sole owner (NOT Clone)
// rx is cloneable observer

drop(tx);  // Closes channel
assert!(rx.is_closed());
```

**Semantics:**
- `RelaySender` dropping closes the relay
- `RelayReceiver` clones don't keep relay alive
- Subscribers receive `None` when relay closes

### Close Propagation

```rust
let child = parent.filter::<T, _>(predicate);
drop(parent);  // Closes parent

// Forwarder task detects closure and calls:
child.close();
assert!(child.is_closed());
```

**Semantics:**
- Derived relays hold **weak references** to parent
- Forwarder tasks detect parent closure (`recv() returns None`)
- Forwarder calls `child.close()` before exiting
- Cascade close ensures no dangling relays

---

## Performance Characteristics

### Observable Delivery

**Throughput:** High (no blocking, `try_send` is immediate)

**Latency:** Low (no coordination between subscribers)

**Memory:** O(N × M) where N = messages, M = subscribers
- Each subscriber has independent buffer (default: 65536 messages)
- Slow subscribers can cause memory pressure

**Buffer Sizing:**
```rust
let (tx, rx) = Relay::channel_with_size(4096);  // Tune for workload
```

**Monitoring:**
```rust
let mut dropped = rx.subscribe::<Dropped>();
while let Some(drop) = dropped.recv().await {
    metrics::increment("relay.drops", 1);
}
```

### Completion Tracking Overhead

**Tracked handlers:**
- Atomic increment on `handler_count` during registration
- Atomic increment on `completed` during `complete_one()`
- Notify wakeup when `completed >= expected`

**Untracked handlers:**
- No atomic operations
- No waiting in `send()`

**Trade-off:** Use tracked handlers (`sink`, `tap`) when coordination needed, untracked (`subscribe`) for fire-and-forget.

---

## Ready Signals (Tier 0 Fix)

### Current Behavior (Bug)

```rust
let child = parent.filter::<T, _>(predicate);
// Race: concurrent sends may see different wiring states
```

**Problem:** `notify_one` permits are single-consumer. Second sender skips wiring.

### Fixed Behavior

```rust
// ReadyGuard::drop uses notify_waiters()
// All concurrent sends wait for wiring
```

**Result:** Ready signal becomes a proper barrier.

---

## Summary Table

| Property | Guarantee |
|----------|-----------|
| **Delivery** | Observable - drops on full buffers |
| **Drops** | Observable via `Dropped` events |
| **Backpressure** | None - senders never block |
| **Completion** | Tracked handlers block sender |
| **Error Flow** | Dual-path: to sender + through relay |
| **Panics** | Caught, converted to `PanicError` |
| **Types** | Heterogeneous, compile-time safe |
| **Lifecycle** | `RelaySender` drop closes relay |
| **Close** | Cascades to derived relays |
| **Tracking** | Snapshot-based, best-effort |

---

## Quick Decision Guide

**Use pipedream when:**
- ✅ High-throughput in-process event streaming
- ✅ Observable drops are acceptable
- ✅ Single-process application
- ✅ Type-safe pub/sub needed
- ✅ Async handler coordination needed

**Use something else when:**
- ❌ Need guaranteed delivery
- ❌ Need distributed messaging
- ❌ Need persistence or durability
- ❌ Need exactly-once semantics
- ❌ Need cross-process communication

**The boundary:** If your system needs durability, distribution, or guaranteed delivery, pipedream is the wrong tool.

---

## Version

- **Contract Version:** 1.0 (Draft)
- **Library Version:** 0.1.0 (pre-1.0)
- **Date:** 2026-02-02
- **Status:** Pending Tier 0 fixes

**Next steps:**
1. Fix README.md to match this contract
2. Fix ready signal races
3. Add forwarder panic supervision
