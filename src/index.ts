// AlephTX - Core Trading System
// 🚀 Speed-first quantitative trading

import { MarketDataFeed } from './feeds/marketDataFeed';
import { TradingEngine } from './engine/tradingEngine';
import { TelegramController } from './controllers/telegramController';
import { Config } from './config';

async function main(): Promise<void> {
  console.log('🤖 AlephTX Trading System Starting...');

  const config = Config.load();
  const marketFeed = new MarketDataFeed(config);
  const tradingEngine = new TradingEngine(config, marketFeed);
  const telegram = new TelegramController(config, tradingEngine);

  await marketFeed.connect();
  await tradingEngine.initialize();
  await telegram.start();

  console.log('✅ AlephTX is running');

  // Graceful shutdown
  process.on('SIGINT', async () => {
    console.log('🛑 Shutting down...');
    await marketFeed.disconnect();
    await telegram.stop();
    process.exit(0);
  });
}

main().catch(console.error);
