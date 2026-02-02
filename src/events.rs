/// Event emitted when a message is dropped due to a slow consumer.
///
/// Subscribe to this event to monitor stream health and identifying slow parts of your pipeline.
#[derive(Clone, Debug)]
pub struct Dropped {
    /// The ID of the message that was dropped.
    pub msg_id: u64,
    /// The ID of the stream where the drop occurred (usually the origin of the message).
    pub origin_stream_id: u64,
    /// The type name of the dropped event (for debugging).
    pub event_type_name: &'static str,
}
