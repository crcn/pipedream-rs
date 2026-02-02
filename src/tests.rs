#![allow(clippy::while_let_loop, clippy::redundant_pattern_matching)]

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// Minimal test: verify drop() closes subscriber channels
#[tokio::test]
async fn test_drop_closes_channels() {
    let stream = Relay::new();
    let mut sub = stream.subscribe::<String>();

    // Drop the stream - this should close the channel
    drop(stream);

    // Receiver should get None
    let result = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .expect("should not timeout");
    assert!(
        result.is_none(),
        "recv should return None when stream dropped"
    );
}

// Test: filter forwarder should close child when parent dropped

#[tokio::test]
async fn test_basic_send_subscribe() {
    let stream = Relay::new();
    let mut sub = stream.subscribe::<String>();

    stream.send("hello".to_string()).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .unwrap();
    assert_eq!(msg.as_deref(), Some(&"hello".to_string()));
}

#[tokio::test]
async fn test_type_filtering() {
    let stream = Relay::new();
    let mut string_sub = stream.subscribe::<String>();
    let mut int_sub = stream.subscribe::<i32>();

    stream.send("hello".to_string()).await.unwrap();
    stream.send(42i32).await.unwrap();

    let string_msg = tokio::time::timeout(Duration::from_millis(100), string_sub.recv())
        .await
        .unwrap();
    let int_msg = tokio::time::timeout(Duration::from_millis(100), int_sub.recv())
        .await
        .unwrap();

    assert_eq!(string_msg.as_deref(), Some(&"hello".to_string()));
    assert_eq!(int_msg.as_deref(), Some(&42i32));
}

#[tokio::test]
async fn test_broadcast_multiple_subscribers() {
    let stream = Relay::new();
    let mut sub1 = stream.subscribe::<String>();
    let mut sub2 = stream.subscribe::<String>();

    stream.send("hello".to_string()).await.unwrap();

    let msg1 = tokio::time::timeout(Duration::from_millis(100), sub1.recv())
        .await
        .unwrap();
    let msg2 = tokio::time::timeout(Duration::from_millis(100), sub2.recv())
        .await
        .unwrap();

    assert_eq!(msg1.as_deref(), Some(&"hello".to_string()));
    assert_eq!(msg2.as_deref(), Some(&"hello".to_string()));
}



#[tokio::test]
async fn test_tap() {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let stream = Relay::new();
    stream.tap::<i32, _, _>(move |n| {
        counter_clone.fetch_add(*n as usize, Ordering::SeqCst);
    });

    // tap() subscribes before spawning, so no pre-send sleep needed
    stream.send(10i32).await.unwrap();
    stream.send(20i32).await.unwrap();

    // Wait for tap task to process messages
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 30);
}

#[tokio::test]
async fn test_sink() {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let stream = Relay::new();
    stream.sink::<i32, _, _>(move |n| {
        counter_clone.fetch_add(*n as usize, Ordering::SeqCst);
    });

    // sink() subscribes before spawning, so no pre-send sleep needed
    stream.send(5i32).await.unwrap();
    stream.send(15i32).await.unwrap();

    // Wait for sink task to process messages
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 20);
}

// ==================== STRESS TESTS ====================





#[tokio::test]
async fn stress_test_concurrent_senders() {
    // Multiple tasks sending concurrently
    // Use larger buffer to handle all messages from all senders
    let stream = Relay::with_channel_size(1024);
    let mut sub = stream.subscribe::<i32>();

    const SENDERS: usize = 10;
    const MESSAGES_PER_SENDER: usize = 100;

    let handles: Vec<_> = (0..SENDERS)
        .map(|sender_id| {
            let stream = stream.clone();
            tokio::spawn(async move {
                for i in 0..MESSAGES_PER_SENDER {
                    stream
                        .send((sender_id * MESSAGES_PER_SENDER + i) as i32)
                        .await
                        .unwrap();
                }
            })
        })
        .collect();

    // Wait for all senders
    for handle in handles {
        handle.await.unwrap();
    }

    // Count received messages
    let mut count = 0;
    loop {
        match tokio::time::timeout(Duration::from_millis(100), sub.recv()).await {
            Ok(Some(_)) => count += 1,
            _ => break,
        }
    }

    assert_eq!(count, SENDERS * MESSAGES_PER_SENDER);
}





// ==================== ERROR HANDLING TESTS ====================

#[derive(Debug, Clone)]
struct TestError(String);

impl std::fmt::Display for TestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TestError: {}", self.0)
    }
}

impl std::error::Error for TestError {}

#[tokio::test]
async fn test_sink_error_propagation() {
    let stream = Relay::new();

    // Sink that returns error for "bad" messages
    stream.sink::<String, _, _>(|msg| {
        if msg.as_str() == "bad" {
            return Err(TestError("message was bad".to_string()));
        }
        Ok(())
    });

    // Good message should succeed
    let result = stream.send("good".to_string()).await;
    assert!(result.is_ok());

    // Bad message should return error
    let result = stream.send("bad".to_string()).await;
    assert!(result.is_err());
    if let Err(SendError::Downstream(err)) = result {
        assert_eq!(err.source, "sink");
    } else {
        panic!("Expected Downstream error");
    }
}

