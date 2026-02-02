use futures::FutureExt;
use parking_lot::Mutex;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tracing::{info_span, Instrument};

use crate::envelope::Envelope;
use crate::error::{PanicError, RelayError};
use crate::events::Dropped;
use crate::subscription::Subscription;
use crate::tracker::CompletionTracker;

const DEFAULT_CHANNEL_SIZE: usize = 65536;

/// Global counter for generating unique relay IDs.
static RELAY_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_relay_id() -> u64 {
    RELAY_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ============================================================================
// Inner - shared state for streams
// ============================================================================

pub(crate) struct Inner {
    /// Unique identifier for this stream (used for echo prevention)
    id: u64,
    subscribers: Mutex<Vec<SubscriberSender>>,
    channel_size: usize,
    /// Number of tasks expected to signal ready (incremented when spawning)
    pending_ready: AtomicUsize,
    /// Number of tasks that have signaled ready
    ready_count: AtomicUsize,
    /// Notifies waiters when ready_count changes
    ready_notify: Notify,
    msg_id_counter: AtomicU64,
    handler_count: Arc<AtomicUsize>,
    closed: AtomicBool,
}

impl Inner {
    fn new(channel_size: usize) -> Self {
        Self {
            id: next_relay_id(),
            subscribers: Mutex::new(Vec::new()),
            channel_size,
            pending_ready: AtomicUsize::new(0),
            ready_count: AtomicUsize::new(0),
            ready_notify: Notify::new(),
            msg_id_counter: AtomicU64::new(0),
            handler_count: Arc::new(AtomicUsize::new(0)),
            closed: AtomicBool::new(false),
        }
    }

    /// Wait until all pending tasks have signaled ready.
    async fn wait_ready(&self) {
        loop {
            // Check if already ready
            let pending = self.pending_ready.load(Ordering::SeqCst);
            let ready = self.ready_count.load(Ordering::SeqCst);
            if ready >= pending {
                return;
            }

            // Register for notification BEFORE re-checking (avoids missing notifications)
            let notified = self.ready_notify.notified();

            // Re-check after registering to avoid race
            let pending = self.pending_ready.load(Ordering::SeqCst);
            let ready = self.ready_count.load(Ordering::SeqCst);
            if ready >= pending {
                return;
            }

            notified.await;
        }
    }

    /// Signal that a task is ready.
    fn signal_ready(&self) {
        self.ready_count.fetch_add(1, Ordering::SeqCst);
        // Use notify_one which stores a permit if no waiter is present
        self.ready_notify.notify_one();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Auto-close when the last reference is dropped
        self.closed.store(true, Ordering::SeqCst);
        self.subscribers.lock().clear();
    }
}

/// Guard that ensures ready is signaled even if task panics or is cancelled.
/// Created via `ReadyGuard::new()`, signals on drop if not already signaled.
/// IMPORTANT: After calling `signal()`, the Arc<Inner> is released to allow
/// proper cleanup when the parent stream is dropped.
struct ReadyGuard {
    inner: Option<Arc<Inner>>,
    signaled: bool,
}

impl ReadyGuard {
    /// Register a pending ready and create a guard.
    /// The guard MUST be moved into the spawned task.
    fn new(inner: Arc<Inner>) -> Self {
        inner.pending_ready.fetch_add(1, Ordering::SeqCst);
        Self {
            inner: Some(inner),
            signaled: false,
        }
    }

    /// Signal ready explicitly. Releases the Arc<Inner> reference.
    fn signal(&mut self) {
        if !self.signaled {
            if let Some(inner) = self.inner.take() {
                inner.signal_ready();
            }
            self.signaled = true;
        }
    }
}

impl Drop for ReadyGuard {
    fn drop(&mut self) {
        // Signal on drop if not already signaled (handles panic/cancel)
        self.signal();
    }
}

/// Guard that decrements handler_count on drop.
/// Ensures handler count is always decremented even if task panics or is cancelled.
struct HandlerGuard {
    count: Arc<AtomicUsize>,
}

impl HandlerGuard {
    fn new(count: Arc<AtomicUsize>) -> Self {
        Self { count }
    }
}

impl Drop for HandlerGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

struct SubscriberSender {
    tx: mpsc::Sender<Envelope>,
}

impl SubscriberSender {
    fn new(tx: mpsc::Sender<Envelope>) -> Self {
        Self { tx }
    }

    fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

// ============================================================================
// Readable - output side of a stream
// ============================================================================

/// A readable stream handle - can be subscribed to and piped from.
#[derive(Clone)]
pub struct Readable {
    pub(crate) inner: Arc<Inner>,
}

impl Readable {
    /// Get the stream ID for this readable.
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// Subscribe to events of type T (untracked).
    ///
    /// Untracked subscriptions are fire-and-forget: the sender does NOT wait
    /// for this subscriber to process messages. Use for observers, logging, etc.
    pub fn subscribe<T: 'static + Send + Sync>(&self) -> Subscription<T> {
        let (tx, rx) = mpsc::channel(self.inner.channel_size);
        self.inner
            .subscribers
            .lock()
            .push(SubscriberSender::new(tx));
        Subscription::new(rx)
    }

    /// Subscribe to events of type T (tracked).
    ///
    /// Tracked subscriptions participate in completion tracking:
    /// - `send().await` waits for all tracked subscribers to complete
    /// - Consumer MUST call `tracker.complete_one()` after processing each message
    /// - Forgetting to complete will block the sender indefinitely
    ///
    /// Returns the subscription and a handler_count Arc (decrement when done).
    pub fn subscribe_tracked<T: 'static + Send + Sync>(
        &self,
    ) -> (Subscription<T>, Arc<AtomicUsize>) {
        self.inner.handler_count.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(self.inner.channel_size);
        self.inner
            .subscribers
            .lock()
            .push(SubscriberSender::new(tx));
        (
            Subscription::new_tracked(rx),
            self.inner.handler_count.clone(),
        )
    }

    /// Subscribe to all events regardless of type.
    /// Returns raw envelopes containing type-erased values.
    pub fn subscribe_all(&self) -> mpsc::Receiver<Envelope> {
        let (tx, rx) = mpsc::channel(self.inner.channel_size);
        self.inner
            .subscribers
            .lock()
            .push(SubscriberSender::new(tx));
        rx
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Create a weak reference to this readable.
    /// Used to allowing subscribing without preventing the stream from being dropped.
    pub fn downgrade(&self) -> WeakReadable {
        WeakReadable {
            inner: Arc::downgrade(&self.inner),
        }
    }
}

// ============================================================================
// Writable - input side of a stream
// ============================================================================

/// A writable stream handle - can be sent to and piped to.
#[derive(Clone)]
pub struct Writable {
    pub(crate) inner: Arc<Inner>,
}

impl Writable {
    /// Get the stream ID for this writable.
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    pub async fn send<T: 'static + Send + Sync>(&self, value: T) -> Result<(), SendError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        let msg_id = self.inner.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        let expected = self.inner.handler_count.load(Ordering::SeqCst);
        let tracker = Arc::new(CompletionTracker::new(expected));
        // Stamp with this stream's ID as origin
        let envelope = Envelope::with_origin(value, msg_id, Some(tracker), self.inner.id);

        let span = info_span!(
            "pipedream.send",
            msg_type = %std::any::type_name::<T>(),
            msg_id = %msg_id,
        );
        self.send_envelope(envelope).instrument(span).await
    }

    pub async fn send_envelope(&self, envelope: Envelope) -> Result<(), SendError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        // Wait for all pending tasks to signal ready
        self.inner.wait_ready().await;

        // Stamp origin if not already set (origin == 0 means unset)
        let envelope = if envelope.origin() == 0 {
            envelope.with_new_origin(self.inner.id)
        } else {
            envelope
        };

        let tracker = envelope.tracker();
        self.deliver_envelope(envelope).await?;

        if let Some(tracker) = tracker {
            tracker.clone().wait_owned().await;
            if let Some(error) = tracker.take_error() {
                return Err(SendError::Downstream(error));
            }
        }

        Ok(())
    }

    pub async fn send_any(
        &self,
        value: Arc<dyn std::any::Any + Send + Sync>,
        type_id: std::any::TypeId,
    ) -> Result<(), SendError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        let msg_id = self.inner.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        let expected = self.inner.handler_count.load(Ordering::SeqCst);
        let tracker = Arc::new(CompletionTracker::new(expected));
        // Stamp with this stream's ID as origin
        let envelope =
            Envelope::from_any_with_origin(value, type_id, msg_id, Some(tracker), self.inner.id);

        self.send_envelope(envelope).await
    }

    /// Send a type-erased value with a specific origin (for echo prevention).
    ///
    /// Use this when forwarding events from external sources where you want
    /// to preserve the original origin for echo prevention in pipes.
    pub async fn send_any_with_origin(
        &self,
        value: Arc<dyn std::any::Any + Send + Sync>,
        type_id: std::any::TypeId,
        origin: u64,
    ) -> Result<(), SendError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        let msg_id = self.inner.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        let expected = self.inner.handler_count.load(Ordering::SeqCst);
        let tracker = Arc::new(CompletionTracker::new(expected));
        let envelope =
            Envelope::from_any_with_origin(value, type_id, msg_id, Some(tracker), origin);

        self.send_envelope(envelope).await
    }

    async fn deliver_envelope(&self, envelope: Envelope) -> Result<(), SendError> {
        let subs: Vec<_> = {
            let mut subs = self.inner.subscribers.lock();
            subs.retain(|s| !s.is_closed());
            subs.iter().map(|s| s.tx.clone()).collect()
        };

        let mut dropped = false;

        for tx in subs {
            match tx.try_send(envelope.clone()) {
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    dropped = true;
                    // tracing::warn!(msg_id = %envelope.msg_id(), "message dropped due to slow consumer");
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Channel closed, will be cleaned up on next iteration
                }
            }
        }

        // Emit Dropped event if any subscriber dropped the message
        // Prevent infinite recursion: don't emit Dropped for Dropped events
        if dropped && !envelope.is::<Dropped>() {
            let dropped_event = Dropped {
                msg_id: envelope.msg_id(),
                origin_stream_id: envelope.origin(),
                // Use a placeholder name if we can't easily get the type name here without more plumbing
                // but envelope.type_id is available. For now, let's just say "message".
                // Ideally we'd store type name in envelope but that adds overhead.
                event_type_name: "message",
            };

            // Create envelope for Dropped event (no tracker, new msg_id)
            let msg_id = self.inner.msg_id_counter.fetch_add(1, Ordering::Relaxed);
            let drop_envelope = Envelope::with_origin(dropped_event, msg_id, None, self.inner.id);

            // Best effort delivery for dropped event - recursive call
            // We await it (it's non-blocking now)
            let _ = Box::pin(self.deliver_envelope(drop_envelope)).await;
        }

        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Close the stream immediately, signaling all subscribers.
    /// This is pub(crate) - external users should rely on RAII (drop all references).
    pub(crate) fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.subscribers.lock().clear();
    }

    /// Create a weak reference to this writable.
    /// Used by forwarders to avoid preventing the stream from being dropped.
    pub fn downgrade(&self) -> WeakWritable {
        WeakWritable {
            inner: Arc::downgrade(&self.inner),
        }
    }
}

