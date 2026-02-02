use pipedream_rs::Relay;
use std::time::Duration;
use tokio::time::timeout;

#[ignore]
#[tokio::test]
async fn test_slow_subscriber_does_not_block_sender() {
    let relay = Relay::with_channel_size(1); // Very small buffer
    let _sub = relay.subscribe::<String>();

    let relay_clone = relay.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        // Send way more messages than buffer size
        for i in 0..100 {
            // This should NOT fail (SendError::Closed only) and should NOT block
            if let Err(_) = relay_clone.send(format!("msg {}", i)).await {
                break;
            }
        }
        let _ = tx.send(true);
    });

    // Current behavior (non-blocking): Should complete quickly.

    match timeout(Duration::from_millis(500), rx).await {
        Ok(_) => println!("Sender finished (Did NOT block) - SUCCESS"),
        Err(_) => panic!("Sender blocked (Timed out) - FAILURE"),
    }
}