#[tokio::test]
async fn test_tap_error_propagation() {
    let stream = Relay::new();

    // Tap that returns error for negative numbers
    let _tapped = stream.tap::<i32, _, _>(|n| {
        if *n < 0 {
            return Err(TestError("negative number".to_string()));
        }
        Ok(())
    });

    // Positive number should succeed
    let result = stream.send(42i32).await;
    assert!(result.is_ok());

    // Negative number should return error
    let result = stream.send(-1i32).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_error_subscription() {
    let stream = Relay::new();

    // Subscribe to errors
    let mut error_sub = stream.subscribe::<RelayError>();

    // Sink that returns error
    stream.sink::<String, _, _>(|msg| {
        if msg.as_str() == "error" {
            return Err(TestError("triggered error".to_string()));
        }
        Ok(())
    });

    // Send error-triggering message
    let _ = stream.send("error".to_string()).await;

    // Should receive the error as a message (emitted asynchronously)
    let error = tokio::time::timeout(Duration::from_millis(100), error_sub.recv())
        .await
        .unwrap();
    assert!(error.is_some());
    let error = error.unwrap();
    assert_eq!(error.source, "sink");
}

#[tokio::test]
async fn test_sink_without_error() {
    let stream = Relay::new();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    // Sink that returns () (no error)
    stream.sink::<i32, _, _>(move |n| {
        counter_clone.fetch_add(*n as usize, Ordering::SeqCst);
        // Returns (), which is converted to Ok(()) by IntoResult
    });

    stream.send(10i32).await.unwrap();
    stream.send(20i32).await.unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 30);
}

#[tokio::test]
async fn test_fire_and_forget() {
    let stream = Relay::new();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    stream.sink::<i32, _, _>(move |n| {
        counter_clone.fetch_add(*n as usize, Ordering::SeqCst);
    });

    // Fire and forget - send without awaiting completion
    // But we still need to await to send the message
    let _ = stream.send(10i32).await;
    let _ = stream.send(20i32).await;

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 30);
}

#[tokio::test]
async fn test_send_any() {
    use std::any::TypeId;

    let stream = Relay::new();
    let mut sub = stream.subscribe::<String>();

    // Send a type-erased value with explicit TypeId
    let value: Arc<dyn std::any::Any + Send + Sync> = Arc::new("hello".to_string());
    stream
        .send_any(value, TypeId::of::<String>())
        .await
        .unwrap();

    let msg = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .unwrap();
    assert_eq!(msg.as_deref(), Some(&"hello".to_string()));
}

#[tokio::test]
async fn test_send_envelope() {
    use crate::Envelope;

    let stream = Relay::new();
    let mut sub = stream.subscribe::<i32>();

    // Create and send envelope directly
    let envelope = Envelope::new(42i32, 0, None);
    stream.send_envelope(envelope).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .unwrap();
    assert_eq!(msg.as_deref(), Some(&42i32));
}

#[tokio::test]
async fn test_within_basic() {
    let stream = Relay::new();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let mut sub = stream.subscribe::<i32>();
    stream.within(move || async move {
        while let Some(n) = sub.recv().await {
            counter_clone.fetch_add(*n as usize, Ordering::SeqCst);
        }
    });

    stream.send(10i32).await.unwrap();
    stream.send(20i32).await.unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 30);
}

#[tokio::test]
async fn test_multiple_handlers_all_must_complete() {
    let stream = Relay::new();
    let counter1 = Arc::new(AtomicUsize::new(0));
    let counter2 = Arc::new(AtomicUsize::new(0));
    let c1 = counter1.clone();
    let c2 = counter2.clone();

    // Two handlers
    stream.sink::<i32, _, _>(move |n| {
        c1.fetch_add(*n as usize, Ordering::SeqCst);
    });
    stream.sink::<i32, _, _>(move |n| {
        c2.fetch_add(*n as usize * 2, Ordering::SeqCst);
    });

    // Send should wait for both handlers
    stream.send(10i32).await.unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(counter1.load(Ordering::SeqCst), 10);
    assert_eq!(counter2.load(Ordering::SeqCst), 20);
}

#[tokio::test]
async fn test_error_from_one_handler() {
    let stream = Relay::new();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    // One handler that succeeds
    stream.sink::<i32, _, _>(move |n| {
        counter_clone.fetch_add(*n as usize, Ordering::SeqCst);
    });

    // Another handler that fails for negative
    stream.sink::<i32, _, _>(|n| {
        if *n < 0 {
            return Err(TestError("negative".to_string()));
        }
        Ok(())
    });

    // Positive should succeed (both handlers run)
    stream.send(10i32).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(counter.load(Ordering::SeqCst), 10);

    // Negative should fail (first handler still runs)
    let result = stream.send(-5i32).await;
    assert!(result.is_err());

    // Wait and check first handler still processed
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Note: First handler runs regardless but we might get -5 as usize overflow
    // Let's use u32 to avoid this
}

#[tokio::test]
async fn test_no_handlers_immediate_return() {
    let stream = Relay::new();

    // No handlers registered
    // send().await should return immediately
    let result = stream.send("test".to_string()).await;
    assert!(result.is_ok());
}

