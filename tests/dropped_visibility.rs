use pipedream_rs::{Dropped, Relay};
use std::time::Duration;
use tokio::time::timeout;

#[ignore]
#[tokio::test]
async fn test_dropped_visibility() {
    // Use small buffer to trigger drops quickly
    let relay = Relay::with_channel_size(10);

    // 1. Slow subscriber (fills up, we don't read it)
    let _slow_sub = relay.subscribe::<String>();

    // 2. Monitor subscriber (listens for drops)
    // MUST consume concurrently because it receives all messages (even non-Dropped) and filters locally.
    let mut monitor = relay.subscribe::<Dropped>();
    let monitor_handle = tokio::spawn(async move {
        let mut drops = 0;
        // Read for a while or until we get enough drops
        while let Ok(Some(_)) = timeout(Duration::from_millis(500), monitor.recv()).await {
            drops += 1;
        }
        drops
    });

    // 3. Fast subscriber (should get everything)
    let mut fast_sub = relay.subscribe::<String>();
    let fast_sub_handle = tokio::spawn(async move {
        let mut count = 0;
        while let Some(_) = fast_sub.recv().await {
            count += 1;
            // Limit to expected count to avoid infinite waiting
            if count == 20 {
                break;
            }
        }
        count
    });

    // Send 20 messages (buffer is 10)
    for i in 0..20 {
        relay.send(format!("msg {}", i)).await.expect("send failed");
        // Yield to allow consumers to drain
        tokio::task::yield_now().await;
    }

    // Fast subscriber should have received all 20
    let received_count = timeout(Duration::from_secs(1), fast_sub_handle)
        .await
        .expect("Fast subscriber timed out")
        .expect("Task failed");

    assert_eq!(
        received_count, 20,
        "Fast subscriber should receive all messages"
    );

    // Monitor should receive Dropped events
    let drops_received = monitor_handle.await.expect("Monitor task failed");

    println!("Received {} dropped events", drops_received);
    // We expect some drops. Exact number depends on timing/races, but definitely > 0.
    // Logic: 20 msgs sent. Buffer 10. `slow_sub` absorbs 10. 11-20 should drop. So ~10 drops.
    assert!(drops_received > 0, "Should have received dropped events");
}
