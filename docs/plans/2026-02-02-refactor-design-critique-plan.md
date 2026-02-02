---
title: Design Critique - pipedream-rs Architecture Review
type: refactor
date: 2026-02-02
---

# Design Critique - pipedream-rs Architecture Review

## Overview

Comprehensive design critique of the pipedream-rs event relay library to sharpen its architecture, API ergonomics, and semantic consistency. This review identifies strengths, concerns, and opportunities for improvement across the entire codebase.

## Current State Analysis

### Actual Public API

**Channel API (Recommended):**
```rust
// Creation
let (tx, rx) = Relay::channel();                    // Default buffer: 65536
let (tx, rx) = Relay::channel_with_size(4096);      // Custom buffer

// Sending (RelaySender - NOT Clone)
tx.send(value).await?;                               // Type-inferred
tx.send_any(arc_value, type_id).await?;             // Type-erased
let weak_tx = tx.weak();                             // WeakSender (Clone)

// Subscribing (RelayReceiver - Clone)
let sub = rx.subscribe::<String>();                  // Untracked (fire-and-forget)
let (sub, count) = rx.subscribe_tracked::<String>(); // Tracked (internal use)

// Transformations (return new Relay)
let filtered = relay.filter::<T, _>(predicate);
let mapped = relay.map::<T, U, _>(transform);
let batched = relay.batch::<T>(100);
let (matched, rest) = relay.split::<T>();

// Handlers
relay.sink::<T, _, _>(|msg| { /* consume */ });     // Terminal, tracked
relay.tap::<T, _, _>(|msg| { /* observe */ });      // Pass-through, tracked
relay.within(|| async { /* custom */ });             // Untracked

// Forwarding
source.forward(&target).await;                       // Blocks until source closes
```

**Hidden/Deprecated APIs:**
- `Relay::new()`, `Relay::with_channel_size()` - marked `#[doc(hidden)]`, loopback API
- `pipe()`, `pipe_to()`, `pipe_from()` - deprecated in favor of `forward()`
- `PipeSource`, `PipeSink`, `Readable`, `Writable` - internal traits, `#[doc(hidden)]`

### Library Characteristics

**Core Purpose:** Typed, heterogeneous event relay library with:
- Lossless delivery guarantees
- Explicit completion tracking
- Error propagation from handlers to senders
- Panic resilience
- Type-based filtering and transformation

**Maturity:** v0.1.0 (early stage with some API evolution)

**Test Coverage:** 108 tests including 19 stress tests for race conditions

**Documentation:** Excellent (CLAUDE.md, design-doc.md, SEMANTICS.md, comprehensive README)

### Key Strengths

1. **Clear Ownership Model** - Channel API (`RelaySender`/`RelayReceiver`) with explicit lifecycle semantics
2. **Type-Safe Heterogeneity** - Single relay carries multiple message types with compile-time safety
3. **Explicit Completion Tracking** - Snapshot-based model with tracked vs. untracked subscriptions
4. **Panic Resilience** - RAII guards and catch-unwind patterns throughout
5. **Cascade Close Semantics** - Weak references ensure derived streams close with parents
6. **Dual Error Propagation** - Errors flow both to sender and through relay as subscribable events
7. **Echo Prevention** - Origin tracking prevents infinite loops in bidirectional pipes
8. **Comprehensive Testing** - Stress tests, invariant tests, regression tests, soak tests

## What Pipedream IS and IS NOT

**Pipedream's Identity:** Observable in-process event streaming with typed heterogeneous messages.

### What Pipedream IS ✅

- **Observable event relay** - Fast broadcast with drop detection via `Dropped` events
- **Type-safe transformations** - Compile-time type checking with runtime filtering
- **Explicit completion tracking** - Coordinate async handlers with sender
- **Panic-resilient** - Catches panics in handlers, converts to errors
- **In-process only** - Single-process, high-throughput event streaming
- **Heterogeneous messaging** - Multiple message types in single relay

**Best for:**
- Logging and metrics collection
- Application event streaming
- Monitoring and observability pipelines
- In-process pub/sub patterns
- High-throughput event fanout

### What Pipedream IS NOT ❌

Pipedream is **not** a distributed system or guaranteed delivery queue.

**It is NOT:**
- ❌ **A guaranteed delivery queue** - Use RabbitMQ, SQS, or disk-backed queues
- ❌ **A replacement for Kafka / NATS / message brokers** - Use those for distributed systems
- ❌ **A transactional system** - No rollback, no exactly-once semantics, no persistence
- ❌ **A state machine runtime** - See statecharts libraries (XState, etc.)
- ❌ **A distributed system** - Single-process only, no network transparency
- ❌ **An actor framework** - No supervision trees, no location transparency
- ❌ **A database** - No persistence, no queries, no durability

**Use something else for:**
- Inter-service communication (use gRPC, message brokers)
- Durable event storage (use event stores, Kafka)
- Distributed coordination (use etcd, Consul)
- Reliable work queues (use Sidekiq, Celery)
- Cross-process communication (use message brokers)

