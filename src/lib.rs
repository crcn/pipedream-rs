mod envelope;
mod error;
mod events;
mod stream;
mod subscription;
mod tracker;

pub use events::Dropped;

pub use envelope::Envelope;
pub use error::{PanicError, RelayError};

// Public channel API - the recommended way to create relays
pub use stream::{Relay, RelayReceiver, RelaySender, SendError, WeakSender};

// Keep Subscription public for typed message receiving
pub use subscription::Subscription;

// Internal types - not part of public API but needed for advanced use cases
#[doc(hidden)]
pub use stream::IntoResult;

pub use tracker::CompletionTracker;

#[cfg(test)]
mod tests;