/// A weak reference to a writable stream.
/// Does not prevent the stream from being dropped.
#[derive(Clone)]
pub struct WeakWritable {
    inner: Weak<Inner>,
}

impl WeakWritable {
    /// Try to upgrade to a strong reference.
    /// Returns None if the stream has been dropped.
    pub fn upgrade(&self) -> Option<Writable> {
        self.inner.upgrade().map(|inner| Writable { inner })
    }
}

// ============================================================================
// WeakReadable - weak reference to output side
// ============================================================================

/// A weak reference to a readable stream.
/// Does not prevent the stream from being dropped.
///
/// Can be used to subscribe to the stream. If the stream is dropped,
/// new subscriptions will be immediately closed.
#[derive(Clone)]
pub struct WeakReadable {
    inner: Weak<Inner>,
}

impl WeakReadable {
    /// Try to upgrade to a strong reference.
    /// Returns None if the stream has been dropped.
    pub fn upgrade(&self) -> Option<Readable> {
        self.inner.upgrade().map(|inner| Readable { inner })
    }

    /// Subscribe to events of type T (untracked).
    ///
    /// If the stream is alive, returns a functional subscription.
    /// If the stream is dropped, returns an immediately closed subscription.
    pub fn subscribe<T: 'static + Send + Sync>(&self) -> Subscription<T> {
        if let Some(inner) = self.inner.upgrade() {
            let (tx, rx) = mpsc::channel(inner.channel_size);
            inner.subscribers.lock().push(SubscriberSender::new(tx));
            Subscription::new(rx)
        } else {
            // Stream is dead, return closed subscription
            let (_, rx) = mpsc::channel(1);
            Subscription::new(rx)
        }
    }

    /// Subscribe to events of type T (tracked).
    /// See `Readable::subscribe_tracked`.
    pub fn subscribe_tracked<T: 'static + Send + Sync>(
        &self,
    ) -> (Subscription<T>, Option<Arc<AtomicUsize>>) {
        if let Some(inner) = self.inner.upgrade() {
            inner.handler_count.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel(inner.channel_size);
            inner.subscribers.lock().push(SubscriberSender::new(tx));
            (
                Subscription::new_tracked(rx),
                Some(inner.handler_count.clone()),
            )
        } else {
            // Stream is dead
            let (_, rx) = mpsc::channel(1);
            (Subscription::new_tracked(rx), None)
        }
    }

    /// Subscribe to all events regardless of type.
    pub fn subscribe_all(&self) -> mpsc::Receiver<Envelope> {
        if let Some(inner) = self.inner.upgrade() {
            let (tx, rx) = mpsc::channel(inner.channel_size);
            inner.subscribers.lock().push(SubscriberSender::new(tx));
            rx
        } else {
            let (_, rx) = mpsc::channel(1);
            rx
        }
    }
}