**The boundary:** If your system needs durability, distribution, or guaranteed delivery, pipedream is the wrong tool.

## Critical Issues Requiring Resolution

### 1. **Documentation vs. Implementation Mismatch (CRITICAL)**

**Problem:** README.md and Subscription.rs docs claim "lossless delivery" with "backpressure from slow consumers," but implementation uses `try_send` which drops messages when buffers are full. This is **broadcast semantics**, not MPSC with backpressure.

**Evidence - Code:**
- `src/stream.rs:361-370` uses `tx.try_send()` which returns `TrySendError::Full` when buffer is full
- Line 364: `dropped = true` - messages ARE dropped, not queued
- `src/stream.rs:375-391` emits `Dropped` event to make loss observable (but not prevent it)
- `tests/dropped_visibility.rs` explicitly tests message dropping with small buffers
- `tests/blocking_check.rs` proves sender does NOT block - 100 messages with buffer of 1 completes quickly

**Evidence - Documentation Conflict:**
- **README.md (lines 281-289)** claims:
  - "No message dropping - every message is delivered to all subscribers"
  - "send() is async and authoritative - if it returns Ok, message was delivered"
  - "Backpressure - slow consumers apply backpressure to senders"
- **Subscription.rs (lines 14-17)** claims: "All subscriptions receive all messages. There is no message dropping."
- **BUT design-doc.md (lines 149-157)** *contradicts README*:
  - "pipedream-rs uses broadcast semantics. Slow consumers may miss messages."
  - "Message loss is silent and intentional."

**Reality:**
- `send().await` does NOT await delivery to subscribers - it just calls `try_send` immediately
- No backpressure mechanism exists - sender never blocks waiting for slow consumers
- Message drops are NOT returned as errors - `send()` returns `Ok()` even when messages are dropped
- `Dropped` events are emitted for observability but don't prevent loss

**Impact:**
- Users reading README expect MPSC backpressure semantics but get broadcast semantics
- Silent data loss under load (only observable via `Dropped` event subscription)
- The design-doc.md is honest about broadcast behavior, but README misleads users
- Semantic contract mismatch: library behavior doesn't match its public API documentation

**Resolution Strategy:**

**Immediate (Tier 0 - Define Identity):**
1. **Adopt "Observable Delivery" as core identity**
   - Remove "lossless delivery" claims from README.md and Subscription.rs
   - Rename to "Observable Delivery" - frames drops as observability feature
   - Document `Dropped` events as the monitoring mechanism
   - This IS what pipedream is: observable in-process event streaming

**Future (Tier 2 - Only After Real Users Prove Need):**
2. **Consider Reliable mode as advanced opt-in**
   - Add builder pattern with `.delivery_mode(Reliable)` if users need it
   - **Observable** = current `try_send` behavior (THE default, THE identity)
   - **Reliable** = `send().await` with backpressure (advanced, foot-gun territory)
   - Frame Reliable as: "For command buses / event sourcing only. Not recommended for high fan-out."
   - Document performance implications explicitly

**Recommendation:** Start with Tier 0 only. Keep Observable as pipedream's identity. Don't solve hypothetical problems. Add Reliable mode ONLY when real users prove they need it.

**Critical Insight:** Observable is not "the fast mode" - it's THE mode. Reliable is an escape hatch, not a peer.

### 2. **Ready Signal Race Conditions**

**Problem:** Ready signals use single-consumer permits which can cause races when concurrent sends happen during filter setup.

**From SEMANTICS.md:**
> Ready signals are single-consumer, not a barrier. If two sends race:
> - One waits for wiring
> - The other proceeds without waiting

**Scenario:**
```rust
let (tx, relay) = Relay::channel();

// Spawn filter
tokio::spawn(async move {
    let filtered = relay.filter::<i32, _>(|x| x > 5);
    // Wiring happens here...
});

// Race: Concurrent sends
tokio::join!(
    tx.send(3),  // May wait for wiring
    tx.send(7)   // May skip wiring
);
```

**Impact:**
- Send B might deliver before wiring completes
- Message ordering can be unexpected
- Users must manually synchronize filter setup before concurrent sends

**Resolution Options:**
1. Add explicit `.ready().await` barrier method users must call
2. **Make ready signals multi-consumer (RECOMMENDED):** Change from `notify_one` to `notify_waiters` to create a true barrier
3. Document limitation prominently and provide synchronization patterns

**Recommendation:** **Option 2** - Change to `notify_waiters()` to make ready signals a proper barrier. This ensures all concurrent sends wait for wiring completion, not just one. Key benefits:
- Eliminates race where second sender skips wiring
- Matches user expectations for transformation setup
- Simple implementation change with significant correctness improvement
- Makes "ready signal" actually mean "ready for all senders"

**Implementation:**
```rust
// In ReadyGuard::drop()
self.relay.ready_notify.notify_waiters();  // Was: notify_one()
```

This is a behavior change but improves correctness. Document in changelog as bug fix.

