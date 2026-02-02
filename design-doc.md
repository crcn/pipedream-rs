# pipedream-rs Design Document

## Purpose

A typed, heterogeneous data stream library for stream graph composition with automatic lifecycle management.

---

## Requirements

### Core Functionality

1. Single stream carries messages of any type
2. Type-based filtering, exclusion, and splitting
3. Typed subscriptions that receive only matching types
4. Pipeline composition via chaining
5. Send to any stream (root or derived)

### Lifecycle

6. Drop root → all descendants close automatically
7. Subscriptions return `None` when upstream closes
8. No orphaned tasks or leaked resources

### Performance

9. Support thousands of messages per second
10. Minimal allocation overhead per message

### Configuration

11. Buffer size inherited from parent to child streams

---

## Public API

| Operation         | Signature                              | Description                                  |
| ----------------- | -------------------------------------- | -------------------------------------------- |
| Create            | `DataStream::new()`                    | New root stream                              |
| Create            | `DataStream::with_buffer_size(n)`      | Root with custom buffer                      |
| Send              | `stream.send(value)`                   | Send any type to stream                      |
| Subscribe         | `stream.subscribe::<T>()`              | Typed subscription                           |
| Filter            | `stream.filter::<T>(predicate)`        | New stream with matching T                   |
| Exclude           | `stream.exclude::<T>()`                | New stream without T                         |
| Split             | `stream.split::<T>()`                  | Returns (T stream, non-T stream)             |
| Only              | `stream.only::<T>()`                   | New stream with only T (alias for filter)    |
| Tap               | `stream.tap::<T>(f)`                   | Observe T messages, returns stream unchanged |
| Sink              | `stream.sink::<T>(f)`                  | Terminal consumer for T messages             |
| Merge             | `merge([a, b, c])`                     | Combine multiple streams into one            |
| Pipe              | `stream.pipe(f)`                       | Chain transformation                         |
| Pipe To           | `stream.pipe_to(&target)`              | Forward messages to target stream            |
| Pipe To (typed)   | `stream.pipe_to_typed::<T>(&target)`   | Forward only T to target                     |
| Pipe From         | `stream.pipe_from(&source)`            | Receive messages from source stream          |
| Pipe From (typed) | `stream.pipe_from_typed::<T>(&source)` | Receive only T from source                   |
| Receive           | `subscription.recv()`                  | Await next message or None                   |

---

## Architecture

### Stream Hierarchy

```
Root Stream
 ├── Derived Stream A (filter)
 │    └── Derived Stream C (exclude)
 └── Derived Stream B (exclude)
      └── Subscription
```

### Ownership Invariant

**A stream's channel remains alive as long as at least one strong sender reference exists.**

Strong senders may be held by:

- **The root stream itself** (via `tx_owner`), or
- **One or more forwarding tasks** (from `filter`, `exclude`, `pipe_to`, etc.)

One or more strong senders may exist concurrently (e.g., multiple `pipe_to` connections into the same stream), but each must be owned exclusively by either a root stream or a forwarding task. **Derived streams themselves never own senders.**

**Cascade shutdown relies on forwarding tasks exiting when their upstream closes, not on a global single-owner guarantee.**

This invariant must be preserved. Giving derived streams strong references will break cascade shutdown.

### Ownership Model

**Root Stream**

- Holds strong reference to its channel
- Channel closes when root is dropped

**Derived Stream**

- Holds weak reference to its channel
- Forwarding task holds the only strong reference
- Channel closes when forwarding task exits

---

## Lifecycle

### Cascade Close Sequence

```
1. Root dropped
2. Root's channel closes
3. Child's forwarding task sees channel closed, exits
4. Child's channel closes (forwarding task was only owner)
5. Grandchild's forwarding task exits...
6. All subscriptions receive None
```

### Invariants

- Subscription alive ↔ at least one upstream path alive
- Send succeeds ↔ channel still open
- **A stream closes only when all of its upstream forwarding tasks have exited and no root owner exists**
- No message loss under nominal load; lagged messages may be dropped under pressure