// ============================================================================
// Stream - the main stream type (loopback: readable/writable share inner)
// ============================================================================

/// A typed, heterogeneous relay with lossless delivery and completion tracking.
#[derive(Clone)]
pub struct Relay {
    readable: Readable,
    writable: Writable,
}

impl Relay {
    // ========================================================================
    // Public API: Channel-based constructors with explicit ownership
    // ========================================================================

    /// Create a new relay channel with explicit ownership semantics.
    ///
    /// Returns `(RelaySender, RelayReceiver)` where:
    /// - `RelaySender` is the sole owner (not Clone) - dropping it closes the channel
    /// - `RelayReceiver` can be cloned for multiple subscribers
    /// - Use `tx.weak()` to get a `WeakSender` for sending without ownership
    ///
    /// This is the recommended way to create relays as it enforces clear lifecycle ownership.
    pub fn channel() -> (RelaySender, RelayReceiver) {
        Self::channel_with_size(DEFAULT_CHANNEL_SIZE)
    }

    /// Create a relay channel with custom buffer size.
    pub fn channel_with_size(channel_size: usize) -> (RelaySender, RelayReceiver) {
        let inner = Arc::new(Inner::new(channel_size));
        (
            RelaySender {
                inner: inner.clone(),
            },
            RelayReceiver { inner },
        )
    }

    // ========================================================================
    // Internal constructors - for pipedream internals and tests only
    // ========================================================================

    /// Internal: Create a loopback relay with shared ownership.
    /// Use `Relay::channel()` for new code.
    #[doc(hidden)]
    pub fn new() -> Self {
        Self::with_channel_size(DEFAULT_CHANNEL_SIZE)
    }

