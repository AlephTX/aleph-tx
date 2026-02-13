# AlephTX 量化交易系统 - 技术规范

## 核心目标
- **速度优先**：行情获取延迟最低
- **代码质量**：严格规范

## 技术方案

### 1. 行情数据获取
- **WebSocket** 优先于 REST API（ Binance WS: wss://stream.binance.com:9443 ）
- 对于不支持 WS 的交易所，用 REST + 缓存，但主要交易所必须用 WS
- 数据预处理在内存中完成，不落盘

### 2. 延迟优化
- WSL2 → 目标交易所的网络延迟需要测试
- 考虑用 China Edge 节点或香港服务器
- 代码层面：异步非阻塞 + 连接池

### 3. 代码规范
- TypeScript 强制类型检查
- ESLint + Prettier
- 单元测试覆盖率 > 80%
- Gitmoji commit 规范
- CI/CD 自动化

### 4. 架构
- Node.js + TypeScript + CCXT
- Telegram Bot (@AlephTXBot) 作为控制面板
- Docker 容器化部署

### 5. 待定
- 交易所选择
- 策略方向
