import * as fs from 'fs';
import * as path from 'path';
import { z } from 'zod';

const ConfigSchema = z.object({
  exchange: z.object({
    id: z.string().default('binance'),
    testnet: z.boolean().default(true),
  }),
  trading: z.object({
    symbols: z.array(z.string()).default(['BTC/USDT', 'ETH/USDT']),
    maxPositionSize: z.number().default(0.1),
    riskPerTrade: z.number().default(0.02),
  }),
  telegram: z.object({
    botToken: z.string(),
    allowedUsers: z.array(z.string()).default([]),
  }),
  api: z.object({
    key: z.string().default(''),
    secret: z.string().default(''),
  }),
});

export type Config = z.infer<typeof ConfigSchema>;

class ConfigLoader {
  private static instance: Config | null = null;

  static load(): Config {
    if (ConfigLoader.instance) {
      return ConfigLoader.instance;
    }

    const envPath = path.resolve(process.cwd(), '.env');
    if (fs.existsSync(envPath)) {
      const env = fs.readFileSync(envPath, 'utf-8');
      env.split('\n').forEach((line) => {
        const [key, value] = line.split('=');
        if (key && value) {
          process.env[key.trim()] = value.trim();
        }
      });
    }

    const config: Config = {
      exchange: {
        id: process.env.EXCHANGE_ID || 'binance',
        testnet: process.env.EXCHANGE_TESTNET === 'true',
      },
      trading: {
        symbols: (process.env.TRADING_SYMBOLS || 'BTC/USDT,ETH/USDT').split(','),
        maxPositionSize: parseFloat(process.env.MAX_POSITION_SIZE || '0.1'),
        riskPerTrade: parseFloat(process.env.RISK_PER_TRADE || '0.02'),
      },
      telegram: {
        botToken: process.env.TELEGRAM_BOT_TOKEN || '',
        allowedUsers: (process.env.ALLOWED_USERS || '').split(',').filter(Boolean),
      },
      api: {
        key: process.env.API_KEY || '',
        secret: process.env.API_SECRET || '',
      },
    };

    ConfigLoader.instance = ConfigSchema.parse(config);
    return ConfigLoader.instance;
  }
}

export const Config = ConfigLoader;