    /// Internal: Create a loopback relay with custom channel size.
    #[doc(hidden)]
    pub fn with_channel_size(channel_size: usize) -> Self {
        let inner = Arc::new(Inner::new(channel_size));
        Self {
            readable: Readable {
                inner: inner.clone(),
            },
            writable: Writable { inner },
        }
    }

    /// Create a relay from separate readable and writable components.
    /// This allows combining relays from different sources.
    pub fn from_parts(readable: Readable, writable: Writable) -> Self {
        Self { readable, writable }
    }

    /// Get parts needed for seesaw bridge().
    ///
    /// Returns `(WeakSender, RelayReceiver)` which can be passed to `bridge()`.
    /// This is a convenience method for transitioning code to the channel API.
    pub fn bridge_parts(&self) -> (WeakSender, RelayReceiver) {
        (
            WeakSender {
                inner: Arc::downgrade(&self.writable.inner),
            },
            RelayReceiver {
                inner: self.readable.inner.clone(),
            },
        )
    }

    fn inner(&self) -> &Arc<Inner> {
        &self.readable.inner
    }

    // ========================================================================
    // Basic operations
    // ========================================================================

    pub async fn send<T: 'static + Send + Sync>(&self, value: T) -> Result<(), SendError> {
        self.writable.send(value).await
    }

    pub async fn send_any(
        &self,
        value: Arc<dyn std::any::Any + Send + Sync>,
        type_id: std::any::TypeId,
    ) -> Result<(), SendError> {
        self.writable.send_any(value, type_id).await
    }

    /// Send a type-erased value with a specific origin (for echo prevention in pipes).
    pub async fn send_any_with_origin(
        &self,
        value: Arc<dyn std::any::Any + Send + Sync>,
        type_id: std::any::TypeId,
        origin: u64,
    ) -> Result<(), SendError> {
        self.writable
            .send_any_with_origin(value, type_id, origin)
            .await
    }

    pub async fn send_envelope(&self, envelope: Envelope) -> Result<(), SendError> {
        self.writable.send_envelope(envelope).await
    }

    /// Emit an event without completion tracking or readiness waiting.
    ///
    /// This is a best-effort, fire-and-forget emit for internal error propagation.
    /// It bypasses:
    /// - Readiness waiting (doesn't wait for forwarders to be ready)
    /// - Completion tracking (sender doesn't wait for handlers)
    /// - Error propagation (errors from handlers are not returned)
    ///
    /// Use `send()` for normal event delivery with full guarantees.
    pub(crate) async fn emit_untracked<T: 'static + Send + Sync + Clone>(&self, value: T) {
        if self.inner().closed.load(Ordering::SeqCst) {
            return;
        }
        let msg_id = self.inner().msg_id_counter.fetch_add(1, Ordering::Relaxed);
        // Stamp with this stream's ID as origin
        let envelope = Envelope::with_origin(value, msg_id, None, self.inner().id);
        let _ = self.writable.deliver_envelope(envelope).await;
    }

    pub fn subscribe<T: 'static + Send + Sync>(&self) -> Subscription<T> {
        self.readable.subscribe()
    }

    /// Subscribe to events of type T (tracked). See `Readable::subscribe_tracked`.
    pub fn subscribe_tracked<T: 'static + Send + Sync>(
        &self,
    ) -> (Subscription<T>, Arc<AtomicUsize>) {
        self.readable.subscribe_tracked()
    }

    pub fn as_readable(&self) -> &Readable {
        &self.readable
    }

    pub fn as_writable(&self) -> &Writable {
        &self.writable
    }

    pub fn is_closed(&self) -> bool {
        self.writable.is_closed()
    }

    /// Close the relay immediately, signaling all subscribers.
    /// This is pub(crate) - external users should rely on RAII (drop all references).
    pub(crate) fn close(&self) {
        self.writable.close()
    }

    pub fn handler_count(&self) -> usize {
        self.inner().handler_count.load(Ordering::SeqCst)
    }

    /// Get the unique ID for this relay.
    pub fn id(&self) -> u64 {
        self.inner().id
    }

    // ========================================================================
    // Chaining helper
    // ========================================================================

    /// Apply a function to this relay and return the result.
    /// Useful for setting up complex pipelines.
    pub fn apply<F, R>(self, f: F) -> R
    where
        F: FnOnce(Self) -> R,
    {
        f(self)
    }

    // ========================================================================
    // Filtering & Transformation
    // ========================================================================

    pub fn filter<T, F>(&self, predicate: F) -> Relay
    where
        T: 'static + Send + Sync + Clone,
        F: Fn(&T) -> bool + Send + Sync + 'static,
    {
        let channel_size = self.inner().channel_size;
        let child = Relay::with_channel_size(channel_size);

        if self.inner().closed.load(Ordering::SeqCst) {
            child.close();
            return child;
        }

        let mut sub = self.subscribe::<T>();
        let mut ready_guard = ReadyGuard::new(self.inner().clone());

        let child_clone = child.clone();
        tokio::spawn(async move {
            ready_guard.signal();
            while let Some(msg) = sub.recv().await {
                if predicate(&*msg) {
                    let _ = child_clone.send((*msg).clone()).await;
                }
            }
            child_clone.close();
        });

        child
    }

    pub fn map<T, U, F>(&self, f: F) -> Relay
    where
        T: 'static + Send + Sync,
        U: 'static + Send + Sync,
        F: Fn(&T) -> U + Send + Sync + 'static,
    {
        let channel_size = self.inner().channel_size;
        let child = Relay::with_channel_size(channel_size);

        if self.inner().closed.load(Ordering::SeqCst) {
            child.close();
            return child;
        }

        let mut sub = self.subscribe::<T>();
        let mut ready_guard = ReadyGuard::new(self.inner().clone());

        let child_clone = child.clone();
        tokio::spawn(async move {
            ready_guard.signal();
            while let Some(msg) = sub.recv().await {
                let _ = child_clone.send(f(&*msg)).await;
            }
            child_clone.close();
        });

        child
    }

    pub fn filter_map<T, U, F>(&self, f: F) -> Relay
    where
        T: 'static + Send + Sync,
        U: 'static + Send + Sync,
        F: Fn(&T) -> Option<U> + Send + Sync + 'static,
    {
        let channel_size = self.inner().channel_size;
        let child = Relay::with_channel_size(channel_size);

        if self.inner().closed.load(Ordering::SeqCst) {
            child.close();
            return child;
        }

        let mut sub = self.subscribe::<T>();
        let mut ready_guard = ReadyGuard::new(self.inner().clone());

        let child_clone = child.clone();
        tokio::spawn(async move {
            ready_guard.signal();
            while let Some(msg) = sub.recv().await {
                if let Some(mapped) = f(&*msg) {
                    let _ = child_clone.send(mapped).await;
                }
            }
            child_clone.close();
        });

        child
    }

    pub fn only<T: 'static + Send + Sync + Clone>(&self) -> Relay {
        self.filter::<T, _>(|_| true)
    }

    pub fn exclude<T: 'static + Send + Sync>(&self) -> Relay {
        let channel_size = self.inner().channel_size;
        let child = Relay::with_channel_size(channel_size);

        if self.inner().closed.load(Ordering::SeqCst) {
            child.close();
            return child;
        }

        let (tx, mut rx) = mpsc::channel::<Envelope>(channel_size);
        self.inner()
            .subscribers
            .lock()
            .push(SubscriberSender::new(tx));

        let mut ready_guard = ReadyGuard::new(self.inner().clone());

        let child_clone = child.clone();
        let exclude_type = std::any::TypeId::of::<T>();

        tokio::spawn(async move {
            ready_guard.signal();
            while let Some(env) = rx.recv().await {
                if env.type_id() != exclude_type {
                    let _ = child_clone.writable.deliver_envelope(env).await;
                }
            }
            child_clone.close();
        });

        child
    }

    pub fn split<T: 'static + Send + Sync + Clone>(&self) -> (Relay, Relay) {
        (self.only::<T>(), self.exclude::<T>())
    }

    pub fn batch<T>(&self, size: usize) -> Relay
    where
        T: 'static + Send + Sync + Clone,
    {
        assert!(size > 0, "batch size must be > 0");

        let channel_size = self.inner().channel_size;
        let child = Relay::with_channel_size(channel_size);

        if self.inner().closed.load(Ordering::SeqCst) {
            child.close();
            return child;
        }

        let mut sub = self.subscribe::<T>();
        let mut ready_guard = ReadyGuard::new(self.inner().clone());

        let child_clone = child.clone();
        tokio::spawn(async move {
            ready_guard.signal();
            let mut buffer: Vec<T> = Vec::with_capacity(size);

            while let Some(msg) = sub.recv().await {
                buffer.push((*msg).clone());
                if buffer.len() >= size {
                    let batch = std::mem::replace(&mut buffer, Vec::with_capacity(size));
                    let _ = child_clone.send(batch).await;
                }
            }

            if !buffer.is_empty() {
                let _ = child_clone.send(buffer).await;
            }
            child_clone.close();
        });

        child
    }

    // ========================================================================
    // Handlers (sink/tap)
    // ========================================================================

    pub fn tap<T, F, R>(&self, f: F) -> Relay
    where
        T: 'static + Send + Sync,
        F: Fn(&T) -> R + Send + Sync + 'static,
        R: IntoResult + 'static,
    {
        let (mut sub, handler_count) = self.subscribe_tracked::<T>();
        let relay = self.clone();
        let msg_type = std::any::type_name::<T>();

        // Guard ensures handler_count is decremented even on panic/cancel
        let _handler_guard = HandlerGuard::new(handler_count);

        tokio::spawn(async move {
            let _guard = _handler_guard; // Move into task
            while let Some(msg) = sub.recv().await {
                let tracker = sub.current_tracker();
                let msg_id = sub.current_msg_id().unwrap_or(0);

                let span = info_span!("pipedream.tap", msg_type = %msg_type, msg_id = %msg_id);
                let _span_guard = span.enter();

                let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&*msg).into_result()));

                match result {
                    Ok(Ok(())) => {
                        if let Some(t) = tracker {
                            t.complete_one();
                        }
                        sub.clear_tracker();
                    }
                    Ok(Err(e)) => {
                        let error = RelayError::new(msg_id, e, "tap");
                        relay.emit_untracked(error.clone()).await;
                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                    Err(panic_info) => {
                        let error = RelayError::new(msg_id, PanicError::new(panic_info), "tap");
                        relay.emit_untracked(error.clone()).await;
                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                }
            }
            // HandlerGuard's Drop decrements handler_count
        });

        self.clone()
    }

    pub fn sink<T, F, R>(&self, f: F)
    where
        T: 'static + Send + Sync,
        F: Fn(&T) -> R + Send + Sync + 'static,
        R: IntoResult + 'static,
    {
        let (mut sub, handler_count) = self.subscribe_tracked::<T>();
        let relay = self.clone();
        let msg_type = std::any::type_name::<T>();

        // Guard ensures handler_count is decremented even on panic/cancel
        let _handler_guard = HandlerGuard::new(handler_count);

        tokio::spawn(async move {
            let _guard = _handler_guard; // Move into task
            while let Some(msg) = sub.recv().await {
                let tracker = sub.current_tracker();
                let msg_id = sub.current_msg_id().unwrap_or(0);

                let span = info_span!("pipedream.sink", msg_type = %msg_type, msg_id = %msg_id);
                let _span_guard = span.enter();

                let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&*msg).into_result()));

                match result {
                    Ok(Ok(())) => {
                        if let Some(t) = tracker {
                            t.complete_one();
                        }
                        sub.clear_tracker();
                    }
                    Ok(Err(e)) => {
                        let error = RelayError::new(msg_id, e, "sink");
                        relay.emit_untracked(error.clone()).await;
                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                    Err(panic_info) => {
                        let error = RelayError::new(msg_id, PanicError::new(panic_info), "sink");
                        relay.emit_untracked(error.clone()).await;
                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                }
            }
            // HandlerGuard's Drop decrements handler_count
        });
    }

    pub fn within<F, Fut>(&self, f: F) -> JoinHandle<()>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let relay = self.clone();
        tokio::spawn(async move {
            let result = AssertUnwindSafe(f()).catch_unwind().await;
            if let Err(panic_info) = result {
                let error = RelayError::new(0, PanicError::new(panic_info), "within");
                relay.emit_untracked(error).await;
            }
        })
    }

    // ========================================================================
    // Piping (forward from one stream to another)
    // ========================================================================

    /// Pipe events from this relay to the target relay.
    ///
    /// **Deprecated:** Use `forward()` instead, which has the same semantics.
    #[deprecated(since = "0.2.0", note = "Use forward() instead")]
    pub async fn pipe(&self, target: &Relay) {
        self.forward(target).await
    }

    // Backwards compatibility aliases
    #[doc(hidden)]
    #[deprecated(since = "0.2.0", note = "Use forward() instead")]
    pub async fn pipe_to(&self, target: &Relay) {
        self.forward(target).await
    }

    #[doc(hidden)]
    #[deprecated(since = "0.2.0", note = "Use forward() instead")]
    pub async fn pipe_from(&self, source: &Relay) {
        source.forward(self).await
    }
}

