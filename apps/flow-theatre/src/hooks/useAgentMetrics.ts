import { useCallback, useEffect, useRef, useState } from 'react'
import {
  parsePrometheusText,
  snapshotFromPrometheus,
  type AgentMetricSnapshot,
} from '../metrics/parsePrometheus'

const DEFAULT_URL = '/metrics'

export type MetricsState = {
  snapshot: AgentMetricSnapshot | null
  eventsPerSec: number | null
  scrapeError: string | null
  lastOkAtMs: number | null
}

function formatSnapshotLine(s: AgentMetricSnapshot): string {
  const parts: string[] = []
  if (s.decisionCycleMs !== undefined) parts.push(`cycle_ms=${s.decisionCycleMs.toFixed(2)}`)
  if (s.queueLen !== undefined) parts.push(`queue=${Math.round(s.queueLen)}`)
  if (s.makerRatio !== undefined) parts.push(`maker_ratio=${(s.makerRatio * 100).toFixed(1)}%`)
  if (s.signalEdge !== undefined) parts.push(`edge=${s.signalEdge.toFixed(5)}`)
  if (s.chainlinkPythBasis !== undefined) parts.push(`cl_pyth=${s.chainlinkPythBasis.toFixed(6)}`)
  if (s.adverseSelection !== undefined) parts.push(`adv_sel=${s.adverseSelection.toFixed(5)}`)
  if (s.eventsIngestedTotal !== undefined) parts.push(`events_total=${s.eventsIngestedTotal.toFixed(0)}`)
  return parts.length > 0 ? parts.join(' · ') : 'scrape_ok (no polyhft_* series yet)'
}

export function useAgentMetrics(pollMs: number) {
  const url = (import.meta.env.VITE_PROMETHEUS_URL as string | undefined) ?? DEFAULT_URL
  const [state, setState] = useState<MetricsState>({
    snapshot: null,
    eventsPerSec: null,
    scrapeError: null,
    lastOkAtMs: null,
  })
  const prevEv = useRef<{ t: number; n: number } | null>(null)

  const tick = useCallback(async () => {
    try {
      const res = await fetch(url, { cache: 'no-store' })
      if (!res.ok) {
        setState((prev) => ({
          ...prev,
          scrapeError: `HTTP ${res.status}`,
        }))
        return
      }
      const text = await res.text()
      const map = parsePrometheusText(text)
      const snapshot = snapshotFromPrometheus(map)
      const now = performance.now()

      let eventsPerSec: number | null = null
      if (snapshot.eventsIngestedTotal !== undefined) {
        const n = snapshot.eventsIngestedTotal
        const p = prevEv.current
        if (p !== null) {
          const dt = (now - p.t) / 1000
          if (dt > 0.05) {
            eventsPerSec = Math.max(0, (n - p.n) / dt)
          }
        }
        prevEv.current = { t: now, n }
      }

      setState({
        snapshot,
        eventsPerSec,
        scrapeError: null,
        lastOkAtMs: Date.now(),
      })
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e)
      setState((prev) => ({
        ...prev,
        scrapeError: msg,
      }))
    }
  }, [url])

  useEffect(() => {
    void tick()
    const id = window.setInterval(() => void tick(), pollMs)
    return () => clearInterval(id)
  }, [tick, pollMs])

  return { ...state, metricsUrl: url, formatSnapshotLine }
}

export type FeedLine = {
  id: string
  ts: string
  stage: string
  level: 'info' | 'ok' | 'warn'
  msg: string
}

export function useMetricsFeed(
  snapshot: AgentMetricSnapshot | null,
  scrapeError: string | null,
  formatSnapshotLine: (s: AgentMetricSnapshot) => string,
  maxLines: number,
) {
  const [feed, setFeed] = useState<FeedLine[]>([])
  const seq = useRef(0)

  useEffect(() => {
    const ts = new Date().toISOString().slice(11, 23)
    if (scrapeError) {
      const line: FeedLine = {
        id: `m-${seq.current++}`,
        ts,
        stage: 'METRICS',
        level: 'warn',
        msg: `scrape failed: ${scrapeError}`,
      }
      setFeed((prev) => [line, ...prev].slice(0, maxLines))
      return
    }
    if (!snapshot) return
    const msg = formatSnapshotLine(snapshot)
    const line: FeedLine = {
      id: `m-${seq.current++}`,
      ts,
      stage: 'METRICS',
      level: 'ok',
      msg,
    }
    setFeed((prev) => [line, ...prev].slice(0, maxLines))
  }, [snapshot, scrapeError, formatSnapshotLine, maxLines])

  return feed
}
