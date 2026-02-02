// This test has been disabled because WeakReadable functionality was removed.
// The test was verifying that weak references to streams don't prevent the stream from being dropped.

/*
use pipedream_rs::Relay;
use std::time::Duration;
use tokio::time::timeout;

#[ignore]
#[tokio::test]
async fn test_weak_stream_allows_drop() {
    // 1. Create a Relay (Source)
    let relay = Relay::new();

    // 2. Create a weak reference to the readable side
    let weak_stream = relay.as_readable().downgrade();

    // 3. Clone relay for the producer task
    let producer_relay = relay.clone();

    // 4. Spawn a producer task that holds the strong ref
    let producer_handle = tokio::spawn(async move {
        producer_relay.send("message 1").await.expect("send failed");
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Producer drops the strong ref when it exits
    });

    // 5. Drop our local strong ref so producer has the only one
    drop(relay);

    // 6. Consumer uses WEAK stream
    // Using weak_stream.subscribe() should work
    let mut sub = weak_stream.subscribe::<&str>();

    // Verify we get the message
    let msg = timeout(Duration::from_millis(100), sub.recv())
        .await
        .expect("timed out waiting for message")
        .expect("stream closed unexpectedly");
    assert_eq!(*msg, "message 1");

    // Wait for producer to finish and drop its ref
    producer_handle.await.expect("producer failed");

    // 7. Verify stream closes
    // If weak_stream held a strong ref, this would hang effectively forever
    // (or until we dropped weak_stream, but we're testing that it DOESN'T permit receive indefinitely)
    let next = timeout(Duration::from_millis(100), sub.recv()).await;

    match next {
        Ok(None) => {
            // Success! Stream closed because all strong refs were dropped,
            // even though we still hold weak_stream and the subscription.
        }
        Ok(Some(_)) => panic!("Should not receive more messages"),
        Err(_) => panic!("Timed out - stream did not close! Weak ref is likely keeping it alive."),
    }
}
*/
