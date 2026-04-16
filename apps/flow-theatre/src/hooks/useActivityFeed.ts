import { useCallback, useEffect, useState } from 'react'

const DEFAULT_URL = '/api/activity'
const POLL_MS = 1_200

export type ActivityMarket = {
  market_id: string
  condition_id: string
  slug: string
  question: string
  tick_size: string
  min_order_size: string
}

export type ActivityTrade = {
  ts_ms: number
  condition_id: string
  slug: string
  question: string
  leg: string
  side: string
  price: string
  size: string
  role: string
  reason: string
  order_id: string | null
}

export type ActivityStats = {
  maker_fills_session: number
  taker_fills_session: number
  trades_in_buffer: number
  session_volume_usdc?: string
  session_volume_shares?: string
}

export type ActivityPayload = {
  server_time_ms: number
  market_count: number
  markets: ActivityMarket[]
  trades: ActivityTrade[]
  stats: ActivityStats
  paper_mode?: boolean
  paper_balance_usdc?: string
  paper_pnl_usdc?: string
  error?: string
}

export function useActivityFeed(pollMs: number = POLL_MS) {
  const url = (import.meta.env.VITE_ACTIVITY_URL as string | undefined) ?? DEFAULT_URL
  const [data, setData] = useState<ActivityPayload | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [lastOkAtMs, setLastOkAtMs] = useState<number | null>(null)

  const tick = useCallback(async () => {
    try {
      const res = await fetch(url, { cache: 'no-store' })
      const raw = await res.json()
      if (!res.ok) {
        setError(`HTTP ${res.status}`)
        return
      }
      const payload = raw as ActivityPayload
      if (payload.error) {
        setError(payload.error)
        setData(payload)
        return
      }
      setError(null)
      setData(payload)
      setLastOkAtMs(Date.now())
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e)
      setError(msg)
    }
  }, [url])

  useEffect(() => {
    void tick()
    const id = window.setInterval(() => void tick(), pollMs)
    return () => clearInterval(id)
  }, [tick, pollMs])

  return { data, error, lastOkAtMs, refresh: tick, feedUrl: url }
}