### 3. **Forwarder Task Panic Safety (Supervisor Pattern)**

**Problem:** Forwarder tasks in transformations (map, filter, batch) may not catch panics, potentially leaving child relays open and subscribers hanging.

**Scenario:**
```rust
let mapped = relay.map::<i32, i32, _>(|x| {
    if x == 13 { panic!("unlucky!"); }
    x * 2
});

// If transformation panics:
// - Forwarder task dies
// - Child relay stays open
// - Subscribers block forever on recv()
```

**Evidence:** Sink/tap use `AssertUnwindSafe + catch_unwind` but transformation forwarders don't have explicit panic documentation.

**Impact:**
- Silent forwarder task death
- Resource leaks (unclosed relays)
- Hangs in downstream code
- No error propagation to sender or subscribers

**Resolution (Supervisor Pattern):**

Implement comprehensive panic handling with error emission:

```rust
// In map/filter/batch forwarder loops
loop {
    match parent.recv().await {
        Some(msg) => {
            // Catch panics in transformation
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                transform(&msg)
            }));

            match result {
                Ok(transformed) => {
                    // Forward successfully
                    child.send(transformed).await.ok();
                }
                Err(panic_payload) => {
                    // Emit panic as error
                    let error = RelayError::HandlerPanic {
                        msg_id: msg.msg_id(),
                        type_id: TypeId::of::<T>(),
                        payload: panic_payload,
                    };
                    parent.emit_untracked(error);

                    // Close child and exit forwarder
                    child.close();
                    break;
                }
            }
        }
        None => {
            child.close();
            break;
        }
    }
}
```

**Benefits:**
- Panics don't leave zombie relays
- Errors propagate to both sender and subscribers
- Makes "Panic Resilience" claim actually true for transformations
- Consistent with sink/tap panic handling

**Tests Needed:**
- Forwarder panics in map closure → child closes, error emitted
- Forwarder panics in filter closure → child closes, error emitted
- Forwarder panics in batch transform → child closes, error emitted

## Semantic Clarity Issues

### 4. **Tracking Boundary Documentation Incomplete**

**Problem:** Different documents list different transformations as creating tracking boundaries.

**Evidence:**
- SEMANTICS.md: Only mentions `map()`
- CLAUDE.md: Lists `map()`, `filter()`, `filter_map()`, `batch()`
- design-doc.md: Doesn't explicitly enumerate tracking boundaries

**Questions Requiring Clarification:**
1. What happens if a tracked handler awaits a `send()` on a parent relay? Can this deadlock?
2. For `batch()`: Does buffering participate in parent tracking or is it isolated?
3. If child relay is closed during parent's mid-send, does forwarder exit cleanly?

**Resolution:**
- Expand SEMANTICS.md with explicit table of ALL transformations and their tracking behavior
- Add examples showing handler awaits (safe vs. deadlock scenarios)
- Document close-during-send behavior with sequence diagrams

### 5. **Handler Count Concurrency Model Lacks Formal Guarantees**

**Problem:** Handler count is "best effort" - handlers registered during `send()` may or may not be counted.

**From SEMANTICS.md:**
> Concurrent registration: Handlers registered _during_ `send()` may or may not be counted.

**Scenario:**
```rust
let (tx, relay) = Relay::channel();

tokio::spawn(async move {
    relay.sink::<i32, _, _>(|n| println!("{}", n));
});

// Race: is the sink counted?
tx.send(42).await?;  // May complete before sink registers
```

**Impact:**
- `send().await` may return before newly-spawned handlers run
- No way to wait for all handlers to be wired up
- Error-prone for users who spawn handlers dynamically

**Resolution Options:**
1. Add `.wait_for_wiring()` explicit barrier method
2. Add debug assertion mode validating handler_count matches actual tracked handlers
3. Document limitation and provide synchronization patterns

**Recommendation:** Option 1 + 3 - Add explicit barrier for users who need it, document that most users don't need perfect synchronization.

### 6. **WeakSender Upgrade Timing Not Documented**

**Problem:** When forwarder tasks exit, derived streams become unusable, but exact timing is unclear.

**Close Sequence:**
1. Parent `close()` called
2. Forwarder task receives `None` from parent
3. Forwarder calls `child.close()`
4. Weak upgrade fails on child operations

**Questions:**
- When does `child.send()` start returning `Err(SendError::Closed)`?
- Can there be a race where child appears open but weak upgrade fails?
- How should users drain messages from dying child relays?

**Resolution:**
- Document exact close propagation timeline with state transitions
- Add explicit tests for close propagation timing
- Provide graceful drain patterns in examples

## API Ergonomics Improvements

### 7. **Builder Pattern for Configuration (Tier 2 - Post 1.0)**

**Note:** Defer this until real users prove they need Reliable mode. Focus on documenting Observable semantics first.

**Current API (keep as-is for now):**
```rust
let (tx, rx) = Relay::channel();               // Observable by default
let (tx, rx) = Relay::channel_with_size(4096); // Tune buffer size
```