---

## Semantic Clarifications

### Send on Derived Streams

**Sending to a derived stream injects messages at that point in the pipeline and does not affect parent streams.**

- Messages sent to a derived stream go only to that stream's subscribers and descendants
- Messages do not propagate upstream
- Messages do not pass through parent filters

Derived streams are independent ingress points, not views of the parent.

### Pipe Intent

`pipe` is intended for **structural composition**, not transformation of individual messages.

Use `pipe` to:

- Wire up subscriptions
- Branch the pipeline
- Attach side-effects
- Return a continuation stream

Do not use `pipe` for per-message transforms like `map<T, U>`.

### Message Loss Policy

**pipedream-rs uses broadcast semantics. Slow consumers may miss messages. Message loss is silent and intentional.**

- Fast producers are not blocked by slow consumers
- Lagged messages are dropped, not queued
- This is a policy decision, not a limitation

Do not build transactional or exactly-once systems on this library without an additional delivery layer.

### Subscription Type Filtering

**Subscriptions perform type filtering locally rather than partitioning channels per type.**

- Channels remain heterogeneous internally
- Each subscription filters by `TypeId` comparison (O(1))
- This minimizes channel fan-out and allocation
- Wrong-type messages are skipped, not errored

### Observable Dropping

**Dropped messages are not silent.**

When a message is dropped due to a full subscriber buffer:

1. A `Dropped` event is injected into the stream (best-effort).
2. Subscribers can listen for these events: `stream.subscribe::<Dropped>()`.
3. This enables monitoring of slow consumers without blocking the producer.

---

## Stream Connections

### pipe_to / pipe_from

These operations connect streams, enabling DAG topologies beyond simple trees.

**`pipe_to(&target)`**: Forward this stream's messages to another stream.

```
stream_a.pipe_to(&stream_b);
// Messages in A now also flow to B
// A still works independently
// B can have multiple inputs
```

**`pipe_from(&source)`**: Receive messages from another stream.

```
stream_b.pipe_from(&stream_a);
// Same as stream_a.pipe_to(&stream_b)
// Different call direction
```

**`pipe_from` exists for readability and symmetry; it does not introduce new semantics.**

### Semantics

| Question                       | Answer                             |
| ------------------------------ | ---------------------------------- |
| Consumes self?                 | No, takes `&self`                  |
| Source closes → target closes? | No, target may have other inputs   |
| Target closes → ?              | Forwarding task exits (send fails) |
| Type filtering?                | Optional: `pipe_to::<T>(&target)`  |

**Close condition for multi-input streams:** A stream with multiple upstream connections (via `pipe_to` or derivation) closes only when _all_ upstream forwarding tasks have exited and no root owner exists. First upstream close does not close the target.

### Topology Examples

**Fan-in (merge):**

```
    A ──────┐
            ▼
    B ──► merge ──► output
            ▲
    C ──────┘
```

```
let merge = DataStream::new();
a.pipe_to(&merge);
b.pipe_to(&merge);
c.pipe_to(&merge);
```

**Fan-out:**

```
              ┌──► B
    A ────────┼──► C
              └──► D
```

```
a.pipe_to(&b);
a.pipe_to(&c);
a.pipe_to(&d);
```

---

## Data Flow

### Message Path

```
send(value) → stream's channel → forwarding task(s) → child channel(s) → subscription(s)
```

### Filter/Exclude/Split

All three operations create a derived stream with a forwarding task that:

1. Receives from parent
2. Applies predicate
3. Forwards matching messages to child

| Operation         | Predicate                                          |
| ----------------- | -------------------------------------------------- |
| filter\<T\>(pred) | Type matches T AND pred(value) is true             |
| exclude\<T\>()    | Type does NOT match T                              |
| split\<T\>()      | Combines filter (T stream) + exclude (rest stream) |

