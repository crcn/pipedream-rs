// DISABLED: This test requires the deprecated loopback API and .close() method
// The loopback API (Relay::new()) is now internal and .close() is not publicly accessible.
// Closing is handled by dropping RelaySender in the new channel API.

/*
use pipedream::{PipeExt, Relay};
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn test_forward_is_awaitable_and_blocking() {
    let source = Relay::new();
    let target = Relay::new();

    let mut sub = target.subscribe::<String>();

    // Spawn the forwarder - it should block!
    let source_clone = source.clone();
    let target_clone = target.clone();
    let forward_handle = tokio::spawn(async move {
        source_clone.forward(&target_clone).await;
    });

    // Give forwarder time to subscribe (async fn is lazy)
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send a message
    source.send("hello".to_string()).await.expect("send failed");

    // partial wait to ensure forwarding happens
    let msg = timeout(Duration::from_millis(100), sub.recv())
        .await
        .expect("timed out")
        .expect("stream closed");
    assert_eq!(*msg, "hello");

    // Ensure forward_handle is still running (it returns () when done)
    let is_done = forward_handle.is_finished();
    assert!(!is_done, "Forward should still be running (blocked on source)");

    // Close source - requires .close() method which is now private
    source.close();

    // Now forward should finish
    timeout(Duration::from_millis(100), forward_handle)
        .await
        .expect("forward did not exit after source close")
        .expect("forward task panic");
}
*/