**Future (only if needed):**
```rust
// Most users: Observable is pipedream's identity
let (tx, rx) = Relay::channel();  // Observable, 65536 buffer

// Advanced users only: Reliable mode
// ⚠️  For command buses / event sourcing ONLY
// ⚠️  Not recommended for high fan-out
let (tx, rx) = Relay::builder()
    .delivery_mode(DeliveryMode::Reliable)
    .channel_size(1024)  // Smaller buffer OK with backpressure
    .build();
```

**If implemented (Tier 2), frame it this way:**
```rust
pub enum DeliveryMode {
    /// Observable delivery (DEFAULT - this is pipedream's identity)
    /// - High throughput with try_send
    /// - Drops are observable via Dropped events
    /// - No backpressure on senders
    /// - Use for: logging, metrics, monitoring, event streaming
    Observable,

    /// ⚠️ ADVANCED: Lossless delivery with backpressure
    /// - send().await blocks until delivered to ALL subscribers
    /// - Slow consumers slow down ALL senders
    /// - ⚠️ NOT recommended for high fan-out scenarios
    /// - Use for: command buses, event sourcing with few subscribers
    #[doc(hidden)]  // Or similar visibility control
    Reliable,
}
```

**Why defer:**
- Don't solve hypothetical problems
- Observable IS pipedream's identity
- Reliable changes the mental model fundamentally
- Wait for real user need before committing to second mode

**If added later, document clearly:**
```markdown
⚠️ Reliable mode is for specialized use cases only.
Most users should use the default Observable mode.
```

### 8. **Error Aggregation Option**

**Current Behavior:** First error wins, other handler errors are lost.

**Use Case:** Debugging - want to see ALL handler failures, not just the first.

**Proposed:**
```rust
relay.sink_with_config::<T, _, _>(
    SinkConfig::builder()
        .error_mode(ErrorMode::FirstWins)  // or ErrorMode::Aggregate
        .build(),
    |msg| { /* handler */ }
);
```

**Benefits:**
- Better debugging experience
- Optional for performance-sensitive paths
- Backward compatible (default to FirstWins)

### 9. **Type-Safe Batching**

**Current API:**
```rust
let batched = relay.batch::<i32>(100);  // Runtime batch size
```

**Proposed:**
```rust
let batched = relay.batch::<i32, { BatchSize::Fixed(100) }>();  // Const generic
```

**Benefits:**
- Batch size in type system
- Compile-time validation
- Better optimization opportunities
- Note: Requires const generics stabilization

## Performance Considerations

### 10. **Arc Allocation Overhead**

**Current:** Every message allocates `Arc<dyn Any>` envelope.

**Concern:** At 10k msg/sec = 10k allocations/sec.

**Questions:**
1. Have you benchmarked against stack-allocated + memcpy for small payloads?
2. What about type-specific channels (T-channels) for hot paths?
3. Does Arc contention show up in profiling?

**Resolution:**
- Add comprehensive benchmark suite comparing:
  - Current Arc<dyn Any> approach
  - Stack-allocated messages with memcpy
  - Type-specific channels (no type erasure)
  - Batched sends (amortize Arc allocation)
- Document expected allocation patterns and when to use each approach
- Consider zero-copy optimization for single-subscriber case

### 11. **Type-Indexed Channel Partitioning**

**Current:** All message types share one broadcast channel, type filtering at subscription.

**Alternative:** Per-type channels with routing.

**Trade-offs:**
- Current: Simple, one channel, O(1) type check per recv()
- Alternative: Complex, N channels, no filtering overhead, better contention profile

**Resolution:**
- Benchmark both approaches with representative workloads
- Consider offering both as configuration options
- Document when to use which approach

## Testing Enhancements

### 12. **Property-Based Testing**

**Current:** 108 example-based tests.

**Missing:** Property-based tests with random inputs to find edge cases.

**Proposed:**
```rust
#[test]
fn prop_completion_tracking_invariant() {
    proptest!(|(
        msg_count in 1..1000usize,
        handler_count in 1..100usize,
    )| {
        // Generate random send/handler patterns
        // Assert: send().await returns when all handlers complete
    });
}
```

**Properties to Test:**
- Completion count always reaches expected count
- No messages lost (if lossless mode)
- Close propagates to all children
- Error propagation is consistent

### 13. **Memory Leak Detection**

**Current:** No explicit leak tests.

**Risk:** Cycles in complex relay topologies.

**Proposed:**
```rust
#[test]
fn test_no_leaks_in_circular_topology() {
    let weak_check = {
        let relay1 = Relay::new();
        let relay2 = Relay::new();
        relay1.forward(&relay2);
        relay2.forward(&relay1);  // Cycle!

        Arc::downgrade(&relay1.inner)
    };

    // Drop everything

    assert!(weak_check.upgrade().is_none());  // Should be dropped
}
```

### 14. **Benchmark Suite**

**Needed:**
- Throughput: messages/sec at different channel sizes
- Latency: P50, P95, P99 from send to recv
- Memory: bytes per message, growth rate
- Contention: performance with many subscribers
- Transformation overhead: map/filter/batch cost

