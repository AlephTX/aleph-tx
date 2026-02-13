import WebSocket from 'ws';
import { Config } from '../config';

export interface Ticker {
  symbol: string;
  bid: number;
  ask: number;
  last: number;
  volume: number;
  timestamp: number;
}

type TickerCallback = (ticker: Ticker) => void;

export class MarketDataFeed {
  private ws: WebSocket | null = null;
  private config: Config;
  private callbacks: TickerCallback[] = [];
  private reconnectTimer: NodeJS.Timeout | null = null;
  private isConnected = false;

  constructor(config: Config) {
    this.config = config;
  }

  async connect(): Promise<void> {
    const { id, testnet } = this.config.exchange;
    const wsUrl = testnet
      ? 'wss://testnet.binance.vision/ws'
      : 'wss://stream.binance.com:9443/ws';

    console.log(`📡 Connecting to ${id} WebSocket: ${wsUrl}`);

    return new Promise((resolve, reject) => {
      this.ws = new WebSocket(wsUrl);

      this.ws.on('open', () => {
        this.isConnected = true;
        console.log('✅ Market data feed connected');
        this.subscribeTickers();
        resolve();
      });

      this.ws.on('message', (data: WebSocket.Data) => {
        this.handleMessage(data.toString());
      });

      this.ws.on('error', (error) => {
        console.error('❌ WebSocket error:', error.message);
        if (!this.isConnected) {
          reject(error);
        }
      });

      this.ws.on('close', () => {
        this.isConnected = false;
        console.log('🔌 WebSocket disconnected, reconnecting...');
        this.scheduleReconnect();
      });
    });
  }

  private subscribeTickers(): void {
    if (!this.ws) return;

    const streams = this.config.trading.symbols
      .map((s) => `${s.toLowerCase().replace('/', '')}@ticker`)
      .join('/');

    this.ws.send(JSON.stringify({
      method: 'SUBSCRIBE',
      params: [streams],
      id: 1,
    }));
  }

  private handleMessage(data: string): void {
    try {
      const msg = JSON.parse(data);
      if (msg.e === '24hrTicker') {
        const ticker: Ticker = {
          symbol: msg.s,
          bid: parseFloat(msg.b),
          ask: parseFloat(msg.a),
          last: parseFloat(msg.c),
          volume: parseFloat(msg.v),
          timestamp: msg.E,
        };
        this.callbacks.forEach((cb) => cb(ticker));
      }
    } catch (e) {
      // Ignore parse errors for subscription confirmations
    }
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer) return;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect().catch(console.error);
    }, 5000);
  }

  onTicker(callback: TickerCallback): void {
    this.callbacks.push(callback);
  }

  async disconnect(): Promise<void> {
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
  }
}
