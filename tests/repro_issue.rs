use pipedream_rs::Relay;
use std::time::Duration;

#[tokio::test]
async fn test_concurrent_usage_repro() {
    let relay = Relay::new();

    // Subscribe BEFORE spawning to avoid race condition
    let mut sub = relay.subscribe::<String>();

    let handle = tokio::spawn(async move {
        loop {
            match sub.recv().await {
                Some(data) => {
                    println!("Received: {:?}", data);
                    if *data == "stop" {
                        break;
                    }
                }
                None => {
                    println!("Stream closed");
                    break;
                }
            }
        }
    });

    // Now safe to send - subscription already exists
    println!("Sending hello");
    relay.send("hello".to_string()).await.unwrap();

    println!("Sending stop");
    relay.send("stop".to_string()).await.unwrap();

    // Wait for task
    match tokio::time::timeout(Duration::from_secs(2), handle).await {
        Ok(_) => println!("Task finished"),
        Err(_) => panic!("Task timed out"),
    }
}