**Compare Against:**
- tokio::sync::broadcast
- crossbeam channels
- flume
- async-channel

## Documentation Improvements

### 15. **Visual Flow Diagrams**

**Needed:**
- Tracking boundary flow charts showing parent/child independence
- Close propagation sequence diagrams
- Error flow diagrams (dual-path propagation)
- Forwarder task lifecycle diagrams

**Tools:** Mermaid diagrams in markdown

### 16. **Migration Guide**

**Current State:** Deprecated APIs (`pipe()`, `pipe_to()`, `pipe_from()`) exist but no migration guide.

**Needed:**
```markdown
## Migrating from Legacy API

### pipe() → forward()

Before:
```rust
source.pipe().pipe_to(&target);
```

After:
```rust
source.forward(&target).await;
```

### Loopback API → Channel API

Before:
```rust
let relay = Relay::new();
relay.send(value).await?;
```

After:
```rust
let (tx, rx) = Relay::channel();
tx.send(value).await?;
```
```

### 17. **Performance Tuning Guide**

**Topics:**
- Channel size selection (default 65536, when to change?)
- Handler count implications (tracking overhead)
- Subscriber count scaling (N * M problem)
- Backpressure modes and buffer sizing
- Allocation patterns and memory usage

### 18. **Fix README.md Lossless Claims**

**Current State:** README.md claims "lossless delivery" and "backpressure from slow consumers" but implementation uses broadcast semantics with `try_send`.

**Required Changes:**

**In README.md (lines 3, 281-289):**

Before:
```markdown
A typed, heterogeneous event relay library for Rust with lossless delivery...

### Lossless Delivery

pipedream guarantees lossless delivery:
- No message dropping - every message is delivered to all subscribers
- send() is async and authoritative - if it returns Ok, message was delivered
- Backpressure - slow consumers apply backpressure to senders
```

After:
```markdown
A typed, heterogeneous event relay library for Rust with observable delivery...

### Observable Delivery

pipedream provides observable delivery semantics with configurable modes:

**Observable Mode (default)** - High throughput with observable drops:
- Messages delivered to all active subscribers via try_send
- Slow consumers may miss messages if their buffer fills (default: 65536)
- Drops are observable via Dropped events: `rx.subscribe::<Dropped>()`
- No backpressure - senders never block waiting for slow consumers
- Ideal for: logging, metrics, monitoring, high-throughput event streams

**Reliable Mode** - Lossless delivery with backpressure:
- Messages delivered via send().await with blocking
- Slow consumers apply backpressure to senders
- No message drops, guaranteed delivery
- Ideal for: event sourcing, state machines, reliable command buses

Configure via builder:
```rust
// Observable (default)
let (tx, rx) = Relay::builder()
    .delivery_mode(DeliveryMode::Observable)
    .build();

// Reliable
let (tx, rx) = Relay::builder()
    .delivery_mode(DeliveryMode::Reliable)
    .build();
```
```

**In Subscription.rs (lines 14-17):**

Before:
```rust
/// ## Lossless Delivery
/// All subscriptions receive all messages. There is no message dropping.
/// Slow consumers apply backpressure to senders.
```

After:
```rust
/// ## Delivery Semantics
/// Supports two delivery modes configured via builder:
/// - Observable: try_send with buffering (drops observable via Dropped events)
/// - Reliable: send().await with backpressure (lossless delivery)
/// See DeliveryMode for details on choosing the right mode.
```

**In CLAUDE.md (line 9):**

Before:
```markdown
- **Lossless delivery** - if `send().await` returns Ok, message was delivered
```

After:
```markdown
- **Observable delivery** - configurable modes: Observable (high throughput, drops observable) or Reliable (lossless with backpressure)
```

**Why:** The design-doc.md already correctly describes broadcast semantics. README.md should match that reality.

### 19. **Anti-Patterns Documentation**

**Common Mistakes:**

#### 1. **Circular Dependency / Forward-Only Violation (DEADLOCK)**

**Problem:** Tracked handlers that await `send()` on their parent relay create circular dependencies.

**Why it deadlocks:**
- Handler must complete before `send().await` returns
- But handler is waiting for its own `send()` to complete
- Circular wait = deadlock

**❌ WRONG:**
```rust
relay.sink::<Event, _, _>(|event| async move {
    // DEADLOCK: Waiting for self to complete!
    relay.send(DerivedEvent::from(event)).await?;
    Ok(())
});
```

**✅ CORRECT - Use Child Relay:**
```rust
let child = relay.filter::<Event, _>(|_| true);
relay.sink::<Event, _, _>(move |event| async move {
    // OK: Different relay, no circular dependency
    child.send(DerivedEvent::from(event)).await?;
    Ok(())
});
```

**✅ CORRECT - Use WeakSender (Untracked):**
```rust
let weak_tx = tx.weak();
relay.sink::<Event, _, _>(move |event| async move {
    // OK: Untracked send, doesn't participate in completion
    weak_tx.send(DerivedEvent::from(event)).await.ok();
    Ok(())
});
```

