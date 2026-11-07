use aleph_tx::exchanges::arcus::{
    Account, ArcusInfo, ArcusSigner, ArcusTrading, HistoryWindow, LimitOrder, Network, Side,
    TimeInForce,
};
use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::SigningKey;
use std::env;

pub(super) async fn run(network: &str, args: &[String]) -> Result<()> {
    let network = match network {
        "testnet" => Network::Testnet,
        "mainnet" => Network::Mainnet,
        _ => bail!("Arcus network must be testnet or mainnet"),
    };
    let info = ArcusInfo::new(network)?;
    match args {
        [command] if command == "markets" => super::print(&info.markets().await?),
        [command, market] if command == "bbo" => super::print(&info.bbo(market).await?),
        [command, order_id] if command == "order" => {
            super::print(&info.order(&account()?, order_id).await?)
        }
        [command] if command == "positions" => super::print(&info.positions(&account()?).await?),
        [command, limit @ ..]
            if matches!(command.as_str(), "open-orders" | "orders" | "fills")
                && limit.len() <= 1 =>
        {
            let limit = limit
                .first()
                .map(|value| value.parse::<u16>())
                .transpose()?;
            ensure!(
                limit.is_none_or(|value| (1..=1000).contains(&value)),
                "Arcus page limit must be 1 through 1000"
            );
            let window = HistoryWindow {
                limit,
                ..HistoryWindow::default()
            };
            let account = account()?;
            match command.as_str() {
                "open-orders" => super::print(&info.open_orders(&account, &window).await?),
                "orders" => super::print(&info.order_history(&account, &window).await?),
                _ => super::print(&info.fills(&account, &window).await?),
            }
        }
        [
            command,
            symbol,
            side,
            size,
            price,
            tif,
            client_id,
            options @ ..,
        ] if command == "place-limit" && (options.is_empty() || options == ["--reduce-only"]) => {
            let order = LimitOrder {
                side: match side.as_str() {
                    "buy" => Side::Buy,
                    "sell" => Side::Sell,
                    _ => bail!("Arcus order side must be buy or sell"),
                },
                quantity: size.parse().context("Invalid Arcus order size")?,
                price: price.parse().context("Invalid Arcus limit price")?,
                time_in_force: match tif.as_str() {
                    "alo" => TimeInForce::Alo,
                    "ioc" => TimeInForce::Ioc,
                    "fok" => TimeInForce::Fok,
                    "gtt" => TimeInForce::Gtt,
                    _ => bail!("Arcus time in force must be alo, ioc, fok or gtt"),
                },
                reduce_only: !options.is_empty(),
                client_id,
            };
            let market = info
                .markets()
                .await?
                .into_iter()
                .find(|market| market.market_display_name.eq_ignore_ascii_case(symbol))
                .with_context(|| format!("Unknown Arcus market {symbol}"))?;
            super::print(&trading(network)?.place_limit(&market, order).await?)
        }
        [command, market_id, id]
            if matches!(command.as_str(), "cancel-order" | "cancel-client") =>
        {
            let market_id = market_id.parse().context("Invalid Arcus market ID")?;
            let client = trading(network)?;
            let receipt = if command == "cancel-order" {
                client.cancel_by_order_id(market_id, id).await?
            } else {
                client.cancel_by_client_id(market_id, id).await?
            };
            super::print(&receipt)
        }
        [command, scope] if command == "cancel-all" => {
            let market_id = if scope == "all" {
                None
            } else {
                Some(scope.parse().context("Invalid Arcus market ID")?)
            };
            super::print(&trading(network)?.cancel_all(market_id).await?)
        }
        _ => bail!(
            "Arcus commands: markets | bbo MARKET | order ORDER_ID | open-orders [LIMIT] | orders [LIMIT] | fills [LIMIT] | positions | place-limit MARKET SIDE SIZE PRICE TIF CLIENT_ID [--reduce-only] | cancel-order MARKET_ID ORDER_ID | cancel-client MARKET_ID CLIENT_ID | cancel-all <MARKET_ID|all>"
        ),
    }
}

fn account_scope() -> Result<(String, u8)> {
    let address = env::var("ARCUS_ACCOUNT_ADDRESS").context("Set ARCUS_ACCOUNT_ADDRESS")?;
    let index = match env::var("ARCUS_ACCOUNT_INDEX") {
        Ok(value) => value.parse().context("Invalid ARCUS_ACCOUNT_INDEX")?,
        Err(env::VarError::NotPresent) => 0,
        Err(_) => bail!("Invalid ARCUS_ACCOUNT_INDEX"),
    };
    Ok((address, index))
}

fn account() -> Result<Account> {
    let (address, index) = account_scope()?;
    Account::new(&address, index)
}

fn trading(network: Network) -> Result<ArcusTrading> {
    let (address, index) = account_scope()?;
    let raw = env::var("ARCUS_API_PRIVATE_KEY").context("Set ARCUS_API_PRIVATE_KEY")?;
    let mut bytes = [0; 32];
    hex::decode_to_slice(raw.strip_prefix("0x").unwrap_or(&raw), &mut bytes)
        .map_err(|_| anyhow::anyhow!("ARCUS_API_PRIVATE_KEY must be a 32-byte hex signing key"))?;
    ArcusTrading::new(
        network,
        ArcusSigner::new(SigningKey::from_bytes(&bytes), &address, index)?,
    )
}
