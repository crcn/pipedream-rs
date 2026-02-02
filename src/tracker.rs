use parking_lot::Mutex;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::Notify;

use crate::error::RelayError;

/// Tracks completion of a single message across all handlers.
///
/// ## Semantics
///
/// The `handler_count` at send time determines `expected`. This is "best effort" -
/// handlers registered concurrently with a send may or may not be counted.
/// Document explicitly: send awaits approximately the current handler count;
/// handlers added concurrently may or may not be accounted for.
pub struct CompletionTracker {
    expected: AtomicUsize,
    completed: AtomicUsize,
    error: Mutex<Option<RelayError>>,
    notify: Arc<Notify>,
}

impl CompletionTracker {
    /// Create a new tracker expecting `expected` handlers to complete.
    ///
    /// Note: `expected` is a snapshot at send time. Handlers registered
    /// concurrently may not be included in this count.
    pub fn new(expected: usize) -> Self {
        Self {
            expected: AtomicUsize::new(expected),
            completed: AtomicUsize::new(0),
            error: Mutex::new(None),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Get the expected count.
    pub fn expected(&self) -> usize {
        self.expected.load(Ordering::SeqCst)
    }

    /// Get the completed count.
    pub fn completed(&self) -> usize {
        self.completed.load(Ordering::SeqCst)
    }

    /// Check if all handlers have completed.
    pub fn is_complete(&self) -> bool {
        self.completed() >= self.expected()
    }

    /// Signal successful completion of one handler.
    ///
    /// Calling this more than once per handler is a logic error but is
    /// handled gracefully (extra completions are ignored after the
    /// expected count is reached).
    pub fn complete_one(&self) {
        let completed = self.completed.fetch_add(1, Ordering::SeqCst) + 1;
        let expected = self.expected.load(Ordering::SeqCst);

        // Debug assertion to catch double-completion bugs during development
        debug_assert!(
            completed <= expected + 1,
            "complete_one called {} times but only {} expected (possible double-completion bug)",
            completed,
            expected
        );

        if completed >= expected {
            self.notify.notify_waiters();
        }
    }

    /// Signal failure with an error.
    pub fn fail(&self, error: RelayError) {
        {
            let mut err = self.error.lock();
            if err.is_none() {
                *err = Some(error);
            }
        }
        self.complete_one();
    }

    /// Take the error if one occurred.
    pub fn take_error(&self) -> Option<RelayError> {
        self.error.lock().take()
    }

    /// Returns a future that completes when all handlers have finished.
    ///
    /// This is the proper way to await completion - it's a pollable future
    /// that doesn't spawn tasks from within poll().
    pub fn wait(&self) -> CompletionFuture<'_> {
        CompletionFuture { tracker: self }
    }

    /// Async method to wait for completion.
    ///
    /// Alternative to `wait()` for use in async contexts where you want to
    /// await directly.
    pub async fn wait_async(&self) {
        while !self.is_complete() {
            self.notify.notified().await;
        }
    }
}

impl CompletionTracker {
    /// Returns an owned future that completes when all handlers have finished.
    ///
    /// This version takes `Arc<Self>` and returns a `'static` future that can
    /// be stored in a state machine.
    pub async fn wait_owned(self: Arc<Self>) {
        while !self.is_complete() {
            self.notify.notified().await;
        }
    }
}

/// Future returned by `CompletionTracker::wait()`.
///
/// Polls completion state and awaits notification without spawning tasks.
pub struct CompletionFuture<'a> {
    tracker: &'a CompletionTracker,
}

impl<'a> Future for CompletionFuture<'a> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Check if already complete
        if self.tracker.is_complete() {
            return Poll::Ready(());
        }

        // Register waker using enable() pattern - this is the proper way
        // to integrate with Notify without storing futures
        let notified = self.tracker.notify.notified();
        // Pin it on the stack
        futures::pin_mut!(notified);

        // Enable the notified future to receive wake notifications
        // This registers the current task's waker with the Notify
        notified.as_mut().enable();

        // Check again after registering (avoid race condition)
        if self.tracker.is_complete() {
            return Poll::Ready(());
        }

        // Now poll the notified future to properly register the waker
        match notified.as_mut().poll(cx) {
            Poll::Ready(()) => {
                // Notification received, check completion again
                if self.tracker.is_complete() {
                    Poll::Ready(())
                } else {
                    // Not yet complete, wake self to retry
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl std::fmt::Debug for CompletionTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionTracker")
            .field("expected", &self.expected())
            .field("completed", &self.completed())
            .field("has_error", &self.error.lock().is_some())
            .finish()
    }
}