**Rule:** **Tracked handlers must only send forward (downstream), never upstream or to self.**

#### 2. **Forgetting to call `complete_one()` in tracked handlers**

Handler never completes, `send().await` hangs forever.

#### 3. **Not calling `clear_tracker()` after completion (double-completion risk)**

Tracker can be completed twice if not cleared.

#### 4. **Using `subscribe()` when `subscribe_tracked()` is needed**

Fire-and-forget semantics when you need completion tracking.

#### 5. **Creating reference cycles with bidirectional forwards**

```rust
relay1.forward(&relay2);
relay2.forward(&relay1);  // Cycle! Use origin field to detect/prevent
```

#### 6. **Not sizing channels for workload in Observable mode**

- Too small: excessive drops
- Too large: memory bloat

Use monitoring (`Dropped` events) to tune buffer size.

## Acceptance Criteria by Tier

### Tier 0: Define Identity (Do Immediately)

**Semantic Contract Fixes:**
- [ ] **Fix README.md** - Lines 3, 281-289: Change "lossless" to "Observable Delivery"
- [ ] **Fix Subscription.rs** - Lines 14-17: Remove "no message dropping" claims
- [ ] **Update CLAUDE.md** - Line 9: Describe Observable Delivery
- [ ] **Add "What Pipedream Is NOT"** - Prevent misuse and support burden

**Correctness Fixes:**
- [ ] **Fix ready signal races** - Change `notify_one` to `notify_waiters` (one-line fix)
- [ ] **Add forwarder panic supervision** - Catch panics, emit `RelayError::HandlerPanic`, close child

**Result:** Clear identity, honest contracts, no semantic debt.

### Tier 1: Documentation & Guidance (Before 1.0)

- [ ] **Expand SEMANTICS.md** - Complete tracking boundary table, circular dependency warnings
- [ ] **Add Anti-patterns section** - Forward-Only rule with deadlock examples
- [ ] **Migration guide** - Legacy API → Channel API
- [ ] **Document WeakSender timing** - Close propagation sequence

**Result:** Users can't shoot themselves in the foot.

### Tier 2: Advanced Features (ONLY After Real Users)

- [ ] **Consider Reliable mode** - If users prove they need lossless delivery
- [ ] **Benchmark suite** - Compare Observable vs Reliable (if implemented)
- [ ] **Performance tuning guide** - Buffer sizing, allocation patterns
- [ ] **Property-based tests** - If complexity increases

**Result:** Solve actual problems, not hypothetical ones.

### Explicitly Deferred (Don't Do)

- [ ] ~~`.wait_for_wiring()` for handler registration~~ - Don't over-promise synchronization
- [ ] ~~Error aggregation~~ - First error wins is fine
- [ ] ~~Type-indexed partitioning~~ - Premature optimization
- [ ] ~~Zero-copy optimization~~ - Solve after measuring
- [ ] ~~Type-safe batching with const generics~~ - Adds rigidity, little benefit

## Success Metrics

**Semantic Consistency:**
- ✅ All documentation matches implementation behavior
- ✅ No silent edge cases in concurrent scenarios
- ✅ Clear contracts between library and users

**API Quality:**
- ✅ Configuration is explicit and discoverable
- ✅ Error messages guide users to correct usage
- ✅ Common patterns are ergonomic
- ✅ Unsafe patterns are hard to write

**Robustness:**
- ✅ No panics escape from internal tasks
- ✅ No resource leaks in complex topologies
- ✅ Graceful degradation under load

**Performance:**
- ✅ Benchmarks show competitive performance vs. alternatives
- ✅ Memory usage scales linearly with load
- ✅ No unexpected contention bottlenecks

## Implementation Details (Tier 0)

### Supervisor Pattern for Forwarders (High Priority)

**Example for map() transformation:**

```rust
// In map() forwarder task
let forwarder = tokio::spawn(async move {
    // Signal ready
    drop(ready_guard);

    loop {
        match parent_sub.recv().await {
            Some(msg) => {
                // Catch panics in transformation
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    transform_fn(&msg)
                }));

                match result {
                    Ok(transformed) => {
                        // Forward successfully
                        if let Err(_) = child_tx.send(transformed).await {
                            break; // Child closed
                        }
                    }
                    Err(panic_payload) => {
                        // Create panic error
                        let panic_error = PanicError::new(panic_payload);
                        let relay_error = RelayError {
                            msg_id: msg.msg_id(),
                            source: "map forwarder",
                            error: Arc::new(panic_error),
                        };

                        // Emit error to parent (observable)
                        let _ = parent_relay.emit_untracked(relay_error).await;

                        // Close child and exit
                        child_relay.close();
                        break;
                    }
                }
            }
            None => {
                // Parent closed
                child_relay.close();
                break;
            }
        }
    }
});
```

