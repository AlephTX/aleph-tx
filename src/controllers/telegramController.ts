import { Telegraf, Context } from 'telegraf';
import { Config } from '../config';
import { TradingEngine } from '../engine/tradingEngine';

export class TelegramController {
  private bot: Telegraf;
  private config: Config;
  private tradingEngine: TradingEngine;

  constructor(config: Config, tradingEngine: TradingEngine) {
    this.config = config;
    this.tradingEngine = tradingEngine;
    this.bot = new Telegraf(config.telegram.botToken);
    this.setupCommands();
  }

  private setupCommands(): void {
    this.bot.start((ctx) => {
      ctx.reply('🤖 AlephTX Trading System\n\nCommands:\n/status - Show positions\n/signals - Recent signals\n/ping - Health check');
    });

    this.bot.command('status', async (ctx) => {
      const positions = this.tradingEngine.getPositions();
      if (positions.length === 0) {
        ctx.reply('📭 No open positions');
        return;
      }

      const msg = positions
        .map((p) => `${p.symbol}: ${p.side.toUpperCase()} ${p.size} @ ${p.entryPrice}`)
        .join('\n');
      ctx.reply(`📊 Positions:\n${msg}`);
    });

    this.bot.command('signals', async (ctx) => {
      const signals = this.tradingEngine.getSignals();
      if (signals.length === 0) {
        ctx.reply('📭 No recent signals');
        return;
      }

      const msg = signals
        .reverse()
        .map((s) => `${s.type} ${s.symbol} @ ${s.price}`)
        .join('\n');
      ctx.reply(`📈 Recent Signals:\n${msg}`);
    });

    this.bot.command('ping', (ctx) => {
      ctx.reply('✅ AlephTX is alive');
    });

    this.bot.command('help', (ctx) => {
      ctx.reply(`
🤖 AlephTX Commands

/status - Show open positions
/signals - Recent trading signals  
/ping - Health check
/start - Welcome message
/help - This help
      `);
    });
  }

  async start(): Promise<void> {
    console.log('📱 Starting Telegram bot...');
    await this.bot.launch();
    console.log('✅ Telegram bot started');
  }

  async stop(): Promise<void> {
    this.bot.stop();
  }
}
