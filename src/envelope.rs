use std::any::{Any, TypeId};
use std::sync::Arc;

use crate::tracker::CompletionTracker;

/// A type-erased envelope that can carry any `Send + Sync` value.
#[derive(Clone)]
pub struct Envelope {
    pub value: Arc<dyn Any + Send + Sync>,
    pub(crate) type_id: TypeId,
    pub(crate) msg_id: u64,
    pub(crate) tracker: Option<Arc<CompletionTracker>>,
    /// Stream ID where this envelope originated (for echo prevention)
    pub(crate) origin: u64,
}

impl Envelope {
    /// Create a new envelope from any `Send + Sync` value.
    pub fn new<T: 'static + Send + Sync>(
        value: T,
        msg_id: u64,
        tracker: Option<Arc<CompletionTracker>>,
    ) -> Self {
        Self {
            value: Arc::new(value),
            type_id: TypeId::of::<T>(),
            msg_id,
            tracker,
            origin: 0, // Will be set by the sending stream
        }
    }

    /// Create a new envelope with a specific origin stream ID.
    pub fn with_origin<T: 'static + Send + Sync>(
        value: T,
        msg_id: u64,
        tracker: Option<Arc<CompletionTracker>>,
        origin: u64,
    ) -> Self {
        Self {
            value: Arc::new(value),
            type_id: TypeId::of::<T>(),
            msg_id,
            tracker,
            origin,
        }
    }

    /// Create an envelope from an already type-erased value with known TypeId.
    ///
    /// This is useful when you have a value that's already been type-erased
    /// (e.g., from a registry or dynamic dispatch) but want to preserve
    /// its original type for downstream filtering.
    pub fn from_any(
        value: Arc<dyn Any + Send + Sync>,
        type_id: TypeId,
        msg_id: u64,
        tracker: Option<Arc<CompletionTracker>>,
    ) -> Self {
        Self {
            value,
            type_id,
            msg_id,
            tracker,
            origin: 0, // Will be set by the sending stream
        }
    }

    /// Create an envelope from type-erased value with a specific origin.
    pub fn from_any_with_origin(
        value: Arc<dyn Any + Send + Sync>,
        type_id: TypeId,
        msg_id: u64,
        tracker: Option<Arc<CompletionTracker>>,
        origin: u64,
    ) -> Self {
        Self {
            value,
            type_id,
            msg_id,
            tracker,
            origin,
        }
    }

    /// Get the TypeId of the contained value.
    pub fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// Get the message ID.
    pub fn msg_id(&self) -> u64 {
        self.msg_id
    }

    /// Get the completion tracker if present.
    pub fn tracker(&self) -> Option<Arc<CompletionTracker>> {
        self.tracker.clone()
    }

    /// Get the origin stream ID.
    pub fn origin(&self) -> u64 {
        self.origin
    }

    /// Create a copy of this envelope with a new origin.
    pub fn with_new_origin(&self, origin: u64) -> Self {
        Self {
            value: self.value.clone(),
            type_id: self.type_id,
            msg_id: self.msg_id,
            tracker: self.tracker.clone(),
            origin,
        }
    }

    /// Create a copy of this envelope without a tracker.
    /// Used when forwarding events - the target stream has its own handlers.
    pub fn without_tracker(&self) -> Self {
        Self {
            value: self.value.clone(),
            type_id: self.type_id,
            msg_id: self.msg_id,
            tracker: None,
            origin: self.origin,
        }
    }

    /// Attempt to downcast to a concrete type reference.
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.value.downcast_ref()
    }

    /// Attempt to get an Arc of the concrete type.
    pub fn downcast<T: 'static + Send + Sync>(&self) -> Option<Arc<T>> {
        Arc::downcast(self.value.clone()).ok()
    }

    /// Check if the envelope contains a value of type T.
    pub fn is<T: 'static>(&self) -> bool {
        self.type_id == TypeId::of::<T>()
    }
}

impl std::fmt::Debug for Envelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Envelope")
            .field("type_id", &self.type_id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envelope_creation() {
        let env = Envelope::new("hello".to_string(), 0, None);
        assert!(env.is::<String>());
        assert!(!env.is::<i32>());
    }

    #[test]
    fn test_envelope_downcast() {
        let env = Envelope::new(42i32, 0, None);
        assert_eq!(env.downcast_ref::<i32>(), Some(&42));
        assert_eq!(env.downcast_ref::<String>(), None);
    }

    #[test]
    fn test_envelope_clone() {
        let env1 = Envelope::new("test".to_string(), 0, None);
        let env2 = env1.clone();
        assert_eq!(env1.downcast_ref::<String>(), env2.downcast_ref::<String>());
    }

    #[test]
    fn test_envelope_msg_id() {
        let env = Envelope::new("test".to_string(), 42, None);
        assert_eq!(env.msg_id(), 42);
    }

    #[test]
    fn test_envelope_tracker() {
        let tracker = Arc::new(CompletionTracker::new(1));
        let env = Envelope::new("test".to_string(), 0, Some(tracker.clone()));
        assert!(env.tracker().is_some());

        let env_no_tracker = Envelope::new("test".to_string(), 0, None);
        assert!(env_no_tracker.tracker().is_none());
    }
}