**Apply to all transformations:**
- `filter()` - catch panics in predicate
- `filter_map()` - catch panics in transform
- `batch()` - catch panics in batching logic
- `map()` - catch panics in map function

### Ready Signal Barrier Fix

**Current (src/tracker.rs or wherever ReadyGuard lives):**
```rust
impl Drop for ReadyGuard {
    fn drop(&mut self) {
        // ... decrement pending_ready ...
        self.relay.ready_notify.notify_one();  // Single consumer!
    }
}
```

**Fixed:**
```rust
impl Drop for ReadyGuard {
    fn drop(&mut self) {
        // ... decrement pending_ready ...
        self.relay.ready_notify.notify_waiters();  // Multi-consumer barrier!
    }
}
```

**Impact:** All concurrent sends now wait for wiring, not just the first one.

## Implementation Phases (Tiered)

### Tier 0: Define Identity (Do Immediately - Days)

**Goal:** Clear semantic contracts, no lies.

**Tasks:**
1. **Fix documentation (30 min):**
   - README.md lines 3, 281-289 → "Observable Delivery"
   - Subscription.rs lines 14-17 → Remove "no dropping" claims
   - CLAUDE.md line 9 → Describe Observable Delivery
   - Add "What Pipedream IS NOT" section to README

2. **Fix ready signals (5 min):**
   - Change `notify_one()` to `notify_waiters()` in ReadyGuard::drop
   - One-line fix, major correctness improvement

3. **Add forwarder panic supervision (2-3 hours):**
   - Wrap transformation closures in `catch_unwind`
   - Emit `RelayError::HandlerPanic` on panic
   - Close child relay and exit forwarder cleanly
   - Update: `map()`, `filter()`, `filter_map()`, `batch()`
   - Add tests for panic scenarios

**Estimated Effort:** 1 day

**Result:** Honest library with clear identity.

### Tier 1: Documentation & Guidance (Before 1.0 - Weeks)

**Goal:** Users can't misuse pipedream.

**Tasks:**
1. **Expand SEMANTICS.md:**
   - Complete tracking boundary table (all transformations)
   - Add circular dependency section with Forward-Only rule
   - Document panic handling in forwarders

2. **Anti-patterns documentation:**
   - Add to README with Forward-Only rule
   - Deadlock examples with correct patterns
   - Common mistakes and fixes

3. **Migration guide:**
   - Legacy API → Channel API
   - Code examples for each pattern

4. **WeakSender timing docs:**
   - Close propagation sequence
   - Upgrade failure scenarios

**Estimated Effort:** 1-2 weeks

**Result:** Clear guardrails, hard to misuse.

### Tier 2: Advanced Features (ONLY After Real Users - Months)

**Goal:** Solve actual problems, not hypothetical ones.

**Tasks (conditional on real user need):**
1. **Consider Reliable mode (if requested):**
   - Add `DeliveryMode` enum
   - Implement builder pattern
   - `send().await` blocking in Reliable mode
   - Document as advanced/opt-in with warnings

2. **Benchmark suite (if performance questions arise):**
   - Throughput, latency, memory
   - Compare Observable vs alternatives
   - Compare Observable vs Reliable (if implemented)

3. **Performance tuning guide (if needed):**
   - Buffer sizing recommendations
   - Allocation patterns
   - Monitoring with `Dropped` events

4. **Property-based tests (if complexity increases):**
   - Completion tracking invariants
   - Error propagation consistency

**Estimated Effort:** Ongoing

**Result:** Solve real problems as they emerge.

### Explicitly NOT Doing

**Won't implement (premature):**
- ❌ `.wait_for_wiring()` - Over-promises synchronization
- ❌ Error aggregation - First error wins is sufficient
- ❌ Type-indexed partitioning - Solve after measuring
- ❌ Zero-copy optimization - Optimize after profiling
- ❌ Const generic batching - Adds rigidity

**Discipline:** Don't drift into actor-framework territory.

## Technical Considerations

### Breaking Changes

**Potential Breaking Changes:**
- Changing `try_send` to `send().await` for lossless semantics
- Making ready signals multi-consumer (behavior change)
- Removing deprecated APIs (`pipe()`, `pipe_to()`, `pipe_from()`)

**Mitigation:**
- Version as 0.2.0 (semver allows breaking changes in 0.x)
- Provide clear migration guide
- Consider feature flags for backward compatibility

### Performance Impact

**Changes That May Affect Performance:**
- `send().await` instead of `try_send` (adds backpressure)
- Multi-consumer ready signals (more notify overhead)
- Error aggregation (Vec allocation per send)
- Panic catching in forwarders (catch_unwind overhead)

**Validation Required:**
- Benchmark before and after each change
- Ensure no regressions in hot paths
- Document performance characteristics

### Security Considerations

**Areas to Review:**
- Panic safety: Can user code cause internal panics?
- Resource exhaustion: Can unbounded growth occur?
- Denial of service: Can slow subscriber starve system?
- Data races: Are all atomics correctly ordered?

## References & Research

### Internal References

