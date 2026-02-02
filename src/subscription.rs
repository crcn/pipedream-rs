use std::marker::PhantomData;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::envelope::Envelope;
use crate::error::RelayError;
use crate::tracker::CompletionTracker;

/// A typed subscription to a Relay.
///
/// Receives messages of type `T` from the stream.
/// Type filtering happens locally - wrong types are skipped, not errored.
///
/// ## Lossless Delivery
///
/// All subscriptions receive all messages. There is no message dropping.
/// Slow consumers apply backpressure to senders.
///
/// ## Completion Semantics
///
/// There are two kinds of subscriptions:
/// - **Tracked** (used by sink/tap): Participates in completion tracking.
///   Signals completion for wrong-type messages, fails tracker on Drop.
/// - **Untracked** (from `subscribe()`): Does NOT participate in tracking.
///   Still receives all messages, but sender doesn't wait for completion.
///
/// Tracked subscriptions are created internally by sink/tap handlers.
/// Raw subscriptions from `subscribe()` are untracked.
pub struct Subscription<T> {
    rx: mpsc::Receiver<Envelope>,
    /// Tracker for the message currently being processed
    current_tracker: Option<Arc<CompletionTracker>>,
    /// Message ID of the message currently being processed
    current_msg_id: Option<u64>,
    /// Whether this subscription participates in completion tracking.
    /// Set to true for sink/tap handlers, false for raw subscribe().
    tracked: bool,
    _marker: PhantomData<T>,
}

impl<T: 'static + Send + Sync> Subscription<T> {
    /// Create an untracked subscription (for raw subscribe()).
    /// Does NOT participate in completion tracking.
    pub(crate) fn new(rx: mpsc::Receiver<Envelope>) -> Self {
        Self {
            rx,
            current_tracker: None,
            current_msg_id: None,
            tracked: false,
            _marker: PhantomData,
        }
    }

    /// Create a tracked subscription (for sink/tap handlers).
    /// Participates in completion tracking - signals completion for
    /// wrong-type messages and fails tracker on Drop.
    pub(crate) fn new_tracked(rx: mpsc::Receiver<Envelope>) -> Self {
        Self {
            rx,
            current_tracker: None,
            current_msg_id: None,
            tracked: true,
            _marker: PhantomData,
        }
    }

    /// Get the completion tracker for the current message.
    ///
    /// Returns `None` if no message is being processed or if the message
    /// was sent without completion tracking (fire-and-forget).
    ///
    /// Handlers should call `tracker.complete_one()` on success or
    /// `tracker.fail(error)` on failure.
    pub fn current_tracker(&self) -> Option<Arc<CompletionTracker>> {
        self.current_tracker.clone()
    }

    /// Get the message ID of the current message being processed.
    ///
    /// Returns `None` if no message is being processed.
    pub fn current_msg_id(&self) -> Option<u64> {
        self.current_msg_id
    }

    /// Clear the current tracker without signaling completion.
    ///
    /// Called by handlers after they've explicitly completed the tracker.
    /// This prevents double-completion and ensures Drop doesn't fail
    /// an already-completed message.
    pub fn clear_tracker(&mut self) {
        self.current_tracker = None;
        self.current_msg_id = None;
    }

    /// Receive the next message of type T.
    ///
    /// Returns `None` when the stream is closed.
    /// Messages of other types are skipped automatically.
    ///
    /// **Important**: Handlers must call `complete_one()` or `fail()` on the
    /// tracker before calling `recv()` again. This method does NOT auto-complete
    /// for messages of the matching type.
    pub async fn recv(&mut self) -> Option<Arc<T>> {
        // Clear previous message state (handler should have already completed it)
        self.current_tracker = None;
        self.current_msg_id = None;

        loop {
            match self.rx.recv().await {
                Some(env) => {
                    if let Some(value) = env.downcast::<T>() {
                        // Store tracker for this message
                        self.current_tracker = env.tracker();
                        self.current_msg_id = Some(env.msg_id());
                        return Some(value);
                    }
                    // Wrong type - this handler can't process it.
                    // Only tracked subscriptions (sink/tap) signal completion.
                    if self.tracked {
                        if let Some(tracker) = env.tracker() {
                            tracker.complete_one();
                        }
                    }
                }
                None => return None, // Channel closed
            }
        }
    }

    /// Try to receive a message without waiting.
    ///
    /// Returns `None` if no message is available or the stream is closed.
    ///
    /// **Important**: Handlers must call `complete_one()` or `fail()` on the
    /// tracker before calling `try_recv()` again. This method does NOT auto-complete
    /// for messages of the matching type.
    pub fn try_recv(&mut self) -> Option<Arc<T>> {
        // Clear previous message state (handler should have already completed it)
        self.current_tracker = None;
        self.current_msg_id = None;

        loop {
            match self.rx.try_recv() {
                Ok(env) => {
                    if let Some(value) = env.downcast::<T>() {
                        // Store tracker for this message
                        self.current_tracker = env.tracker();
                        self.current_msg_id = Some(env.msg_id());
                        return Some(value);
                    }
                    // Wrong type - only tracked subscriptions signal completion
                    if self.tracked {
                        if let Some(tracker) = env.tracker() {
                            tracker.complete_one();
                        }
                    }
                }
                Err(_) => return None,
            }
        }
    }
}

impl<T> Drop for Subscription<T> {
    fn drop(&mut self) {
        // Safety net: only for tracked subscriptions.
        // If dropped while processing and handler didn't complete, fail tracker.
        // Untracked subscriptions don't participate in completion tracking.
        if self.tracked {
            if let Some(tracker) = self.current_tracker.take() {
                let error = RelayError::new(
                    self.current_msg_id.unwrap_or(0),
                    std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "subscription dropped while processing message",
                    ),
                    "subscription",
                );
                tracker.fail(error);
            }
        }
    }
}

impl<T> std::fmt::Debug for Subscription<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("type", &std::any::type_name::<T>())
            .field("tracked", &self.tracked)
            .field("has_current_tracker", &self.current_tracker.is_some())
            .finish()
    }
}
