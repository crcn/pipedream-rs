# pipedream-rs Semantics

This document describes the core guarantees and behaviors of pipedream-rs.

## Completion Tracking

### What's Tracked

| Component                        | Tracked | Notes                                |
| -------------------------------- | ------- | ------------------------------------ |
| `sink()`                         | Yes     | Terminal handler, must complete      |
| `tap()`                          | Yes     | Pass-through observer, must complete |
| `subscribe()`                    | No      | Raw subscription, fire-and-forget    |
| `within()`                       | No      | Spawner, does not participate        |
| `map()` child handlers           | No      | Isolated tracking boundary           |
| Forwarders (filter/only/exclude) | Yes     | Complete for filtered-out messages   |

### Handler Count Semantics

Handler count is **snapshotted at send time**:

```rust
let stream = DataStream::new();
stream.sink::<i32, _, _>(|n| println!("{}", n));  // handler_count = 1
stream.send(42).await?;  // expected = 1, waits for 1 completion
```

**Concurrent registration**: Handlers registered _during_ `send()` may or may not be counted. This is intentional "best effort" - not strict membership.

**Guarantee**: If all handlers are registered before `send()`, all will be tracked.

### Type Filtering

Tracked handlers complete immediately for messages of non-matching types:

```rust
stream.sink::<String, _, _>(|s| println!("{}", s));  // Handler 1
stream.sink::<i32, _, _>(|n| println!("{}", n));     // Handler 2

// expected = 2
// Handler 1 sees String, processes, completes
// Handler 2 sees String (wrong type), completes immediately
stream.send("hello".to_string()).await?;
```

## Tracking Boundaries

### map() Isolation

`map()` creates a **tracking boundary**:

```rust
let mapped = stream.map::<i32, String, _>(|n| n.to_string());
mapped.sink::<String, _, _>(|s| println!("{}", s));

// send() on parent does NOT wait for mapped sink
stream.send(42).await?;  // Returns immediately (parent has no handlers)
```

Parent and child streams have independent tracking. The map task is not a tracked handler.

### Rationale

- Parent `send()` returns when parent handlers complete
- Child has its own `send()` lifecycle
- Errors in child propagate to child callers, not parent

## Error Propagation

### Dual-Path Error Flow

Errors from handlers flow two ways:

```
Handler error
    │
    ├──► tracker.fail(error)     ──► send().await returns Err
    │
    └──► stream.send_raw(error)  ──► subscribe::<DataStreamError>()
```

Both paths fire simultaneously.

### Error Types

```rust
pub struct DataStreamError {
    pub msg_id: u64,                              // Correlates to envelope
    pub error: Arc<dyn Error + Send + Sync>,      // The underlying error
    pub source: &'static str,                     // "sink", "tap", "within", etc.
}

pub enum SendError {
    Closed,                      // Stream was dropped
    Downstream(DataStreamError), // Handler failed
}
```

### Panic Handling

Panics in `sink()`, `tap()`, and `within()` are caught and converted to `DataStreamError`:

```rust
stream.sink::<i32, _, _>(|n| {
    if *n == 0 { panic!("division by zero"); }
    println!("{}", 100 / n);
});

// Panic becomes DataStreamError with source="sink"
```

## Subscription Types

### Untracked (subscribe)

```rust
let mut sub = stream.subscribe::<String>();
```

- Does NOT participate in completion tracking
- `send().await` does not wait for these
- Safe for monitoring, logging, side-channel processing

### Tracked (internal)

Used internally by `sink()` and `tap()`:

- Participates in completion tracking
- Signals completion for wrong-type messages
- Fails tracker on Drop (safety net for panics)

## Ready Signals

Forwarders signal readiness before processing. `send()` drains and awaits these signals.

**Note**: Ready signals are single-consumer, not a barrier. If two sends race:

- One waits for wiring
- The other proceeds without waiting

This is acceptable given "best effort" handler count semantics.

## Message Delivery

### Broadcast Semantics

- All subscribers receive the same messages
- Fast producers are not blocked by slow consumers
- Lagged messages are dropped silently

**Do not use for exactly-once delivery** without an additional layer.

### Closed Stream Behavior

```rust
drop(stream);  // Closes stream

// Subsequent operations:
// - send() returns Err(SendError::Closed)
// - subscribe() returns subscription that yields None
// - filter/map/etc. return closed derived streams
```

## Invariants

1. **Every tracked handler must either forward responsibility or discharge it**

   - Process message → `complete_one()`
   - Filter out message → `complete_one()`
   - Error/panic → `fail(error)` (which calls `complete_one()`)

2. **Completion is explicit, never implicit**

   - Handlers call `complete_one()` immediately after processing
   - `clear_tracker()` prevents double-completion

3. **Errors are always surfaced**

   - To sender via `send().await`
   - Through stream via `DataStreamError`

4. **Wiring changes are eventually consistent**
   - Handler count snapshot at send time
   - Ready signals drained on send
   - Not strict real-time membership