**Key Files:**
- Architecture: `src/stream.rs:1-1521` (main Relay implementation)
- Completion tracking: `src/tracker.rs:1-100`
- Type erasure: `src/envelope.rs:1-50`
- Subscription modes: `src/subscription.rs:1-200`
- Tests: `src/tests.rs:1-3172` (108 tests)
- Design rationale: `design-doc.md`
- Formal semantics: `SEMANTICS.md`
- AI guidance: `CLAUDE.md`

**Design Decisions:**
- Per-subscriber MPSC vs. broadcast: `design-doc.md` section 3
- Tracking boundaries: `SEMANTICS.md` section 2
- Panic handling: `CLAUDE.md` invariants section

### External References

**Relevant Patterns:**
- Actor model completion semantics (Akka, Orleans)
- Rust async error handling patterns (tokio, async-std)
- Broadcast channel implementations (tokio::sync::broadcast, crossbeam)
- Type-erased message passing (actix, bastion)

**Rust Community Best Practices:**
- [The Rustonomicon](https://doc.rust-lang.org/nomicon/) - Unsafe code guidelines
- [Tokio tutorial](https://tokio.rs/tokio/tutorial) - Async patterns
- [async-book](https://rust-lang.github.io/async-book/) - Async Rust

### Related Work

**Similar Libraries:**
- `tokio::sync::broadcast` - Broadcast channel with lagging semantics
- `actix` - Actor framework with typed messages
- `bastion` - Fault-tolerant distributed actors
- `crossbeam-channel` - Multi-producer multi-consumer channels

**Differences:**
- pipedream-rs: Heterogeneous messages, explicit completion, transformations
- tokio::broadcast: Homogeneous, no completion tracking, simple broadcast
- actix: Actor-based, request-response, supervision trees

## Risk Analysis

### High Risk

1. **Lossless semantics change** - Changing from try_send to send().await could break existing code expecting non-blocking behavior
   - Mitigation: Feature flag for backward compatibility, clear migration guide

2. **Forwarder panic handling** - Adding catch_unwind might mask bugs in user code
   - Mitigation: Emit panic errors through relay, enable debug mode with panic propagation

### Medium Risk

3. **Ready signal behavior change** - Multi-consumer ready could add latency
   - Mitigation: Benchmark impact, provide opt-out if needed

4. **Performance regressions** - Multiple changes could compound to slow critical paths
   - Mitigation: Comprehensive benchmarks before/after each change

### Low Risk

5. **Documentation expansion** - More docs = more maintenance
   - Mitigation: Generate diagrams from code where possible

6. **Builder pattern complexity** - More API surface = more to learn
   - Mitigation: Good defaults, progressive disclosure

## Future Considerations

### Extensibility

**Potential Future Features:**
- Custom completion strategies (quorum, majority, etc.)
- Pluggable error handling policies
- Metrics and observability hooks
- Distributed relay (network transparent)
- Priority queues and ordering guarantees

### Long-Term Vision

**What pipedream-rs Could Become:**
- Standard async event bus for Rust applications
- Foundation for actor frameworks
- Basis for distributed systems
- Teaching tool for async Rust patterns

### Maintenance Burden

**Considerations:**
- Test suite is large (108 tests) - CI time concerns
- Multiple API styles (legacy + channel) - documentation burden
- Complex semantics - support burden for edge cases
- Performance sensitive - requires ongoing benchmarking

## Documentation Plan

### Files to Update

**Critical Updates:**
- [ ] `CLAUDE.md` - Fix lossless delivery claim
- [ ] `SEMANTICS.md` - Expand tracking boundaries section
- [ ] `design-doc.md` - Add panic handling, ready signal race documentation
- [ ] `README.md` - Add configuration examples, anti-patterns

**New Documentation:**
- [ ] `docs/migration-guide.md` - Legacy API → Channel API
- [ ] `docs/performance.md` - Tuning guide with benchmarks
- [ ] `docs/architecture.md` - Visual diagrams of tracking, close propagation, error flow
- [ ] `docs/anti-patterns.md` - Common mistakes and how to avoid them

### Examples to Add

**Missing Patterns:**
- Bidirectional pipes with echo prevention
- Graceful shutdown with drain
- Dynamic handler registration
- Error recovery strategies
- Monitoring and observability

## Conclusion

**Overall Assessment:** pipedream-rs is a well-designed, production-quality library with excellent documentation and comprehensive testing. The core architecture is sound and the design decisions are well-reasoned.

**Primary Focus Areas:**
1. **Semantic consistency** - Ensure documentation matches implementation
2. **Panic safety** - Catch panics in all internal tasks
3. **Concurrency edge cases** - Document or fix ready signals and handler registration races
4. **API ergonomics** - Add builder pattern and explicit barriers

**Recommended Next Steps:**
1. Resolve critical issues (lossless semantics, panic handling)
2. Expand semantic documentation
3. Add property-based tests
4. Create benchmark suite
5. Polish documentation with visual diagrams

The library is in great shape for early-stage development. With these improvements, it will be production-ready for 1.0 release.
