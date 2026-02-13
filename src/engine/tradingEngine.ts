import { MarketDataFeed, Ticker } from '../feeds/marketDataFeed';
import { Config } from '../config';
import { Order, Position, Signal } from '../types';

export class TradingEngine {
  private config: Config;
  private marketFeed: MarketDataFeed;
  private positions: Map<string, Position> = new Map();
  private signals: Signal[] = [];

  constructor(config: Config, marketFeed: MarketDataFeed) {
    this.config = config;
    this.marketFeed = marketFeed;
  }

  async initialize(): Promise<void> {
    console.log('⚙️ Initializing trading engine...');

    this.marketFeed.onTicker((ticker) => {
      this.processTick(ticker);
    });
  }

  private processTick(ticker: Ticker): void {
    // Simple RSI-based signal generation (placeholder)
    // Real implementation would store price history and calculate indicators

    const position = this.positions.get(ticker.symbol);
    if (!position) {
      // No position, look for entry signals
      // This is where strategy logic goes
    }

    // Emit signals for monitoring
    this.signals.push({
      symbol: ticker.symbol,
      type: 'PRICE_UPDATE',
      price: ticker.last,
      timestamp: Date.now(),
    });

    // Keep only recent signals
    if (this.signals.length > 100) {
      this.signals = this.signals.slice(-100);
    }
  }

  async openPosition(symbol: string, side: 'long' | 'short', size: number): Promise<Order> {
    const order: Order = {
      id: `order_${Date.now()}`,
      symbol,
      side,
      size,
      status: 'pending',
      timestamp: Date.now(),
    };

    console.log(`📝 Opening ${side} position: ${symbol} ${size}`);

    // Place order via exchange API (mock for now)
    order.status = 'filled';
    order.fillPrice = await this.getPrice(symbol);

    this.positions.set(symbol, {
      symbol,
      side,
      size,
      entryPrice: order.fillPrice!,
      timestamp: Date.now(),
    });

    return order;
  }

  async closePosition(symbol: string): Promise<Order | null> {
    const position = this.positions.get(symbol);
    if (!position) return null;

    const order: Order = {
      id: `order_${Date.now()}`,
      symbol,
      side: position.side === 'long' ? 'short' : 'long',
      size: position.size,
      status: 'pending',
      timestamp: Date.now(),
    };

    order.status = 'filled';
    order.fillPrice = await this.getPrice(symbol);

    this.positions.delete(symbol);
    return order;
  }

  private async getPrice(symbol: string): Promise<number> {
    // In real implementation, fetch from market feed or exchange API
    return 0;
  }

  getPositions(): Position[] {
    return Array.from(this.positions.values());
  }

  getSignals(): Signal[] {
    return this.signals.slice(-10);
  }
}
