#!/bin/bash

# AlephTX Dual-Track IPC 实时监控仪表板

clear
echo "╔════════════════════════════════════════════════════════════════╗"
echo "║     🚀 AlephTX Dual-Track IPC - 实时监控仪表板                 ║"
echo "║     账户余额: ~\$200 USDC | 目标: 盈利 >5%                      ║"
echo "╚════════════════════════════════════════════════════════════════╝"
echo ""

# 检查进程状态
echo "📊 系统状态"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if pgrep -f "lighter_feeder" > /dev/null; then
    echo "✅ Go Feeder (Lighter Private):  运行中"
else
    echo "❌ Go Feeder (Lighter Private):  已停止"
fi

if pgrep -f "event_monitor" > /dev/null; then
    echo "✅ Event Monitor:                运行中"
else
    echo "❌ Event Monitor:                已停止"
fi

echo ""

# 检查共享内存
echo "💾 共享内存"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
ls -lh /dev/shm/aleph-* 2>/dev/null | awk '{print $9, $5}'
echo ""

# Lighter Feeder 日志
echo "🔌 Lighter Private Stream (最近 10 行)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
tail -10 lighter_feeder.log 2>/dev/null | sed 's/^/  /'
echo ""

# Event Monitor 日志
echo "🔍 Event Monitor (最近 10 行)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
tail -10 event_monitor.log 2>/dev/null | sed 's/^/  /'
echo ""

# 网络连接
echo "🌐 网络连接"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
netstat -an 2>/dev/null | grep "mainnet.zklighter" | head -3 | sed 's/^/  /'
echo ""

# 系统资源
echo "⚡ 系统资源"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  CPU: $(top -bn1 | grep "Cpu(s)" | awk '{print $2}')% | RAM: $(free -h | awk '/^Mem:/ {print $3 "/" $2}')"
echo ""

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "💡 提示: 使用 'watch -n 2 ./monitor.sh' 实时刷新"
echo "📝 日志: tail -f lighter_feeder.log event_monitor.log"
echo "🛑 停止: pkill -f 'lighter_feeder|event_monitor'"