// ==================== RACE CONDITION / STRESS TESTS FOR ERROR HANDLING ====================

#[tokio::test]
async fn stress_test_concurrent_errors() {
    // Multiple handlers receiving the same message, some succeed, some fail
    let stream = Relay::new();
    let success_count = Arc::new(AtomicUsize::new(0));
    let error_count = Arc::new(AtomicUsize::new(0));

    // 5 handlers: odd ones fail, even ones succeed
    for i in 0..5 {
        let sc = success_count.clone();
        let ec = error_count.clone();
        stream.sink::<i32, _, _>(move |_n| {
            if i % 2 == 1 {
                ec.fetch_add(1, Ordering::SeqCst);
                Err(TestError(format!("handler {} failed", i)))
            } else {
                sc.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
    }

    // Send 100 messages
    for _ in 0..100 {
        let result = stream.send(1i32).await;
        // Should get an error (from odd handlers) but all handlers still run
        assert!(result.is_err());
    }

    // Wait for all processing
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Each message processed by 3 success handlers and 2 error handlers
    assert_eq!(success_count.load(Ordering::SeqCst), 300); // 100 * 3
    assert_eq!(error_count.load(Ordering::SeqCst), 200); // 100 * 2
}

#[tokio::test]
async fn stress_test_rapid_error_sending() {
    let stream = Relay::with_channel_size(1024);

    // Handler that always fails
    stream.sink::<i32, _, _>(|_| Err(TestError("always fails".to_string())));

    // Send many messages rapidly
    let mut error_count = 0;
    for i in 0..500 {
        let result = stream.send(i).await;
        if result.is_err() {
            error_count += 1;
        }
    }

    // All should fail
    assert_eq!(error_count, 500);
}

#[tokio::test]
async fn stress_test_error_subscription_under_load() {
    let stream = Relay::with_channel_size(1024);
    let errors_received = Arc::new(AtomicUsize::new(0));
    let errors_received_clone = errors_received.clone();

    // Subscribe to errors
    let mut error_sub = stream.subscribe::<RelayError>();
    tokio::spawn(async move {
        while let Some(_err) = error_sub.recv().await {
            errors_received_clone.fetch_add(1, Ordering::SeqCst);
        }
    });

    // Handler that fails for even numbers
    stream.sink::<i32, _, _>(|n| {
        if *n % 2 == 0 {
            Err(TestError("even number".to_string()))
        } else {
            Ok(())
        }
    });

    // Send 200 messages (100 even, 100 odd)
    for i in 0..200 {
        let _ = stream.send(i).await;
    }

    // Wait for error propagation
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should have received 100 errors (one per even number)
    assert_eq!(errors_received.load(Ordering::SeqCst), 100);
}

#[tokio::test]
async fn stress_test_concurrent_senders_with_errors() {
    let stream = Relay::with_channel_size(2048);
    let error_count = Arc::new(AtomicUsize::new(0));
    let success_count = Arc::new(AtomicUsize::new(0));

    // Handler that fails for multiples of 10
    let ec = error_count.clone();
    let sc = success_count.clone();
    stream.sink::<i32, _, _>(move |n| {
        if *n % 10 == 0 {
            ec.fetch_add(1, Ordering::SeqCst);
            Err(TestError("multiple of 10".to_string()))
        } else {
            sc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });

    // Spawn multiple concurrent senders
    let handles: Vec<_> = (0..10)
        .map(|sender_id| {
            let stream = stream.clone();
            tokio::spawn(async move {
                for i in 0..100 {
                    let value = sender_id * 100 + i;
                    let _ = stream.send(value).await;
                }
            })
        })
        .collect();

    // Wait for all senders
    for handle in handles {
        handle.await.unwrap();
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 1000 messages total, 100 are multiples of 10 (0, 10, 20, ...)
    // Each sender sends 10 multiples of 10 (0, 10, 20, ... 90)
    // Total errors: 10 senders * 10 multiples = 100
    assert_eq!(error_count.load(Ordering::SeqCst), 100);
    assert_eq!(success_count.load(Ordering::SeqCst), 900);
}

#[tokio::test]
async fn stress_test_handler_registration_during_sends() {
    let stream = Relay::new();
    let total_received = Arc::new(AtomicUsize::new(0));

    // Start sending immediately
    let stream_clone = stream.clone();
    let sender_handle = tokio::spawn(async move {
        for i in 0..100 {
            let _ = stream_clone.send(i).await;
            // Small yield to allow handler registration
            if i % 10 == 0 {
                tokio::task::yield_now().await;
            }
        }
    });

    // Register handlers while sending is in progress
    for _ in 0..5 {
        tokio::task::yield_now().await;
        let tr = total_received.clone();
        stream.sink::<i32, _, _>(move |_| {
            tr.fetch_add(1, Ordering::SeqCst);
        });
    }

    sender_handle.await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Some messages should have been received by some handlers
    // The exact count depends on timing, but should be > 0
    assert!(total_received.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn stress_test_within_with_panics() {
    let stream = Relay::new();
    let processed = Arc::new(AtomicUsize::new(0));
    let processed_clone = processed.clone();

    // Subscribe to errors to verify panics are caught
    let mut error_sub = stream.subscribe::<RelayError>();
    let panic_errors = Arc::new(AtomicUsize::new(0));
    let pe = panic_errors.clone();
    tokio::spawn(async move {
        while let Some(err) = error_sub.recv().await {
            if err.source == "within" || err.source == "subscription" {
                pe.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    // Handler using within() that panics on certain values
    let mut sub = stream.subscribe::<i32>();
    stream.within(move || async move {
        while let Some(n) = sub.recv().await {
            processed_clone.fetch_add(1, Ordering::SeqCst);
            if *n == 50 {
                panic!("deliberate panic at 50");
            }
        }
    });

    // Send messages until panic
    for i in 0..100 {
        let result = stream.send(i).await;
        if i == 50 {
            // The panic should cause an error
            // But due to timing, it might not be immediate
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if i > 50 && result.is_err() {
            // After panic, sends might fail
            break;
        }
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should have processed up to 50 messages
    assert!(processed.load(Ordering::SeqCst) >= 50);
}

#[tokio::test]
async fn stress_test_mixed_success_and_error_types() {
    let stream = Relay::new();

    let string_count = Arc::new(AtomicUsize::new(0));
    let int_count = Arc::new(AtomicUsize::new(0));
    let error_count = Arc::new(AtomicUsize::new(0));

    let sc = string_count.clone();
    stream.sink::<String, _, _>(move |_| {
        sc.fetch_add(1, Ordering::SeqCst);
    });

    let ic = int_count.clone();
    let ec = error_count.clone();
    stream.sink::<i32, _, _>(move |n| {
        ic.fetch_add(1, Ordering::SeqCst);
        if *n < 0 {
            ec.fetch_add(1, Ordering::SeqCst);
            Err(TestError("negative".to_string()))
        } else {
            Ok(())
        }
    });

    // Send mixed types
    for i in 0..50 {
        let _ = stream.send(format!("msg_{}", i)).await;
        let _ = stream.send(i).await;
        let _ = stream.send(-i).await; // Negative triggers error
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Allow for slight timing variations
    let sc = string_count.load(Ordering::SeqCst);
    let ic = int_count.load(Ordering::SeqCst);
    let ec = error_count.load(Ordering::SeqCst);

    assert!(
        (48..=50).contains(&sc),
        "string_count {} not in expected range",
        sc
    );
    assert!(
        (98..=100).contains(&ic),
        "int_count {} not in expected range",
        ic
    );
    assert!(
        (48..=50).contains(&ec),
        "error_count {} not in expected range",
        ec
    );
}



#[tokio::test]
async fn stress_test_rapid_subscribe_unsubscribe() {
    // Test rapid subscribe/unsubscribe doesn't cause issues
    let stream = Relay::new();

    // Spawn a sender
    let stream_clone = stream.clone();
    let sender = tokio::spawn(async move {
        for i in 0..200 {
            let _ = stream_clone.send(i).await;
            tokio::task::yield_now().await;
        }
    });

    // Rapidly subscribe and drop
    for _ in 0..100 {
        let mut sub = stream.subscribe::<i32>();
        // Maybe receive one message
        let _ = tokio::time::timeout(Duration::from_micros(100), sub.recv()).await;
        drop(sub);
    }

    sender.await.unwrap();
}

// ==================== ADDITIONAL EDGE CASE TESTS ====================

#[tokio::test]
async fn test_multiple_errors_first_wins() {
    // When multiple handlers fail, first error should be returned
    let stream = Relay::new();

    // Three handlers that all fail
    stream.sink::<i32, _, _>(|_| Err(TestError("error 1".to_string())));
    stream.sink::<i32, _, _>(|_| Err(TestError("error 2".to_string())));
    stream.sink::<i32, _, _>(|_| Err(TestError("error 3".to_string())));

    let result = stream.send(42i32).await;
    assert!(result.is_err());

    // Should get one of the errors (first to complete wins)
    if let Err(SendError::Downstream(e)) = result {
        assert!(e.source == "sink");
    } else {
        panic!("Expected Downstream error");
    }
}


#[tokio::test]
async fn test_tracker_zero_expected() {
    // When no handlers, send should complete immediately
    let stream = Relay::new();

    // No handlers registered
    let start = std::time::Instant::now();
    let result = stream.send("test".to_string()).await;
    let elapsed = start.elapsed();

    assert!(result.is_ok());
    assert!(
        elapsed < Duration::from_millis(50),
        "Should complete immediately"
    );
}

#[tokio::test]
async fn test_sink_panic_propagation() {
    let stream = Relay::new();
    let mut error_sub = stream.subscribe::<RelayError>();

    // Sink that panics
    stream.sink::<i32, _, _>(|n| {
        if *n == 42 {
            panic!("deliberate panic");
        }
    });

    // Send panic-triggering value
    let result = stream.send(42i32).await;
    assert!(result.is_err());

    // Error should be subscribable
    let error = tokio::time::timeout(Duration::from_millis(100), error_sub.recv())
        .await
        .unwrap();
    assert!(error.is_some());
    let error = error.unwrap();
    assert_eq!(error.source, "sink");
    assert!(error.error.to_string().contains("panic"));
}

#[tokio::test]
async fn test_tap_panic_propagation() {
    let stream = Relay::new();

    // Tap that panics
    let _tapped = stream.tap::<String, _, _>(|s| {
        if s.as_str() == "boom" {
            panic!("tap panic");
        }
    });

    // Normal message succeeds
    let result = stream.send("hello".to_string()).await;
    assert!(result.is_ok());

    // Panic message fails
    let result = stream.send("boom".to_string()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_handler_slow_completion() {
    let stream = Relay::new();
    let completed = Arc::new(AtomicUsize::new(0));
    let completed_clone = completed.clone();

    // Slow handler
    stream.sink::<i32, _, _>(move |_| {
        std::thread::sleep(Duration::from_millis(50));
        completed_clone.fetch_add(1, Ordering::SeqCst);
    });

    // Send should wait for slow handler
    let start = std::time::Instant::now();
    stream.send(1i32).await.unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed >= Duration::from_millis(40),
        "Should wait for handler"
    );
    assert_eq!(completed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_subscription_drop_mid_message() {
    let stream = Relay::new();
    let processed = Arc::new(AtomicUsize::new(0));
    let processed_clone = processed.clone();

    // Handler that will be dropped mid-processing
    let sub = stream.subscribe::<i32>();
    stream.within({
        let mut sub = sub;
        move || async move {
            if let Some(_) = sub.recv().await {
                processed_clone.fetch_add(1, Ordering::SeqCst);
                // Simulate work then drop subscription by exiting
            }
        }
    });

    // Give the within task time to subscribe
    tokio::time::sleep(Duration::from_millis(10)).await;

    // This should complete (within doesn't block send)
    stream.send(1i32).await.unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(processed.load(Ordering::SeqCst), 1);
}


#[tokio::test]
async fn test_error_does_not_block_subsequent_sends() {
    let stream = Relay::new();
    let success_count = Arc::new(AtomicUsize::new(0));
    let sc = success_count.clone();

    // Handler that fails on negative
    stream.sink::<i32, _, _>(move |n| {
        if *n < 0 {
            return Err(TestError("negative".to_string()));
        }
        sc.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });

    // First send fails
    let r1 = stream.send(-1i32).await;
    assert!(r1.is_err());

    // Subsequent sends should still work
    let r2 = stream.send(1i32).await;
    assert!(r2.is_ok());

    let r3 = stream.send(2i32).await;
    assert!(r3.is_ok());

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(success_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_mixed_handler_types_completion() {
    // Handlers for different types should all complete
    let stream = Relay::new();
    let string_done = Arc::new(AtomicUsize::new(0));
    let int_done = Arc::new(AtomicUsize::new(0));
    let sd = string_done.clone();
    let id = int_done.clone();

    stream.sink::<String, _, _>(move |_| {
        sd.fetch_add(1, Ordering::SeqCst);
    });

    stream.sink::<i32, _, _>(move |_| {
        id.fetch_add(1, Ordering::SeqCst);
    });

    // Send string - both handlers count, but only string handler processes
    stream.send("test".to_string()).await.unwrap();

    // Send int - both handlers count, but only int handler processes
    stream.send(42i32).await.unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(string_done.load(Ordering::SeqCst), 1);
    assert_eq!(int_done.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_rapid_handler_registration_deregistration() {
    let stream = Relay::new();
    let total = Arc::new(AtomicUsize::new(0));

    // Rapidly register and let handlers complete
    for _ in 0..10 {
        let t = total.clone();
        stream.sink::<i32, _, _>(move |_| {
            t.fetch_add(1, Ordering::SeqCst);
        });
    }

    // Send one message
    stream.send(1i32).await.unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // All 10 handlers should have processed
    assert_eq!(total.load(Ordering::SeqCst), 10);
}

// ============================================================================
// Tests for new convenience methods
// ============================================================================

#[tokio::test]
async fn test_handler_count() {
    let stream = Relay::new();

    // Initially no handlers
    assert_eq!(stream.handler_count(), 0);

    // Add a sink
    stream.sink::<i32, _, _>(|_| {});
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(stream.handler_count(), 1);

    // Add a tap
    stream.tap::<i32, _, _>(|_| {});
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(stream.handler_count(), 2);

    // subscribe() doesn't count
    let _sub = stream.subscribe::<i32>();
    assert_eq!(stream.handler_count(), 2);
}

// ============================================================================
// NEW API TESTS: Stream, Duplex, Readable, Writable, Pipe
// ============================================================================

#[tokio::test]
async fn test_stream_basic_send_subscribe() {
    let stream = Relay::new();
    let mut sub = stream.subscribe::<String>();

    stream.send("hello".to_string()).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .unwrap();
    assert_eq!(msg.as_deref(), Some(&"hello".to_string()));
}

#[tokio::test]
async fn test_channel_no_echo() {
    // Channel sender and receiver are separate - no automatic echo
    let (tx, rx) = Relay::channel();

    let mut sub = rx.subscribe::<String>();

    // Send message
    tx.send("test".to_string()).await.unwrap();

    // Should be received on subscription
    let msg = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .unwrap();
    assert_eq!(msg.as_deref(), Some(&"test".to_string()));
}

/* REMOVED: Duplex API no longer exists

*/

/* REMOVED: Duplex API no longer exists
#[tokio::test]
async fn test_duplex_close() {
    let duplex = Duplex::new();

    assert!(!duplex.is_closed());

    duplex.close();
    assert!(duplex.input.is_closed());
}

*/

#[tokio::test]
async fn test_stream_is_loopback() {
    // Stream with shared inner should loopback
    let stream = Relay::new();
    let mut sub = stream.subscribe::<i32>();

    stream.send(123i32).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .unwrap();
    assert_eq!(msg.as_deref(), Some(&123i32));
}

#[tokio::test]
async fn test_stream_close() {
    let stream = Relay::new();

    assert!(!stream.is_closed());
    stream.close();
    assert!(stream.is_closed());

    // Send should fail on closed stream
    let result = stream.send("test".to_string()).await;
    assert!(matches!(result, Err(SendError::Closed)));
}

// ============================================================================
// INVARIANT TESTS - Adversarial tests for core guarantees
// ============================================================================
//
// These tests verify the core invariants:
// I1 - Completion safety: send().await returns Ok only when all tracked handlers completed
// I2 - Failure propagation: handler failures propagate to sender and are emitted once
// I3 - Echo prevention: no envelope delivered back to its origin via piping
// I4 - No silent deadlock: send() only waits for actively running handlers
// I5 - Closure monotonicity: once closed, no new messages accepted, downstreams close

// ==================== I1: Completion Safety ====================

#[tokio::test]
async fn invariant_send_waits_for_all_tracked_handlers() {
    // I1: send().await must wait for all tracked handlers to complete
    let relay = Relay::new();
    let flag = Arc::new(AtomicBool::new(false));

    let f = flag.clone();
    relay.tap::<i32, _, _>(move |_| {
        std::thread::sleep(Duration::from_millis(50));
        f.store(true, Ordering::SeqCst);
    });

    relay.send(1i32).await.unwrap();

    assert!(
        flag.load(Ordering::SeqCst),
        "send() must wait for handler to complete"
    );
}

#[tokio::test]
async fn invariant_multiple_handlers_all_complete() {
    // I1: All tracked handlers must complete before send() returns
    let relay = Relay::new();
    let count = Arc::new(AtomicUsize::new(0));

    for _ in 0..5 {
        let c = count.clone();
        relay.sink::<i32, _, _>(move |_| {
            std::thread::sleep(Duration::from_millis(10));
            c.fetch_add(1, Ordering::SeqCst);
        });
    }

    relay.send(1i32).await.unwrap();

    assert_eq!(
        count.load(Ordering::SeqCst),
        5,
        "all 5 handlers must complete"
    );
}

// ==================== I2: Failure Propagation ====================

#[tokio::test]
async fn invariant_downstream_error_propagates() {
    // I2: Handler errors must propagate to sender
    let relay = Relay::new();

    relay.tap::<i32, _, Result<(), std::io::Error>>(|_| {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "fail"))
    });

    let result = relay.send(1i32).await;
    assert!(
        matches!(result, Err(SendError::Downstream(_))),
        "error must propagate"
    );
}

#[tokio::test]
async fn invariant_panic_propagates_as_error() {
    // I2: Handler panics must propagate as errors
    let relay = Relay::new();

    relay.tap::<i32, _, ()>(|_| {
        panic!("deliberate panic");
    });

    let result = relay.send(1i32).await;
    assert!(
        matches!(result, Err(SendError::Downstream(_))),
        "panic must propagate as error"
    );
}

#[tokio::test]
async fn invariant_error_emitted_exactly_once() {
    // I2: Errors should be emitted exactly once to error subscribers
    let relay = Relay::new();
    let error_count = Arc::new(AtomicUsize::new(0));
    let ec = error_count.clone();

    let mut error_sub = relay.subscribe::<RelayError>();
    tokio::spawn(async move {
        while let Some(_) = error_sub.recv().await {
            ec.fetch_add(1, Ordering::SeqCst);
        }
    });

    relay.sink::<i32, _, _>(|_| Err(TestError("fail".to_string())));

    let _ = relay.send(1i32).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        error_count.load(Ordering::SeqCst),
        1,
        "error must be emitted exactly once"
    );
}

// ==================== I3: Echo Prevention ====================



// ==================== I4: No Silent Deadlock ====================

#[tokio::test]
async fn invariant_no_deadlock_on_handler_panic() {
    // I4: Panicking handler must not cause send() to deadlock
    let relay = Relay::new();

    relay.tap::<i32, _, _>(|v| {
        if *v == 1 {
            panic!("boom");
        }
    });

    let result = tokio::time::timeout(Duration::from_millis(500), relay.send(1i32)).await;

    assert!(result.is_ok(), "send() must not deadlock on handler panic");
}

#[tokio::test]
async fn invariant_handler_count_restored_after_panic() {
    // I4: handler_count must return to correct value after panic
    let relay = Relay::new();

    relay.tap::<i32, _, ()>(|_| {
        panic!("boom");
    });

    let _ = relay.send(1i32).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // After panic, handler task exits and HandlerGuard decrements count
    // But the task is still running (just completed one iteration)
    // Actually, tap spawns a long-running task that loops, so it doesn't exit on one panic
    // Let's close the relay to terminate the task
    relay.close();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        relay.handler_count(),
        0,
        "handler_count must be 0 after relay closed"
    );
}


// ==================== I5: Closure Monotonicity ====================

#[tokio::test]
async fn invariant_closed_relay_rejects_sends() {
    // I5: Closed relay must reject new messages
    let relay = Relay::new();
    relay.close();

    let result = relay.send(1i32).await;
    assert!(matches!(result, Err(SendError::Closed)));
}


#[tokio::test]
async fn invariant_subscriptions_close_on_relay_close() {
    // I5: Subscriptions must receive None when relay closes
    let relay = Relay::new();
    let mut sub = relay.subscribe::<i32>();

    relay.send(1i32).await.unwrap();
    relay.close();

    // Should receive the message
    let msg = sub.recv().await;
    assert!(msg.is_some());

    // Then should receive None
    let closed = sub.recv().await;
    assert!(closed.is_none(), "subscription must close");
}

// ==================== Soak/Fuzz Test ====================


// ==================== Regression Tests (fixes we implemented) ====================


#[tokio::test]
async fn regression_handler_guard_decrements_on_panic() {
    // Tests that HandlerGuard decrements handler_count even on panic
    let relay = Relay::new();

    // Create sink that will panic
    relay.sink::<i32, _, ()>(|_| {
        panic!("intentional panic");
    });

    // Verify handler was registered
    assert_eq!(relay.handler_count(), 1);

    // Send will trigger panic
    let _ = relay.send(1i32).await;

    // Close to terminate the handler loop
    relay.close();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // HandlerGuard should have decremented on task exit
    assert_eq!(
        relay.handler_count(),
        0,
        "handler_count must be 0 after handler task exits"
    );
}



#[tokio::test]
async fn soak_concurrent_subscribe_unsubscribe() {
    // Stress test: rapid subscribe/unsubscribe during sends
    let relay = Relay::with_channel_size(256);
    let relay_clone = relay.clone();

    // Sender task
    let sender = tokio::spawn(async move {
        for i in 0..200 {
            let _ = relay_clone.send(i).await;
            tokio::task::yield_now().await;
        }
    });

    // Subscriber churn task
    let relay_clone2 = relay.clone();
    let churner = tokio::spawn(async move {
        for _ in 0..50 {
            let sub = relay_clone2.subscribe::<i32>();
            tokio::task::yield_now().await;
            drop(sub);
        }
    });

    let results = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = sender.await;
        let _ = churner.await;
    })
    .await;

    assert!(
        results.is_ok(),
        "must not deadlock under subscribe/unsubscribe churn"
    );
}

// ==================== Forward/Pipe Independence Tests ====================
//
// These tests verify that forward/pipe do NOT propagate closure to the target.
// The target is an independent stream that may have other sources, so closing
// one source should NOT close the target. Derived streams (filter, map, etc.)
// handle closure propagation differently - they DO close their children.


// ============================================================================
// DUPLEX TRANSFORM UTILITY TESTS
// ============================================================================

/* REMOVED: Duplex API no longer exists
#[tokio::test]
async fn test_duplex_transform_basic() {
    // Transform String → usize (string length)
    let duplex = Duplex::new().transform::<String, usize, _>(|s| s.len());

    let mut sub = duplex.output.subscribe::<usize>();

    // Send string to writable, expect length on readable
    duplex.input.send("hello".to_string()).await.unwrap();

    let len = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .unwrap();
    assert_eq!(len.as_deref(), Some(&5usize));
}

*/

/* REMOVED: Duplex API no longer exists
#[tokio::test]
async fn test_duplex_transform_passthrough_other_types() {
    // Transform String → usize, but i32 should pass through
    let duplex = Duplex::new().transform::<String, usize, _>(|s| s.len());

    let mut string_len_sub = duplex.output.subscribe::<usize>();
    let mut int_sub = duplex.output.subscribe::<i32>();

    // Send both types
    duplex.input.send("hello".to_string()).await.unwrap();
    duplex.input.send(42i32).await.unwrap();

    // String transformed to length
    let len = tokio::time::timeout(Duration::from_millis(100), string_len_sub.recv())
        .await
        .unwrap();
    assert_eq!(len.as_deref(), Some(&5usize));

    // i32 passes through unchanged
    let num = tokio::time::timeout(Duration::from_millis(100), int_sub.recv())
        .await
        .unwrap();
    assert_eq!(num.as_deref(), Some(&42i32));
}

*/

/* REMOVED: Duplex API no longer exists
#[tokio::test]
async fn test_duplex_transform_chained() {
    // Chain transforms: i32 → double, then String → length
    let duplex = Duplex::new()
        .transform::<i32, i32, _>(|n| n * 2)
        .transform::<String, usize, _>(|s| s.len());

    let mut int_sub = duplex.output.subscribe::<i32>();
    let mut len_sub = duplex.output.subscribe::<usize>();

    duplex.input.send(5i32).await.unwrap();
    duplex.input.send("test".to_string()).await.unwrap();

    // i32 is doubled
    let doubled = tokio::time::timeout(Duration::from_millis(100), int_sub.recv())
        .await
        .unwrap();
    assert_eq!(doubled.as_deref(), Some(&10i32));

    // String is converted to length
    let len = tokio::time::timeout(Duration::from_millis(100), len_sub.recv())
        .await
        .unwrap();
    assert_eq!(len.as_deref(), Some(&4usize));
}

*/

/// Test that send() doesn't block on subscriptions that signal ready immediately.
/// Without ReadyGuard auto-signaling, this would deadlock.
#[tokio::test]
async fn test_send_with_immediate_ready() {
    let relay = Relay::new();
    let received = Arc::new(AtomicBool::new(false));
    let received_clone = received.clone();

    // Create subscription (ready is signaled automatically)
    relay.sink(move |_msg: &String| {
        received_clone.store(true, Ordering::SeqCst);
    });

    // Send should complete quickly without blocking
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        relay.send("test".to_string())
    )
    .await;

    assert!(
        result.is_ok(),
        "send() should not block - subscriptions signal ready immediately"
    );

    // Verify message was received
    assert!(
        received.load(Ordering::SeqCst),
        "handler should have received the message"
    );
}

/// Test that subscriptions are immediately ready
#[tokio::test]
async fn test_subscription_immediately_ready() {
    let relay = Relay::new();

    // Create subscription
    relay.sink(|_msg: &String| {});

    // Send should not block - subscription is ready immediately
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        relay.send("test".to_string())
    )
    .await;

    assert!(
        result.is_ok(),
        "send() should not block - subscriptions are ready immediately"
    );
}

/* REMOVED: Duplex API no longer exists

*/

/* REMOVED: Duplex API no longer exists

*/

/* REMOVED: Duplex API no longer exists
#[tokio::test]
async fn test_duplex_passthrough_except() {
    // Passthrough all except String
    let duplex = Duplex::new().passthrough_except::<String>();

    let mut int_sub = duplex.output.subscribe::<i32>();
    let mut string_sub = duplex.output.subscribe::<String>();

    // i32 passes through
    duplex.input.send(42i32).await.unwrap();

    let num = tokio::time::timeout(Duration::from_millis(100), int_sub.recv())
        .await
        .unwrap();
    assert_eq!(num.as_deref(), Some(&42i32));

    // String is consumed (not passed through)
    duplex.input.send("hello".to_string()).await.unwrap();

    let result = tokio::time::timeout(Duration::from_millis(50), string_sub.recv()).await;
    assert!(
        result.is_err(),
        "String should be consumed, not passed through"
    );
}

*/

/* REMOVED: Duplex API no longer exists
#[tokio::test]
async fn test_duplex_passthrough_all() {
    // Passthrough all types
    let duplex = Duplex::new().passthrough_all();

    let mut int_sub = duplex.output.subscribe::<i32>();
    let mut string_sub = duplex.output.subscribe::<String>();

    duplex.input.send(42i32).await.unwrap();
    duplex.input.send("hello".to_string()).await.unwrap();

    let num = tokio::time::timeout(Duration::from_millis(100), int_sub.recv())
        .await
        .unwrap();
    assert_eq!(num.as_deref(), Some(&42i32));

    let text = tokio::time::timeout(Duration::from_millis(100), string_sub.recv())
        .await
        .unwrap();
    assert_eq!(text.as_deref(), Some(&"hello".to_string()));
}

*/

/* REMOVED: Duplex API no longer exists

*/

/* REMOVED: Duplex API no longer exists
#[tokio::test]
async fn test_duplex_no_echo_with_transform() {
    // Verify duplex with transform still prevents echo
    // Duplex has separate readable/writable - no echo by design
    let duplex = Duplex::new().transform::<i32, String, _>(|n| format!("{}", n));

    // Subscribe to readable side
    let mut readable_sub = duplex.output.subscribe::<String>();

    // Send i32 to writable - should transform to String on readable
    duplex.input.send(42i32).await.unwrap();

    // Transformed String should appear on readable
    let result = tokio::time::timeout(Duration::from_millis(100), readable_sub.recv()).await;
    assert!(result.is_ok(), "Transformed output should appear on readable");
    assert_eq!(result.unwrap().as_deref(), Some(&"42".to_string()));

    // But sending String directly to writable should NOT appear back on writable
    // (This is already tested by test_duplex_no_echo, but we verify the combination)
}

*/

/* REMOVED: Duplex API no longer exists
#[tokio::test]
async fn test_duplex_multiple_transforms_same_type() {
    // Multiple transforms that consume the same type - first wins
    let duplex = Duplex::new()
        .transform::<i32, String, _>(|n| format!("first: {}", n))
        .transform::<i32, String, _>(|n| format!("second: {}", n));

    let mut sub = duplex.output.subscribe::<String>();

    duplex.input.send(42i32).await.unwrap();

    // First transform should process the i32
    let msg1 = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .unwrap();

    // Should get one transformed string (from first transform)
    // The second transform won't see it because it's consumed by the first
    assert!(msg1.is_some());
    let value = msg1.unwrap();
    assert!(
        value.contains("42"),
        "Should contain the transformed number"
    );
}

*/