impl Default for Relay {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Pipe Traits
// ============================================================================

pub trait PipeSource {
    fn readable(&self) -> &Readable;
}

pub trait PipeSink {
    fn writable(&self) -> &Writable;
}

pub trait PipeExt: PipeSource {
    /// Forward events from this source to the target sink.
    ///
    /// Echo prevention: Messages that originated from the target
    /// are not forwarded back to prevent infinite loops.
    ///
    /// This method is async and blocks until the source stream is closed.
    /// It holds a weak reference to the target, but since the future borrows
    /// the target, it effectively keeps the target alive while forwarding.
    async fn forward<S: PipeSink>(&self, target: &S) {
        let src = self.readable();
        let dst_weak = target.writable().downgrade();
        let target_id = target.writable().id();

        let (tx, rx) = mpsc::channel::<Envelope>(src.inner.channel_size);
        src.inner.subscribers.lock().push(SubscriberSender::new(tx));

        forward_to(rx, dst_weak, target_id).await;
    }
}

impl<T: PipeSource + Sync> PipeExt for T {}

async fn forward_to(mut rx: mpsc::Receiver<Envelope>, target: WeakWritable, target_id: u64) {
    while let Some(env) = rx.recv().await {
        // Try to upgrade weak reference - if target was dropped, exit
        let Some(target) = target.upgrade() else {
            break;
        };
        if target.is_closed() {
            break;
        }
        // Echo prevention: don't forward messages back to their origin
        if env.origin() == target_id {
            continue;
        }
        // Strip tracker when forwarding - target stream has its own handlers.
        // Preserving the tracker would cause double-completion bugs when
        // the target has different number of handlers than the source.
        let env = env.without_tracker();
        let _ = target.deliver_envelope(env).await;
    }
    // Note: We intentionally do NOT close the target when source closes.
    // The target is an independent stream that may have other sources.
    // Derived streams (filter, map, etc.) handle closure propagation themselves.
}

impl PipeSource for Readable {
    fn readable(&self) -> &Readable {
        self
    }
}

impl PipeSource for Relay {
    fn readable(&self) -> &Readable {
        &self.readable
    }
}

impl PipeSink for Writable {
    fn writable(&self) -> &Writable {
        self
    }
}

impl PipeSink for Relay {
    fn writable(&self) -> &Writable {
        &self.writable
    }
}

// ============================================================================
// SendError
// ============================================================================

#[derive(Debug, Clone)]
pub enum SendError {
    Closed,
    Downstream(RelayError),
}

impl PartialEq for SendError {
    fn eq(&self, other: &Self) -> bool {
        matches!((self, other), (SendError::Closed, SendError::Closed))
    }
}

impl Eq for SendError {}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Closed => write!(f, "stream closed"),
            SendError::Downstream(e) => write!(f, "downstream error: {}", e),
        }
    }
}

