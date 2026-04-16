import { useCallback, useEffect, useState } from 'react'

const DEFAULT_URL = '/panel-api/snapshot'
const POLL_MS = 1_200

export type PanelFeedPoint = {
  value: number | null
  source_ts_ms: number | null
  observed_ts_ms: number | null
  stale: boolean
}

export type PanelFeed = {
  binance: PanelFeedPoint
  coinbase: PanelFeedPoint
  pyth: PanelFeedPoint
  chainlink: PanelFeedPoint
  blended: PanelFeedPoint
}

export type PanelSnapshot = {
  last_update_ms: number
  feed: PanelFeed
  errors?: string[]
}

export function usePanelFeed(pollMs: number = POLL_MS) {
  const url = (import.meta.env.VITE_PANEL_URL as string | undefined) ?? DEFAULT_URL
  const [data, setData] = useState<PanelSnapshot | null>(null)
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

      const payload = raw as PanelSnapshot
      setData(payload)
      setLastOkAtMs(Date.now())

      if (Array.isArray(payload.errors) && payload.errors.length > 0) {
        setError(payload.errors[0] ?? 'panel feed warning')
      } else {
        setError(null)
      }
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
