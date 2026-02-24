use crate::types::Ticker;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;

/// Raw ticker as received from the Go feeder over IPC.
#[derive(Debug, Deserialize)]
struct IpcTicker {
    exchange: String,
    symbol: String,
    bid: String,
    ask: String,
    last: String,
    volume_24h: String,
    ts: i64,
}

#[derive(Debug, Deserialize)]
struct IpcMessage {
    #[serde(rename = "type")]
    msg_type: String,
    payload: serde_json::Value,
}

/// Listen on a Unix socket and yield normalised Tickers.
pub async fn listen(socket_path: String, tx: flume::Sender<Ticker>) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!("IPC listener: {}", socket_path);
    loop {
        let (stream, _) = listener.accept().await?;
        let tx = tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stream);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(msg) = serde_json::from_str::<IpcMessage>(&line) {
                    if msg.msg_type == "ticker" {
                        if let Ok(raw) = serde_json::from_value::<IpcTicker>(msg.payload) {
                            let ticker = Ticker {
                                symbol: crate::types::Symbol::new(&raw.symbol),
                                bid: Decimal::from_str(&raw.bid).unwrap_or(Decimal::ZERO),
                                ask: Decimal::from_str(&raw.ask).unwrap_or(Decimal::ZERO),
                                last: Decimal::from_str(&raw.last).unwrap_or(Decimal::ZERO),
                                volume_24h: Decimal::from_str(&raw.volume_24h).unwrap_or(Decimal::ZERO),
                                timestamp: raw.ts as u64,
                            };
                            let _ = tx.send_async(ticker).await;
                        }
                    }
                }
            }
        });
    }
}