impl std::error::Error for SendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SendError::Closed => None,
            SendError::Downstream(e) => Some(e),
        }
    }
}

// ============================================================================
// Helper Traits
// ============================================================================

pub trait IntoResult {
    type Error: std::error::Error + Send + Sync + 'static;
    fn into_result(self) -> Result<(), Self::Error>;
}

impl IntoResult for () {
    type Error = std::convert::Infallible;
    fn into_result(self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<E: std::error::Error + Send + Sync + 'static> IntoResult for Result<(), E> {
    type Error = E;
    fn into_result(self) -> Result<(), E> {
        self
    }
}

// ============================================================================
// Merge
// ============================================================================

pub async fn merge<I>(relays: I) -> Relay
where
    I: IntoIterator<Item = Relay>,
{
    let output = Relay::new();
    for relay in relays {
        // Spawn forward as background task since forward blocks until source closes
        let output = output.clone();
        tokio::spawn(async move {
            relay.forward(&output).await;
        });
    }
    // Give forward tasks time to set up subscriptions
    tokio::task::yield_now().await;
    output
}

// ============================================================================
// RelaySender - Single Owner
// ============================================================================

/// The single owner of a relay channel.
///
/// This type is intentionally **not** `Clone`. There is exactly one owner
/// of a channel, and when this owner is dropped, the channel closes.
///
/// Use `weak()` to create `WeakSender` handles that can send messages
/// without keeping the channel alive.
pub struct RelaySender {
    inner: Arc<Inner>,
}

impl RelaySender {
    /// Send a typed message to all subscribers.
    pub async fn send<T: 'static + Send + Sync>(&self, value: T) -> Result<(), SendError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        let msg_id = self.inner.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        let expected = self.inner.handler_count.load(Ordering::SeqCst);
        let tracker = Arc::new(CompletionTracker::new(expected));
        let envelope = Envelope::with_origin(value, msg_id, Some(tracker), self.inner.id);

