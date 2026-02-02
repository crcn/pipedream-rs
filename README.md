# pipedream

A typed, heterogeneous event relay library for Rust with lossless delivery, explicit lifecycle management, and error propagation.

## Features

- **Lossless delivery** - if `send().await` returns Ok, message was delivered to all subscribers
- **Channel-based ownership** - explicit lifecycle with `RelaySender` and `RelayReceiver`
- Single relay carries messages of any type
- Type-based filtering and transformation
- Typed subscriptions that receive only matching types
- Pipeline composition via chaining
- **Explicit completion tracking** - `send().await` waits for tracked handlers
- **Error propagation** from handlers back to senders
- **Panic catching** in handlers with error reporting
- **Backpressure** - slow consumers apply backpressure to senders

## Installation

```toml
[dependencies]
pipedream = "0.1"
tokio = { version = "1", features = ["full"] }
```

## Quick Start

```rust
use pipedream::Relay;

#[tokio::main]
async fn main() {
    // Create a relay channel
    let (tx, rx) = Relay::channel();

    // Subscribe to specific types
    let mut sub = rx.subscribe::<String>();

    // Send any type - awaiting guarantees delivery
    tx.send("hello".to_string()).await.unwrap();
    tx.send(42i32).await.unwrap();

    // Receive only messages matching your subscription type
    let msg = sub.recv().await.unwrap();
    println!("Got: {}", *msg); // "hello"
}
```

## API

### Creating a Relay

```rust
// Create a relay channel with default buffer size (65536)
let (tx, rx) = Relay::channel();

// Custom channel size (affects backpressure threshold)
let (tx, rx) = Relay::channel_with_size(128);

// WeakSender - send without keeping channel alive
let weak_tx = tx.weak();
weak_tx.send("hello").await?;

// Check tracked handler count (for debugging)
println!("handlers: {}", tx.handler_count());

// Check if closed
if tx.is_closed() {
    println!("relay was closed");
}
```

### Sending & Receiving

```rust
// Create channel
let (tx, rx) = Relay::channel();

// Send and wait for tracked handlers to complete
// Returns Ok only if message was delivered to all subscribers
tx.send("hello".to_string()).await?;

// Type-filtered subscription (untracked - send() doesn't wait for these)
let mut sub = rx.subscribe::<String>();
while let Some(msg) = sub.recv().await {
    println!("{}", *msg);
}

// Closing: drop RelaySender to close the channel
drop(tx);
assert!(sub.recv().await.is_none());
```

### Filtering

```rust
let (tx, rx) = Relay::channel();

// Filter by type and predicate
let large = rx.filter::<i32, _>(|n| *n > 100);

// Filter by type only
let strings = rx.only::<String>();

// Exclude a type
let no_errors = rx.exclude::<Error>();

// Split by type
let (strings, rest) = rx.split::<String>();
```

### Transformation

```rust
let (tx, rx) = Relay::channel();

// Map values of one type to another
let lengths = rx.map::<String, usize, _>(|s| s.len());

// Filter and map in one operation
let numbers = rx.filter_map::<String, i32, _>(|s| s.parse().ok());

// Chain transformations
let processed = rx
    .filter::<i32, _>(|n| *n > 0)
    .map::<i32, String, _>(|n| format!("value: {}", n));
```

### Batching

```rust
let (tx, rx) = Relay::channel();

// Collect messages into batches of N
let batched = rx.batch::<Event>(10);

// Process batches
batched.sink::<Vec<Event>, _, _>(|batch| {
    process_batch(batch);  // batch.len() <= 10
});
```

When the relay closes, any remaining buffered messages are emitted as a final (possibly smaller) batch.

### Handlers with Error Propagation

Handlers can return `()` or `Result<(), E>`. Errors propagate back to `send().await`:

```rust
let (tx, rx) = Relay::channel();

// Handler that can fail
rx.sink::<String, _, _>(|msg| {
    if msg.is_empty() {
        return Err(MyError::new("empty message"));
    }
    process(msg);
    Ok(())
});

// Handler without errors (returns ())
rx.sink::<i32, _, _>(|n| {
    println!("got: {}", n);
});

// Errors propagate to sender
match tx.send("".to_string()).await {
    Ok(()) => println!("processed"),
    Err(SendError::Downstream(e)) => println!("handler error: {}", e),
    Err(SendError::Closed) => println!("relay closed"),
}
```

