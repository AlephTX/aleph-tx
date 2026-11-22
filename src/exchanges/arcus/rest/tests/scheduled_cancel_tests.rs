use super::*;

#[tokio::test]
async fn schedule_roundtrip_signs_scope_and_correlates_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let trading = local_client(&listener);
    let deadline = trading.timestamp().unwrap() / 1_000 + 60_000_000;
    let server = tokio::spawn(async move {
        for armed in [true, false] {
            let (socket, _) = listener.accept().await.unwrap();
            let (socket, line, headers, body) = read_request(socket).await;
            assert_eq!(
                line,
                format!("POST /v1/scheduleCancel?address={ADDRESS} HTTP/1.1\r\n")
            );
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["accountIndex"], 2);
            assert_eq!(value["marketId"], 88);
            assert_eq!(value["time"].as_u64(), armed.then_some(deadline));
            let message = format!(
                "{}scheduleCancel{}",
                headers["x-timestamp"],
                String::from_utf8(body).unwrap()
            );
            let signature =
                Signature::from_slice(&hex::decode(&headers["x-signature"]).unwrap()).unwrap();
            SigningKey::from_bytes(&KEY)
                .verifying_key()
                .verify(message.as_bytes(), &signature)
                .unwrap();
            respond(socket, "200 OK", json!({"address":ADDRESS,"accountIndex":2,"marketId":88,
                "time":armed.then_some(deadline),"status":if armed {"scheduled"} else {"disarmed"}})).await;
        }
    });
    assert_eq!(
        trading
            .schedule_cancel(Some(88), Some(deadline))
            .await
            .unwrap()
            .deadline_us(),
        Some(deadline)
    );
    assert_eq!(
        trading
            .schedule_cancel(Some(88), None)
            .await
            .unwrap()
            .deadline_us(),
        None
    );
    server.await.unwrap();
}

#[tokio::test]
async fn schedule_failures_are_not_order_acks_or_successful_renewals() {
    for (status, body, rejected) in [
        ("202 Accepted", json!({"status":"ACK"}), false),
        ("200 OK", json!({"status":"scheduled"}), false),
        (
            "200 OK",
            json!({"address":ADDRESS,"accountIndex":2,"marketId":89,"status":"disarmed"}),
            false,
        ),
        ("400 Bad Request", json!({}), true),
        ("401 Unauthorized", json!({}), true),
        ("403 Forbidden", json!({}), true),
        ("429 Too Many Requests", json!({}), true),
        ("503 Service Unavailable", json!({}), false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let trading = local_client(&listener);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (socket, _, _, _) = read_request(socket).await;
            respond(socket, status, body).await;
        });
        let error = trading.schedule_cancel(Some(88), None).await.unwrap_err();
        match error.downcast_ref::<ExecutionError>().unwrap() {
            ExecutionError::Rejected { .. } => assert!(rejected, "{status}"),
            ExecutionError::Uncertain { .. } => assert!(!rejected, "{status}"),
        }
        server.await.unwrap();
    }
}