        self.send_envelope(envelope).await
    }

    /// Send a type-erased message.
    pub async fn send_any(
        &self,
        value: Arc<dyn std::any::Any + Send + Sync>,
        type_id: std::any::TypeId,
    ) -> Result<(), SendError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        let msg_id = self.inner.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        let expected = self.inner.handler_count.load(Ordering::SeqCst);
        let tracker = Arc::new(CompletionTracker::new(expected));
        let envelope =
            Envelope::from_any_with_origin(value, type_id, msg_id, Some(tracker), self.inner.id);

        self.send_envelope(envelope).await
    }

    /// Send a type-erased message with a specific origin (for echo prevention).
    pub async fn send_any_with_origin(
        &self,
        value: Arc<dyn std::any::Any + Send + Sync>,
        type_id: std::any::TypeId,
        origin: u64,
    ) -> Result<(), SendError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        let msg_id = self.inner.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        let expected = self.inner.handler_count.load(Ordering::SeqCst);
        let tracker = Arc::new(CompletionTracker::new(expected));
        let envelope =
            Envelope::from_any_with_origin(value, type_id, msg_id, Some(tracker), origin);

        self.send_envelope(envelope).await
    }

    async fn send_envelope(&self, envelope: Envelope) -> Result<(), SendError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        self.inner.wait_ready().await;

        let envelope = if envelope.origin() == 0 {
            envelope.with_new_origin(self.inner.id)
        } else {
            envelope
        };

        let tracker = envelope.tracker();
        self.deliver_envelope(envelope).await?;

        if let Some(tracker) = tracker {
            tracker.clone().wait_owned().await;
            if let Some(error) = tracker.take_error() {
                return Err(SendError::Downstream(error));
            }
        }

        Ok(())
    }

    async fn deliver_envelope(&self, envelope: Envelope) -> Result<(), SendError> {
        let subs: Vec<_> = {
            let mut subs = self.inner.subscribers.lock();
            subs.retain(|s| !s.is_closed());
            subs.iter().map(|s| s.tx.clone()).collect()
        };

        for tx in subs {
            match tx.try_send(envelope.clone()) {
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // Backpressure - slow consumer
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Channel closed, will be cleaned up on next iteration
                }
            }
        }

        Ok(())
    }

    /// Create a weak sender handle.
    ///
    /// Weak senders can send messages but don't keep the channel alive.
    /// Use this for background tasks that should exit when the owner drops.
    pub fn weak(&self) -> WeakSender {
        WeakSender {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Check if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Get the unique ID of this channel.
    pub fn id(&self) -> u64 {
        self.inner.id
    }
}

impl Drop for RelaySender {
    fn drop(&mut self) {
        // Close the channel when the owner is dropped
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.subscribers.lock().clear();
    }
}

// RelaySender is intentionally NOT Clone

// ============================================================================
// WeakSender - Non-owning Send Capability
// ============================================================================

/// A weak sender handle that can send messages without keeping the channel alive.
///
/// Created via `RelaySender::weak()`. If the `RelaySender` has been dropped,
/// send operations will return `Err(SendError::Closed)`.
#[derive(Clone)]
pub struct WeakSender {
    inner: Weak<Inner>,
}

impl WeakSender {
    /// Send a typed message if the channel is still open.
    pub async fn send<T: 'static + Send + Sync>(&self, value: T) -> Result<(), SendError> {
        let inner = self.inner.upgrade().ok_or(SendError::Closed)?;

        if inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        let msg_id = inner.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        let expected = inner.handler_count.load(Ordering::SeqCst);
        let tracker = Arc::new(CompletionTracker::new(expected));
        let envelope = Envelope::with_origin(value, msg_id, Some(tracker), inner.id);

        self.send_envelope_inner(&inner, envelope).await
    }

    /// Send a type-erased message if the channel is still open.
    pub async fn send_any(
        &self,
        value: Arc<dyn std::any::Any + Send + Sync>,
        type_id: std::any::TypeId,
    ) -> Result<(), SendError> {
        let inner = self.inner.upgrade().ok_or(SendError::Closed)?;

        if inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        let msg_id = inner.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        let expected = inner.handler_count.load(Ordering::SeqCst);
        let tracker = Arc::new(CompletionTracker::new(expected));
        let envelope =
            Envelope::from_any_with_origin(value, type_id, msg_id, Some(tracker), inner.id);

        self.send_envelope_inner(&inner, envelope).await
    }

    /// Send a type-erased message with a specific origin.
    pub async fn send_any_with_origin(
        &self,
        value: Arc<dyn std::any::Any + Send + Sync>,
        type_id: std::any::TypeId,
        origin: u64,
    ) -> Result<(), SendError> {
        let inner = self.inner.upgrade().ok_or(SendError::Closed)?;

        if inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        let msg_id = inner.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        let expected = inner.handler_count.load(Ordering::SeqCst);
        let tracker = Arc::new(CompletionTracker::new(expected));
        let envelope =
            Envelope::from_any_with_origin(value, type_id, msg_id, Some(tracker), origin);

        self.send_envelope_inner(&inner, envelope).await
    }

    async fn send_envelope_inner(
        &self,
        inner: &Arc<Inner>,
        envelope: Envelope,
    ) -> Result<(), SendError> {
        if inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }

        inner.wait_ready().await;

        let envelope = if envelope.origin() == 0 {
            envelope.with_new_origin(inner.id)
        } else {
            envelope
        };

        let tracker = envelope.tracker();

        // Deliver
        let subs: Vec<_> = {
            let mut subs = inner.subscribers.lock();
            subs.retain(|s| !s.is_closed());
            subs.iter().map(|s| s.tx.clone()).collect()
        };

        for tx in subs {
            let _ = tx.try_send(envelope.clone());
        }

        if let Some(tracker) = tracker {
            tracker.clone().wait_owned().await;
            if let Some(error) = tracker.take_error() {
                return Err(SendError::Downstream(error));
            }
        }

        Ok(())
    }

    /// Check if the channel is closed or the owner has been dropped.
    pub fn is_closed(&self) -> bool {
        match self.inner.upgrade() {
            Some(inner) => inner.closed.load(Ordering::SeqCst),
            None => true,
        }
    }
}