### Observing with tap()

```rust
let (tx, rx) = Relay::channel();

// Tap - observe without consuming (returns receiver for chaining)
rx.tap::<Event, _, _>(|e| {
        log::info!("{:?}", e);
        Ok(())
    })
    .tap::<Metric, _, _>(|m| {
        metrics.record(m);
    });
```

### Subscribing to Errors

Errors from handlers are also sent through the relay:

```rust
use pipedream::RelayError;

let (tx, rx) = Relay::channel();

// Subscribe to errors globally
let mut errors = rx.subscribe::<RelayError>();
tokio::spawn(async move {
    while let Some(err) = errors.recv().await {
        eprintln!("[{}] msg {}: {}", err.source, err.msg_id, err.error);
    }
});
```

### Spawning Tasks with within()

For custom async processing with panic catching:

```rust
let (tx, rx) = Relay::channel();
let mut sub = rx.subscribe::<String>();

rx.within(|| async move {
    while let Some(msg) = sub.recv().await {
        // Panics here are caught and sent to relay as RelayError
        process(&msg);
    }
});
```

Note: `within()` does not participate in completion tracking. Use `sink()` or `tap()` if you need `send().await` to wait for your handler.

## Error Handling

### Error Flow

Errors in handlers flow two ways:

1. **To sender**: `send().await` returns `Err(SendError::Downstream(...))`
2. **Through relay**: Subscribe to `RelayError` for monitoring

```rust
use pipedream::{Relay, RelayError, SendError};

let (tx, rx) = Relay::channel();

// Monitor all errors
let mut error_sub = rx.subscribe::<RelayError>();
tokio::spawn(async move {
    while let Some(err) = error_sub.recv().await {
        metrics.increment("handler_errors");
    }
});

// Handler that may fail
rx.sink::<Request, _, _>(|req| {
    validate(req)?;
    process(req)?;
    Ok(())
});

// Caller gets error
if let Err(SendError::Downstream(e)) = tx.send(bad_request).await {
    // Handle error
}
```

### Panic Catching

Panics in `sink()`, `tap()`, and `within()` are caught and converted to `RelayError`:

```rust
let (tx, rx) = Relay::channel();

rx.sink::<i32, _, _>(|n| {
    if *n == 0 {
        panic!("division by zero");
    }
    println!("{}", 100 / n);
});

// Panic becomes RelayError with source="sink"
```

## Execution Semantics

### Lossless Delivery

pipedream guarantees lossless delivery:

- **No message dropping** - every message is delivered to all subscribers
- `send()` is async and authoritative - if it returns `Ok`, message was delivered
- Per-subscriber MPSC channels ensure no message loss
- **Backpressure** - slow consumers apply backpressure to senders
- Delivery happens **inside** `send()`, not in background tasks

**Non-blocking:** Delivery is fully async - uses `join_all` to deliver to all subscribers concurrently. No threads are blocked. If a subscriber's channel buffer is full, only that specific delivery awaits (backpressure).

### Message Lifecycle

```
tx.send(msg) called
    │
    ▼
┌─────────────────────────────────────┐
│ 1. Check if relay is closed         │
│ 2. Await ready signals              │
│ 3. Snapshot handler_count (N)       │
│ 4. Create tracker expecting N       │
│ 5. Deliver to all subscribers       │  ← Concurrent async delivery (join_all)
│ 6. Wait for tracked handlers        │
└─────────────────────────────────────┘
    │
    ▼ (delivered to all subscribers)
    │
┌───┴───┬───────┬───────┐
▼       ▼       ▼       ▼
Handler Handler Handler Subscription
(sink)  (tap)   (sink)  (untracked)
    │       │       │
    ▼       ▼       ▼
complete complete complete
    │       │       │
    └───────┴───────┘
            │
            ▼
    tracker.is_complete()
            │
            ▼
    send().await returns
```

### Completion Tracking

**What's tracked** (send().await waits for these):

- `sink()` - terminal handler
- `tap()` - pass-through observer

**What's NOT tracked** (fire-and-forget):

