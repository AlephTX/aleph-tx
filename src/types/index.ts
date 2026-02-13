export interface Order {
  id: string;
  symbol: string;
  side: 'long' | 'short';
  size: number;
  status: 'pending' | 'filled' | 'cancelled' | 'failed';
  fillPrice?: number;
  timestamp: number;
}

export interface Position {
  symbol: string;
  side: 'long' | 'short';
  size: number;
  entryPrice: number;
  timestamp: number;
}

export interface Signal {
  symbol: string;
  type: 'ENTRY' | 'EXIT' | 'PRICE_UPDATE' | 'ERROR';
  price: number;
  timestamp: number;
  reason?: string;
}