// ============================================================================
// RelayReceiver - Observer
// ============================================================================

/// A receiver handle for subscribing to channel messages.
///
/// This type is `Clone` but does **not** keep the channel alive.
/// When the `RelaySender` is dropped, all subscriptions will return `None`.
#[derive(Clone)]
pub struct RelayReceiver {
    inner: Arc<Inner>,
}

impl RelayReceiver {
    /// Subscribe to messages of type `T`.
    ///
    /// Returns a `Subscription` that yields messages until the channel closes.
    pub fn subscribe<T: 'static + Send + Sync>(&self) -> Subscription<T> {
        let (tx, rx) = mpsc::channel(self.inner.channel_size);
        self.inner
            .subscribers
            .lock()
            .push(SubscriberSender::new(tx));
        Subscription::new(rx)
    }

    /// Subscribe to all messages regardless of type.
    ///
    /// Returns raw `Envelope`s containing type-erased values.
    pub fn subscribe_all(&self) -> mpsc::Receiver<Envelope> {
        let (tx, rx) = mpsc::channel(self.inner.channel_size);
        self.inner
            .subscribers
            .lock()
            .push(SubscriberSender::new(tx));
        rx
    }

    /// Subscribe with completion tracking (for internal use).
    pub fn subscribe_tracked<T: 'static + Send + Sync>(
        &self,
    ) -> (Subscription<T>, Arc<AtomicUsize>) {
        let (tx, rx) = mpsc::channel(self.inner.channel_size);
        self.inner
            .subscribers
            .lock()
            .push(SubscriberSender::new(tx));
        self.inner.handler_count.fetch_add(1, Ordering::SeqCst);
        (
            Subscription::new_tracked(rx),
            self.inner.handler_count.clone(),
        )
    }

    /// Check if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Get the unique ID of this channel.
    pub fn id(&self) -> u64 {
        self.inner.id
    }
}
