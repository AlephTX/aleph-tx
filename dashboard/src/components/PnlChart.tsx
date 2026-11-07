import { useEffect, useRef, useState } from 'react'
import {
  AreaSeries,
  ColorType,
  CrosshairMode,
  LineSeries,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type UTCTimestamp,
} from 'lightweight-charts'
import { usePnl, type Bucket } from '../queries'
import { Card, Empty, ErrorNote } from './ui'

const RANGES: { label: string; bucket: Bucket; span: number }[] = [
  { label: '6H', bucket: '1m', span: 6 * 3_600_000 },
  { label: '1D', bucket: '5m', span: 86_400_000 },
  { label: '7D', bucket: '1h', span: 7 * 86_400_000 },
  { label: '30D', bucket: '1d', span: 30 * 86_400_000 },
  { label: '全部', bucket: '1d', span: 0 },
]

const css = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim()
const ts = (ms: number) => Math.floor(ms / 1000) as UTCTimestamp

export function PnlChart() {
  const [range, setRange] = useState(1)
  const r = RANGES[range] ?? RANGES[1]!
  // Round `since` to the bucket so the query key stays stable between polls.
  const [since] = useSinceFor(r.span)
  const { data, error } = usePnl(r.bucket, since)
  const host = useRef<HTMLDivElement>(null)
  const chart = useRef<IChartApi | null>(null)
  const pnl = useRef<ISeriesApi<'Area'> | null>(null)
  const equity = useRef<ISeriesApi<'Line'> | null>(null)

  useEffect(() => {
    const el = host.current
    if (!el) return
    const c = createChart(el, {
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: 'transparent' },
        textColor: css('--color-muted') || '#7d8a9c',
        fontFamily: css('--font-mono'),
        fontSize: 11,
        attributionLogo: false,
      },
      grid: { vertLines: { color: '#1d253166' }, horzLines: { color: '#1d253166' } },
      crosshair: { mode: CrosshairMode.Normal },
      rightPriceScale: { borderColor: '#1d2531' },
      leftPriceScale: { visible: true, borderColor: '#1d2531' },
      timeScale: { borderColor: '#1d2531', timeVisible: true, secondsVisible: false },
      localization: { locale: 'zh-CN' },
    })
    pnl.current = c.addSeries(AreaSeries, {
      lineColor: '#5aa9ff',
      topColor: '#5aa9ff40',
      bottomColor: '#5aa9ff00',
      lineWidth: 2,
      priceScaleId: 'right',
      title: '累计净盈亏',
      priceFormat: { type: 'price', precision: 2, minMove: 0.01 },
    })
    equity.current = c.addSeries(LineSeries, {
      color: '#f5b84a',
      lineWidth: 1,
      lineStyle: 2,
      priceScaleId: 'left',
      title: '权益',
      priceFormat: { type: 'price', precision: 2, minMove: 0.01 },
    })
    chart.current = c
    return () => {
      c.remove()
      chart.current = null
    }
  }, [])

  useEffect(() => {
    if (!data || !pnl.current || !equity.current) return
    pnl.current.setData(data.pnl.map((p) => ({ time: ts(p.t), value: p.cumulative })))
    equity.current.setData(data.equity.map((p) => ({ time: ts(p.t), value: p.equity })))
    chart.current?.timeScale().fitContent()
  }, [data])

  const empty = data && data.pnl.length === 0 && data.equity.length === 0
  return (
    <Card
      title="累计净盈亏 / 权益"
      right={
        <div className="flex rounded-md bg-bg p-0.5 text-xs">
          {RANGES.map((x, i) => (
            <button
              key={x.label}
              type="button"
              onClick={() => setRange(i)}
              className={`rounded px-2 py-0.5 ${i === range ? 'bg-panel-2 text-text' : 'text-muted hover:text-text'}`}
            >
              {x.label}
            </button>
          ))}
        </div>
      }
    >
      <ErrorNote error={error} />
      <div className="relative h-80">
        <div ref={host} className="absolute inset-0" />
        {empty && (
          <div className="absolute inset-0 grid place-items-center">
            <Empty>该时间范围内暂无成交或快照</Empty>
          </div>
        )}
      </div>
    </Card>
  )
}

/** Window start, refreshed once a minute so the query key changes slowly. */
function useSinceFor(span: number): [number] {
  const [tick, setTick] = useState(() => Math.floor(Date.now() / 60_000))
  useEffect(() => {
    const id = setInterval(() => setTick(Math.floor(Date.now() / 60_000)), 60_000)
    return () => clearInterval(id)
  }, [])
  return [span > 0 ? tick * 60_000 - span : 0]
}
