use parking_lot::Mutex;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{mpsc, Notify};
use tracing::info_span;

use crate::envelope::Envelope;
use crate::error::{PanicError, RelayError};
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
pub struct ReadyGuard {
    inner: Option<Arc<Inner>>,
    signaled: bool,
}

impl ReadyGuard {
    fn new(inner: Arc<Inner>) -> Self {
        inner.pending_ready.fetch_add(1, Ordering::SeqCst);
        Self {
            inner: Some(inner),
            signaled: false,
        }
    }

    /// Signal that this task is ready to receive messages.
    /// Should be called after the task is spawned and set up.
    pub fn signal(&mut self) {
        if !self.signaled {
            self.signaled = true;
            if let Some(inner) = self.inner.take() {
                inner.ready_count.fetch_add(1, Ordering::SeqCst);
                inner.ready_notify.notify_waiters();
            }
        }
    }
}

impl Drop for ReadyGuard {
    fn drop(&mut self) {
        // Ensure ready is signaled even if task panics or is cancelled
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
// ============================================================================
// Stream - the main stream type (loopback: readable/writable share inner)
// ============================================================================

/// A typed, heterogeneous relay with lossless delivery and completion tracking.
#[derive(Clone)]
/// Relay namespace for creating channels.
///
/// Use `Relay::channel()` to create a new relay channel.
pub struct Relay;

impl Relay {
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

    /// Temporary helper for tests - creates a loopback-style wrapper.
    /// TODO: Remove once tests are migrated to channel API.
    #[doc(hidden)]
    pub fn new() -> TestRelay {
        let (tx, rx) = Self::channel();
        TestRelay { tx: Arc::new(tx), rx }
    }

    /// Temporary helper for tests with custom size.
    #[doc(hidden)]
    pub fn with_channel_size(size: usize) -> TestRelay {
        let (tx, rx) = Self::channel_with_size(size);
        TestRelay { tx: Arc::new(tx), rx }
    }
}

/// Temporary test helper that provides loopback-style API.
/// TODO: Remove once tests are migrated to channel API.
#[doc(hidden)]
pub struct TestRelay {
    tx: Arc<RelaySender>,
    pub rx: RelayReceiver,
}

impl TestRelay {
    pub async fn send<T: 'static + Send + Sync>(&self, value: T) -> Result<(), SendError> {
        self.tx.send(value).await
    }

    pub fn subscribe<T: 'static + Send + Sync>(&self) -> Subscription<T> {
        self.rx.subscribe()
    }

    pub fn sink<T, F, R>(&self, f: F)
    where
        T: 'static + Send + Sync,
        F: Fn(&T) -> R + Send + Sync + 'static,
        R: IntoResult + 'static,
    {
        let (mut sub, handler_count) = self.rx.subscribe_tracked::<T>();
        let weak_tx = self.tx.weak();
        let msg_type = std::any::type_name::<T>();
        let _handler_guard = HandlerGuard::new(handler_count);

        tokio::spawn(async move {
            let _guard = _handler_guard;
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

                        // Emit error asynchronously (non-blocking)
                        tokio::spawn({
                            let weak_tx = weak_tx.clone();
                            let error = error.clone();
                            async move {
                                let _ = weak_tx.send(error).await;
                            }
                        });

                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                    Err(panic_info) => {
                        let error = RelayError::new(msg_id, PanicError::new(panic_info), "sink");

                        // Emit error asynchronously (non-blocking)
                        tokio::spawn({
                            let weak_tx = weak_tx.clone();
                            let error = error.clone();
                            async move {
                                let _ = weak_tx.send(error).await;
                            }
                        });

                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                }
            }
        });
    }

    pub fn tap<T, F, R>(&self, f: F)
    where
        T: 'static + Send + Sync,
        F: Fn(&T) -> R + Send + Sync + 'static,
        R: IntoResult + 'static,
    {
        let (mut sub, handler_count) = self.rx.subscribe_tracked::<T>();
        let weak_tx = self.tx.weak();
        let msg_type = std::any::type_name::<T>();
        let _handler_guard = HandlerGuard::new(handler_count);

        tokio::spawn(async move {
            let _guard = _handler_guard;
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

                        // Emit error asynchronously (non-blocking)
                        tokio::spawn({
                            let weak_tx = weak_tx.clone();
                            let error = error.clone();
                            async move {
                                let _ = weak_tx.send(error).await;
                            }
                        });

                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                    Err(panic_info) => {
                        let error = RelayError::new(msg_id, PanicError::new(panic_info), "tap");

                        // Emit error asynchronously (non-blocking)
                        tokio::spawn({
                            let weak_tx = weak_tx.clone();
                            let error = error.clone();
                            async move {
                                let _ = weak_tx.send(error).await;
                            }
                        });

                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                }
            }
        });
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    pub fn handler_count(&self) -> usize {
        self.tx.handler_count()
    }

    pub fn close(&self) {
        self.tx.close()
    }

    pub async fn send_any(
        &self,
        value: Arc<dyn std::any::Any + Send + Sync>,
        type_id: std::any::TypeId,
    ) -> Result<(), SendError> {
        self.tx.send_any(value, type_id).await
    }

    pub async fn send_envelope(&self, envelope: Envelope) -> Result<(), SendError> {
        self.tx.send_envelope(envelope).await
    }

    pub fn within<F, Fut>(&self, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        self.rx.within(f);
    }
}

