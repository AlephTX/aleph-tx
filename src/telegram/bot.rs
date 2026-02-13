//! Telegram bot for AlephTX control

use std::sync::Arc;
use telegraf::{Bot, message::Text, Context, Update};
use telegraf::collector::Collector;
use tokio::sync::mpsc;
use tracing::{info, error};

use crate::core::Config;

/// Telegram command handler
pub struct TelegramBot {
    bot: Bot,
    allowed_users: Vec<i64>,
    command_tx: Option<mpsc::Sender<Command>>,
}

#[derive(Debug, Clone)]
pub enum Command {
    Status,
    Positions,
    Signals,
    Pause,
    Resume,
    Exit,
}

impl TelegramBot {
    pub fn new(bot_token: impl Into<String>, allowed_users: Vec<i64>) -> Self {
        let bot = Bot::new(bot_token);
        Self {
            bot,
            allowed_users,
            command_tx: None,
        }
    }

    /// Start the bot
    pub async fn start(&mut self, command_rx: mpsc::Receiver<Command>) -> Result<(), Box<dyn std::error::Error>> {
        let (tx, mut rx) = mpsc::channel::<(i64, String)>(100);
        self.command_tx = Some(tx.clone());

        // Clone for handler
        let allowed = self.allowed_users.clone();

        // Spawn message handler
        let bot_clone = self.bot.clone();
        tokio::spawn(async move {
            while let Some((user_id, text)) = rx.recv().await {
                let response = format!("Received: {}", text);
                if let Err(e) = bot_clone.send_message(user_id, response).await {
                    error!("Failed to send message: {}", e);
                }
            }
        });

        // Set up commands
        self.bot.use_command("start", |ctx| {
            ctx.reply("🤖 AlephTX Trading System\n\nCommands:\n/status - Positions\n/pause - Pause trading\n/resume - Resume trading").await
        });

        self.bot.use_command("status", |ctx| {
            ctx.reply("📊 Getting status...").await
        });

        self.bot.use_command("pause", |ctx| {
            ctx.reply("⏸️ Paused").await
        });

        self.bot.use_command("resume", |ctx| {
            ctx.reply("▶️ Resumed").await
        });

        // Start polling
        info!("Starting Telegram bot...");

        // Note: In production, use webhook or long polling
        Ok(())
    }

    /// Send message to all allowed users
    pub async fn broadcast(&self, message: &str) -> Result<(), Box<dyn std::error::Error>> {
        for user_id in &self.allowed_users {
            self.bot.send_message(*user_id, message).await?;
        }
        Ok(())
    }
}