- `subscribe()` - raw subscription, for monitoring/logging
- `within()` - spawner for custom async processing
- Child relay handlers - isolated tracking boundary

### Tracking Boundaries

Transformations (`filter`, `map`, `filter_map`, `batch`) create **tracking boundaries**:

```rust
let (tx, rx) = Relay::channel();

let filtered = rx.filter::<i32, _>(|n| *n > 0);
filtered.sink::<i32, _, _>(|n| println!("{}", n));

// send() on parent does NOT wait for filtered sink
// Each relay has its own independent handler_count
tx.send(42).await?;  // Returns after parent's handlers complete
```

Parent and child relays have independent tracking. Errors from child handlers don't propagate to parent.

### Handler Count Semantics

Handler count is **snapshotted at send time**:

```rust
let (tx, rx) = Relay::channel();

// At this point, handler_count = 0

// Now handler_count = 1
rx.sink::<i32, _, _>(|n| println!("{}", n));

// send() snapshots expected = 1, waits for 1 completion
tx.send(42).await?;
```

**Concurrent registration behavior:**

- Handlers registering _during_ `send()` may or may not be counted
- A late-registered handler may receive the message but won't be waited for
- This is intentional "best effort" - not strict membership

**Guarantee:** If all handlers are registered before `send()`, all will be tracked.

### Type Filtering Completion

Handlers complete immediately for messages of non-matching types:

```rust
let (tx, rx) = Relay::channel();

rx.sink::<String, _, _>(|s| println!("{}", s));  // Handler 1
rx.sink::<i32, _, _>(|n| println!("{}", n));     // Handler 2

// expected = 2
// Handler 1 sees String, processes, completes
// Handler 2 sees String (wrong type), completes immediately
tx.send("hello".to_string()).await?; // Returns after both complete
```

### Error Flow

```
Handler error
    │
    ├──► tracker.fail(error)     ──► send().await returns Err
    │
    └──► stream.send_raw(error)  ──► subscribe::<DataStreamError>()
```

Both paths fire. Errors are observable via subscription AND returned to sender.

## Lifecycle

Relays follow automatic cascade close:

```
1. RelaySender dropped/closed
2. Subscriber channels close
3. Child forwarding tasks detect closure
4. Child relays are closed
5. All subscriptions receive None
```

Creating a transformation on a closed relay returns a closed child immediately.

## Backpressure

pipedream uses per-subscriber MPSC channels with configurable buffer size:

- When a subscriber's buffer is full, `send()` will wait
- This creates natural backpressure from slow consumers
- Use `channel_with_size()` to tune buffer size for your workload
- Larger buffers allow more burst capacity but use more memory

```rust
// Higher buffer for bursty workloads (default is 65536)
let (tx, rx) = Relay::channel_with_size(4096);

// Lower buffer for strict backpressure
let (tx, rx) = Relay::channel_with_size(16);
```

## Example: Processing Pipeline with Error Handling

```rust
#[derive(Clone)]
struct RawEvent { data: Vec<u8> }

#[derive(Clone)]
struct Parsed { value: i32 }

#[derive(Clone)]
struct Validated { value: i32 }

let (tx, rx) = Relay::channel();

// Monitor errors
rx.subscribe::<RelayError>().within(|| async move {
    while let Some(e) = sub.recv().await {
        log::error!("Pipeline error: {}", e);
    }
});

// Stage 1: Parse (can fail)
rx.sink::<RawEvent, _, _>({
    let weak_tx = tx.weak();
    move |raw| {
        let parsed = parse(&raw.data)?;
        let _ = weak_tx.send(Parsed { value: parsed });
        Ok(())
    }
});

// Stage 2: Validate
rx.sink::<Parsed, _, _>({
    let weak_tx = tx.weak();
    move |p| {
        if p.value <= 0 {
            return Err(ValidationError::new("must be positive"));
        }
        let _ = weak_tx.send(Validated { value: p.value });
        Ok(())
    }
});

// Stage 3: Consume
rx.sink::<Validated, _, _>(|v| {
    println!("Valid: {}", v.value);
});
```

## Detailed Semantics

For a complete specification of completion tracking, error propagation, and invariants, see [SEMANTICS.md](./SEMANTICS.md).

## License

MIT
