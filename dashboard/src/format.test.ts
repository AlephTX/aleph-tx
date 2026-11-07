import { describe, expect, it } from 'vitest'
import { ago, bps, compactUsd, duration, parse, pct, price, signedUsd, tone, usd } from './format'

describe('format', () => {
  it('renders dashes for missing values', () => {
    for (const v of [null, undefined, Number.NaN, Infinity]) {
      expect(usd(v)).toBe('—')
      expect(pct(v)).toBe('—')
      expect(bps(v)).toBe('—')
    }
  })

  it('formats money with sign', () => {
    expect(usd(1234.5)).toBe('$1,234.50')
    expect(usd(-3.2)).toBe('-$3.20')
    expect(signedUsd(2)).toBe('+$2.00')
    expect(signedUsd(-2)).toBe('-$2.00')
    expect(signedUsd(0)).toBe('$0.00')
    expect(compactUsd(1_500_000)).toBe('$1.50M')
    expect(compactUsd(-25_000)).toBe('-$25.0K')
  })

  it('scales price precision', () => {
    expect(price(2500.123)).toBe('2,500.12')
    expect(price(1.23456)).toBe('1.2346')
    expect(price(0.000123)).toBe('0.000123')
    expect(price(0)).toBe('—')
  })

  it('formats ratios and tone', () => {
    expect(pct(0.5234)).toBe('52.3%')
    expect(bps(1.234)).toBe('+1.23 bps')
    expect(tone(1)).toBe('up')
    expect(tone(-1)).toBe('down')
    expect(tone(0)).toBe('flat')
  })

  it('formats durations', () => {
    expect(duration(5_000)).toBe('5秒')
    expect(duration(125_000)).toBe('2分5秒')
    expect(duration(3_720_000)).toBe('1时2分')
    expect(duration(90_000_000)).toBe('1天1时')
    expect(ago(1000, 6000)).toBe('5秒前')
  })

  it('parses decimal strings', () => {
    expect(parse('-0.25')).toBe(-0.25)
    expect(parse(null)).toBeNull()
    expect(parse('abc')).toBeNull()
  })
})