---

## Message Envelope

Messages are wrapped in a type-erased envelope that:

- Stores the value as `Arc<dyn Any>`
- Tracks the original `TypeId`
- Supports downcast to concrete type
- Is cheaply cloneable (Arc)

---

## Patterns

### Tap Pattern

To observe messages without consuming them:

```
stream.tap::<MyEvent, _>(|event| {
    println!("Observed: {:?}", event);
})
```

This spawns a task that receives each message and calls the closure.

### Sink Pattern

To consume messages at a terminal point:

```
stream.sink::<Result, _>(|result| {
    database.save(result);
});
```

Unlike `tap`, `sink` does not return a stream - it's an endpoint.

### Merge Pattern

To merge multiple streams:

```
let merged = merge([stream_a, stream_b, stream_c]);
let mut sub = merged.subscribe::<T>();
```

Messages from all sources are interleaved in arrival order.

---

## Performance

### Per-Message Cost

- One Arc allocation (envelope)
- One broadcast send per stream level
- Type check at subscription (TypeId comparison)

### Per-Derived-Stream Cost

- One spawned task
- One broadcast channel

### Target Performance

- 10,000+ messages/second
- ~1-2μs overhead per message
- At 10k msg/sec: ~1-2% overhead

---

## Error Handling

| Scenario                    | Behavior                                            |
| --------------------------- | --------------------------------------------------- |
| Send to closed stream       | Returns `Err(Closed)`                               |
| Subscribe to closed stream  | Returns subscription; first `recv()` returns `None` |
| Recv on closed subscription | Returns `None`                                      |
| Slow consumer               | Messages dropped silently (broadcast semantics)     |

---

## Design Decisions

### Why return `Subscription<T>` unconditionally from `subscribe()`?

- `recv()` already signals closure cleanly via `None`
- Failing subscription creation adds API complexity for no user benefit
- Weak upgrade failure is equivalent to "already closed" — handle it the same way

### Why no backpressure mode?

Backpressure fundamentally changes:

- Channel choice
- Ownership model
- Task lifetimes
- API expectations

If needed, it should be a separate stream type or feature-gated backend, not bolted onto this design.

### Why no `map<T, U>`?

This library handles **routing**, not **computation**. Keeping these orthogonal:

- Simplifies the ownership model
- Avoids type-level complexity
- Lets users choose their own transform strategy

Transform messages in your subscription handler or upstream of the pipeline.

### Why broadcast instead of mpsc-per-subscriber?

- Simpler ownership (one channel per stream level)
- Natural fan-out to multiple subscribers
- Acceptable trade-off: loss under pressure vs. complexity

### Why local type filtering instead of per-type channels?

- Avoids channel explosion for many types
- O(1) TypeId comparison is cheap
- Heterogeneous channels match the mental model

---

## Future Considerations

### Metrics (passive only)

Expose read-only metrics:

- Subscriber count
- Dropped message count (lagged)
- Active forwarding task count

Do not expose per-message hooks or synchronous instrumentation. Keep hot paths clean.

### Backpressure (separate design)

If required, design as:

- Separate `BoundedDataStream` type
- Or feature-gated alternate channel backend
- Do not mix semantics in the same type

---

## Non-Goals

- Per-message transformation (`map`, `filter_map`)
- Exactly-once delivery
- Transactional semantics
- Backpressure in core API

---

## Summary

| Aspect      | Decision                                                                     |
| ----------- | ---------------------------------------------------------------------------- |
| Ownership   | Weak sender pattern; tasks own channels, streams do not (unless root)        |
| Lifecycle   | Cascade close when all upstream tasks exit; multi-input streams wait for all |
| Semantics   | Broadcast with silent loss under pressure                                    |
| Filtering   | Local at subscription, not channel-level                                     |
| Composition | Structural via `pipe`, not per-message                                       |
| Topology    | Trees by default; DAGs via `pipe_to`/`pipe_from`                             |