impl Clone for TestRelay {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            rx: self.rx.clone(),
        }
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

    /// Send a pre-constructed envelope.
    /// Used for advanced scenarios like forwarding.
    pub async fn send_envelope(&self, envelope: Envelope) -> Result<(), SendError> {
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

    /// Get the current handler count for this relay.
    pub fn handler_count(&self) -> usize {
        self.inner.handler_count.load(Ordering::SeqCst)
    }

    /// Close the channel explicitly.
    /// This has the same effect as dropping the RelaySender.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.subscribers.lock().clear();
    }

    /// Temporary clone method for tests only.
    /// Violates the single-owner design but needed for test compatibility.
    #[doc(hidden)]
    pub fn clone_for_test(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
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
    /// Returns (subscription, handler_count).
    /// The subscription is immediately ready to receive messages (channel is created and buffering).
    pub fn subscribe_tracked<T: 'static + Send + Sync>(
        &self,
    ) -> (Subscription<T>, Arc<AtomicUsize>) {
        let (tx, rx) = mpsc::channel(self.inner.channel_size);
        self.inner
            .subscribers
            .lock()
            .push(SubscriberSender::new(tx));
        self.inner.handler_count.fetch_add(1, Ordering::SeqCst);

        // Signal ready immediately - the subscription can receive messages now
        let mut ready_guard = ReadyGuard::new(self.inner.clone());
        ready_guard.signal();

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

    /// Attach a handler that consumes messages of type `T`.
    ///
    /// The sender will wait for this handler to complete before `send()` returns.
    /// Errors and panics are propagated back to the sender via the completion tracker.
    pub fn sink<T, F, R>(&self, f: F)
    where
        T: 'static + Send + Sync,
        F: Fn(&T) -> R + Send + Sync + 'static,
        R: IntoResult + 'static,
    {
        let (mut sub, handler_count) = self.subscribe_tracked::<T>();
        let msg_type = std::any::type_name::<T>();
        let _handler_guard = HandlerGuard::new(handler_count);

        tokio::spawn(async move {
            let _guard = _handler_guard;
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
                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                    Err(panic_info) => {
                        let error = RelayError::new(msg_id, PanicError::new(panic_info), "sink");
                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                }
            }
        });
    }

    /// Attach an observer that sees messages of type `T` without consuming them.
    ///
    /// Similar to `sink()` but doesn't prevent other handlers from receiving the message.
    /// The sender will wait for this handler to complete before `send()` returns.
    pub fn tap<T, F, R>(&self, f: F)
    where
        T: 'static + Send + Sync,
        F: Fn(&T) -> R + Send + Sync + 'static,
        R: IntoResult + 'static,
    {
        let (mut sub, handler_count) = self.subscribe_tracked::<T>();
        let msg_type = std::any::type_name::<T>();
        let _handler_guard = HandlerGuard::new(handler_count);

        tokio::spawn(async move {
            let _guard = _handler_guard;
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
                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                    Err(panic_info) => {
                        let error = RelayError::new(msg_id, PanicError::new(panic_info), "tap");
                        if let Some(t) = tracker {
                            t.fail(error);
                        }
                        sub.clear_tracker();
                    }
                }
            }
        });
    }

    /// Spawn a custom async task with panic catching.
    ///
    /// Unlike sink/tap, this does NOT participate in completion tracking.
    /// Panics are caught but not propagated (just logged).
    pub fn within<F, Fut>(&self, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        use futures::FutureExt;
        tokio::spawn(async move {
            let result = AssertUnwindSafe(f()).catch_unwind().await;
            if let Err(panic_info) = result {
                eprintln!("Panic in within(): {:?}", PanicError::new(panic_info));
            }
        });
    }
}
