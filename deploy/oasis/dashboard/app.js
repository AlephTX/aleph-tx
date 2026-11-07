'use strict';
const get = (id) => document.getElementById(id);
const price = new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 8 });
const clock = new Intl.DateTimeFormat('zh-CN', { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });
const fmt = (value) => value == null || !Number.isFinite(Number(value)) ? '—' : price.format(Number(value));
const time = (value) => value ? clock.format(new Date(value)) : '—';
function set(id, value, tone = '') {
  const el = get(id);
  el.textContent = value;
  el.classList.remove('good', 'warn', 'bad');
  if (tone) el.classList.add(tone);
}
function node(tag, value, kind = '') {
  const el = document.createElement(tag);
  el.textContent = value;
  if (kind) el.className = kind;
  return el;
}
function marketRow(venue, symbol) {
  const row = document.createElement('tr');
  const first = row.insertCell();
  first.append(node('span', symbol, 'market-symbol'), node('span', venue, 'venue'));
  return row;
}
function cell(row, value, kind) { row.append(node('td', value, kind)); }
function state(service, inactive) {
  switch (service?.ActiveState) {
    case 'active': return ['运行中', 'good'];
    case 'inactive': return [inactive, ''];
    case 'activating': return ['启动中', 'warn'];
    case 'deactivating': return ['停止中', 'warn'];
    case 'failed': return ['故障', 'bad'];
    default: return ['状态未知', 'warn'];
  }
}
function render(data) {
  const services = data.services;
  const feeder = services['aleph-tx-feeder'];
  const live = services['aleph-tx-live'];
  const count = data.markets.filter(m => m.state === 'live').length;
  set('host', data.host.split('.')[0].toUpperCase());
  set('checked', time(data.checked_at));
  get('checked').dateTime = data.checked_at;
  set('feeder', ...state(feeder, '已停止'));
  set('feeder-detail', feeder.MainPID && feeder.MainPID !== '0' ? `PID ${feeder.MainPID} · 重启 ${feeder.NRestarts ?? '—'} 次` : `systemd · ${feeder.SubState ?? 'unknown'}`);
  set('live', ...state(live, '未启动'));
  set('live-detail', live.ActiveState === 'inactive' ? '当前运行公开行情采集' : `systemd · ${live.SubState ?? 'unknown'}`);
  set('markets', `${count} / ${data.markets.length}`, count === data.markets.length ? 'good' : 'warn');
  set('markets-detail', '15 秒内收到报价的市场');
  get('quotes').replaceChildren(...data.markets.map(m => {
    const row = marketRow(m.venue, `${m.symbol}-PERP`);
    for (const n of [m.bid, m.ask, m.mid]) cell(row, fmt(n));
    cell(row, m.spread_bps == null ? '—' : m.spread_bps.toFixed(2));
    const age = m.age_ms == null ? '' : ` · ${(m.age_ms / 1000).toFixed(1)}s`;
    cell(row, (m.state === 'live' ? '实时' : m.state === 'stale' ? '延迟' : '等待报价') + age, m.state === 'live' ? 'good' : 'warn');
    return row;
  }));
  const accountCards = [], positionRows = [];
  for (const [venue, snapshot] of Object.entries(data.accounts)) {
    const card = node('div', '', 'account');
    let detail, tone = '';
    if (snapshot.state === 'ready') {
      detail = `${snapshot.positions.length ? `${snapshot.positions.length} 个持仓` : '无未平仓合约'} · ${time(snapshot.as_of)}`;
      tone = 'good';
      for (const p of snapshot.positions) {
        const row = marketRow(venue, p.symbol);
        const short = Number(p.size) < 0;
        cell(row, short ? '空' : '多', short ? 'bad' : 'good');
        cell(row, fmt(Math.abs(Number(p.size))));
        cell(row, fmt(p.entry));
        cell(row, fmt(p.unrealized_pnl), Number(p.unrealized_pnl) < 0 ? 'bad' : 'good');
        cell(row, fmt(p.liquidation));
        positionRows.push(row);
      }
    } else if (snapshot.state === 'error') {
      detail = `仓位读取失败 · ${snapshot.error} · ${time(snapshot.as_of)}`;
      tone = 'bad';
    } else {
      detail = snapshot.state === 'waiting' ? '正在读取账户' : '未配置账户';
    }
    card.append(node('strong', venue), node('p', detail, `detail ${tone}`));
    accountCards.push(card);
  }
  get('accounts').replaceChildren(...accountCards);
  get('positions').replaceChildren(...positionRows);
  get('position-table').hidden = !positionRows.length;
  set('release', data.release ? `发布 ${data.release.id}` : '开发预览');
}
let first = true;
async function refresh() {
  try {
    const response = await fetch('/api/status', { cache: 'no-store', signal: AbortSignal.timeout(4500) });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    render(await response.json());
    first = false;
    get('disconnect').hidden = true;
    set('connection', 'Oasis 已连接', 'good');
  } catch {
    set('connection', '连接中断', 'bad');
    if (first) get('disconnect').textContent = '无法读取 Oasis。请检查本地 SSH 隧道和主机上的看板服务。';
    else get('disconnect').textContent = '无法读取 Oasis。请检查本地 SSH 隧道；下方为最后一次成功读取的数据。';
    get('disconnect').hidden = false;
  } finally {
    setTimeout(refresh, 3000);
  }
}
refresh();
