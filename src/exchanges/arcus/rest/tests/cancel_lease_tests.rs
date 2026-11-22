use super::*;
use crate::exchanges::arcus::CancelLeasePolicy;
use std::{sync::Arc, time::Duration};

fn guard(address: &str) -> CancelLease {
    CancelLease::new(
        Account::new(address, 2).unwrap(),
        Some(88),
        CancelLeasePolicy {
            lifetime: Duration::from_secs(60),
            refresh_after: Duration::from_secs(10),
            stop_before_expiry: Duration::from_secs(5),
            max_clock_step: Duration::from_millis(100),
        },
    )
    .unwrap()
}

#[tokio::test]
async fn rest_renewal_confirms_protection_then_failure_closes_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let trading = local_client(&listener);
    let server = tokio::spawn(async move {
        let mut previous = 0;
        for success in [true, false] {
            let (socket, _) = listener.accept().await.unwrap();
            let (socket, _, _, body) = read_request(socket).await;
            let value: Value = serde_json::from_slice(&body).unwrap();
            let deadline = value["time"].as_u64().unwrap();
            assert!(deadline > previous);
            previous = deadline;
            assert_eq!(value["marketId"], 88);
            if success {
                respond(
                    socket,
                    "200 OK",
                    json!({"address":ADDRESS,"accountIndex":2,
                    "marketId":88,"time":deadline,"status":"scheduled"}),
                )
                .await;
            } else {
                respond(socket, "503 Service Unavailable", json!({})).await;
            }
        }
    });
    let mut lease = guard(ADDRESS);
    trading.renew_cancel_lease(&mut lease).await.unwrap();
    assert!(lease.allows_new_exposure(Instant::now()));
    assert!(trading.renew_cancel_lease(&mut lease).await.is_err());
    assert!(!lease.allows_new_exposure(Instant::now()));
    server.await.unwrap();
}

#[tokio::test]
async fn wall_clock_rollback_during_rest_renewal_requires_reconciliation() {
    const SENT: u64 = 1_800_000_000_000_000_000;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut trading = local_client(&listener);
    let wall = Arc::new(AtomicU64::new(SENT));
    let clock = Arc::clone(&wall);
    trading.wall_clock_override = Some(Box::new(move || clock.load(Ordering::SeqCst)));
    trading.last_timestamp.store(SENT + 10, Ordering::Relaxed);
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (socket, _, headers, body) = read_request(socket).await;
        assert_eq!(headers["x-timestamp"].parse::<u64>().unwrap(), SENT + 11);
        let value: Value = serde_json::from_slice(&body).unwrap();
        let deadline = value["time"].as_u64().unwrap();
        assert_eq!(deadline, SENT / 1_000 + 60_000_000);
        wall.store(SENT - 1_000_000_000, Ordering::SeqCst);
        respond(
            socket,
            "200 OK",
            json!({"address":ADDRESS,"accountIndex":2,
            "marketId":88,"time":deadline,"status":"scheduled"}),
        )
        .await;
    });
    let mut lease = guard(ADDRESS);
    let error = trading.renew_cancel_lease(&mut lease).await.unwrap_err();
    assert_eq!(error.to_string(), "Arcus wall clock moved backwards");
    assert!(lease.requires_reconciliation(Instant::now()));
    assert!(!lease.allows_new_exposure(Instant::now()));
    server.await.unwrap();
}

#[tokio::test]
async fn wrong_account_is_rejected_before_any_network_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let trading = local_client(&listener);
    let mut lease = guard("0x2222222222222222222222222222222222222222");
    assert!(trading.renew_cancel_lease(&mut lease).await.is_err());
    assert!(!lease.allows_new_exposure(Instant::now()));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dropping_in_flight_renewal_requires_reconciliation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let trading = local_client(&listener);
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (socket, _, _, _) = read_request(socket).await;
        seen_tx.send(()).unwrap();
        release_rx.await.unwrap();
        drop(socket);
    });
    let mut lease = guard(ADDRESS);
    {
        let renewal = trading.renew_cancel_lease(&mut lease);
        tokio::pin!(renewal);
        tokio::select! {
            _ = seen_rx => {},
            _ = &mut renewal => panic!("renewal completed without a response"),
        }
        // Equivalent to a timeout cancelling the polled future.
    }
    assert!(lease.requires_reconciliation(Instant::now()));
    assert!(!lease.allows_new_exposure(Instant::now()));
    assert!(trading.renew_cancel_lease(&mut lease).await.is_err());
    release_tx.send(()).unwrap();
    server.await.unwrap();
}
