import { useEffect, useRef, useState, type CSSProperties } from 'react'
import { val } from '@theatre/core'
import { simObject, themeObject } from '../theatre/flowProject'
import { useAgentMetrics, useMetricsFeed } from '../hooks/useAgentMetrics'
import { useActivityFeed } from '../hooks/useActivityFeed'
import { usePanelFeed, type PanelFeedPoint } from '../hooks/usePanelFeed'
import type { AgentMetricSnapshot } from '../metrics/parsePrometheus'
import styles from './FlowDashboard.module.css'

const STAGES = [
  { id: 'ingest', label: 'Ingest', eyebrow: 'Market tape' },
  { id: 'signal', label: 'Signal', eyebrow: 'Edge map' },
  { id: 'risk', label: 'Risk', eyebrow: 'Guard rails' },
  { id: 'execute', label: 'Execute', eyebrow: 'Order flow' },
] as const

const METRICS_POLL_MS = 750

function rgbaToCss(c: { r: number; g: number; b: number; a: number }) {
  return `rgba(${c.r}, ${c.g}, ${c.b}, ${c.a})`
}

function wrap01(x: number) {
  return ((x % 1) + 1) % 1
}

function fmtBalance(v: string | undefined): string {
  if (!v) return '$5,000.00'
  const n = Number.parseFloat(v)
  if (Number.isNaN(n)) return v
  return n.toLocaleString('en-US', { style: 'currency', currency: 'USD' })
}

function fmtPnl(v: string | undefined): { text: string; sign: 'pos' | 'neg' | 'zero' } {
  if (!v) return { text: '—', sign: 'zero' }
  const n = Number.parseFloat(v)
  if (Number.isNaN(n)) return { text: v, sign: 'zero' }
  const sign = n > 0.005 ? 'pos' : n < -0.005 ? 'neg' : 'zero'
  const prefix = n > 0.005 ? '+' : ''
  return {
    text: `${prefix}${n.toLocaleString('en-US', { style: 'currency', currency: 'USD' })}`,
    sign,
  }
}

function formatTime(ms: number) {
  return new Date(ms).toLocaleTimeString(undefined, {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  })
}

function formatAge(nowMs: number, lastOkAtMs: number | null) {
  if (lastOkAtMs === null) return '—'
  const sec = Math.max(0, Math.floor((nowMs - lastOkAtMs) / 1000))
  if (sec < 60) return `${sec}s`
  if (sec < 3600) return `${Math.floor(sec / 60)}m`
  return `${Math.floor(sec / 3600)}h`
}

function formatMakerPct(v: number | undefined) {
  return v === undefined ? '—' : `${(v * 100).toFixed(1)}%`
}

function formatBasisBp(v: number | undefined) {
  return v === undefined ? '—' : `${(v * 10000).toFixed(2)} bp`
}

function formatMetric(v: number | undefined, digits = 2) {
  return v === undefined ? '—' : v.toFixed(digits)
}

function formatNotional(price: string, size: string) {
  const px = Number.parseFloat(price)
  const sz = Number.parseFloat(size)
  if (Number.isNaN(px) || Number.isNaN(sz)) return '—'
  return `$${(px * sz).toFixed(2)}`
}

function formatUsdPrice(v: number | null | undefined) {
  if (v === undefined || v === null || Number.isNaN(v)) return '—'
  return v.toLocaleString('en-US', {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })
}

function formatFeedMeta(nowMs: number, point: PanelFeedPoint | undefined) {
  if (!point) return 'waiting…'
  const age = formatAge(nowMs, point.source_ts_ms)
  return `${point.stale ? 'stale' : 'live'} · ${age}`
}

function compactSlug(slug: string) {
  return slug.replace(
    /^(btc|eth|sol)-updown-(5m|15m)-/,
    (_, sym: string, tf: string) => `${sym.toUpperCase()} ${tf} · `,
  )
}

function compactReason(reason: string) {
  return reason
    .replace(/directional FOK market buy /i, '')
    .replace(/directional FOK market sell /i, '')
    .replace(/maker /i, '')
}

function summarizeError(message: string | null | undefined, fallback: string) {
  if (!message) return fallback
  const lower = message.toLowerCase()
  if (lower.includes('unexpected token') || lower.includes('valid json')) {
    return `${fallback.replace(' offline', '')} invalid`
  }
  if (lower.includes('failed to fetch')) {
    return `${fallback.replace(' offline', '')} unreachable`
  }
  if (lower.startsWith('http ')) return message
  return message.length > 30 ? `${message.slice(0, 30)}…` : message
}

export function FlowDashboard() {
  const rafRef = useRef(0)
  const startRef = useRef(performance.now())
  const snapshotRef = useRef<AgentMetricSnapshot | null>(null)
  const prevFillCount = useRef(0)

  const {
    snapshot,
    eventsPerSec,
    scrapeError,
    lastOkAtMs,
    metricsUrl,
    formatSnapshotLine,
  } = useAgentMetrics(METRICS_POLL_MS)
  snapshotRef.current = snapshot

  const metricsFeed = useMetricsFeed(snapshot, scrapeError, formatSnapshotLine, 48)
  const { data: activity, error: actError, lastOkAtMs: actOkMs } = useActivityFeed(1200)
  const { data: panel, error: panelError, lastOkAtMs: panelOkMs } = usePanelFeed(1200)

  const [nowMs, setNowMs] = useState(() => Date.now())
  const [theme, setTheme] = useState(() => ({
    accent: val(themeObject.props.accent),
    spine: val(themeObject.props.spine),
    nodeIdle: val(themeObject.props.nodeIdle),
    nodeHot: val(themeObject.props.nodeHot),
  }))
  const [sim, setSim] = useState(() => ({
    packetPhase: val(simObject.props.packetPhase),
    wave: val(simObject.props.wave),
  }))

  useEffect(() => {
    const unsubTheme = themeObject.onValuesChange((value) => setTheme(value))
    const unsubSim = simObject.onValuesChange((value) => setSim(value))
    return () => {
      unsubTheme()
      unsubSim()
    }
  }, [])

  useEffect(() => {
    const id = window.setInterval(() => setNowMs(Date.now()), 1000)
    return () => clearInterval(id)
  }, [])

  useEffect(() => {
    const tick = () => {
      const t = (performance.now() - startRef.current) / 1000
      const snap = snapshotRef.current
      simObject.initialValue = {
        packetPhase: wrap01(t * 0.14),
        wave: (Math.sin(t * 1.1) * 0.5 + 0.5) % 1,
        scanSweep: wrap01(t * 0.26),
        throughput: eventsPerSec ?? 0,
        latencyMs: snap?.decisionCycleMs ?? 0,
        queueDepth: Math.max(0, Math.round(snap?.queueLen ?? 0)),
        spineGlow: 0.2 + ((Math.sin(t * 1.1) * 0.5 + 0.5) % 1) * 0.55,
        makerRatio: snap?.makerRatio ?? 0,
        basisBp: (snap?.chainlinkPythBasis ?? 0) * 10000,
      }
      setSim({
        packetPhase: wrap01(t * 0.14),
        wave: (Math.sin(t * 1.1) * 0.5 + 0.5) % 1,
      })
      rafRef.current = requestAnimationFrame(tick)
    }
    rafRef.current = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(rafRef.current)
  }, [eventsPerSec])

  const accentCss = rgbaToCss(theme.accent)
  const shellStyle = {
    ['--accent' as string]: accentCss,
    ['--wave' as string]: sim.wave.toFixed(3),
  } as CSSProperties

  const metricsOk = !scrapeError && lastOkAtMs !== null
  const actOk = !actError && activity !== null && !activity.error
  const paperMode = activity?.paper_mode === true

  const trades = activity?.trades ?? []
  const markets = activity?.markets ?? []
  const stats = activity?.stats
  const visibleMarkets = markets.slice(0, 10)

  const newFills = Math.max(0, trades.length - prevFillCount.current)
  prevFillCount.current = trades.length

  const totalFills = (stats?.maker_fills_session ?? 0) + (stats?.taker_fills_session ?? 0)
  const makerRatio =
    snapshot?.makerRatio ??
    (totalFills > 0 ? (stats?.maker_fills_session ?? 0) / totalFills : undefined)
  const pnlInfo = fmtPnl(activity?.paper_pnl_usdc)
  const balancePct = (() => {
    const balance = activity?.paper_balance_usdc
    if (!balance) return 100
    const n = Number.parseFloat(balance)
    if (Number.isNaN(n)) return 100
    return Math.max(0, Math.min(100, (n / 5000) * 100))
  })()

  const metricsAge = formatAge(nowMs, lastOkAtMs)
  const activityAge = formatAge(nowMs, actOkMs)
  const panelAge = formatAge(nowMs, panelOkMs)
  const metricsStatusText = metricsOk ? `metrics ${metricsAge}` : summarizeError(scrapeError, 'metrics offline')
  const activityError = actError ?? activity?.error
  const activityStatusText = actOk ? `activity ${activityAge}` : summarizeError(activityError, 'activity offline')
  const panelOk = panel !== null && !panelError
  const panelStatusText = panelOk ? `btc feed ${panelAge}` : summarizeError(panelError, 'btc feed offline')
  const activeStageIdx = Math.min(STAGES.length - 1, Math.floor(sim.packetPhase * STAGES.length))
  const signalHeatPct =
    snapshot?.signalEdge !== undefined
      ? Math.max(8, Math.min(100, Math.abs(snapshot.signalEdge) * 140000))
      : 24 + sim.wave * 44
  const queuePressurePct =
    snapshot?.queueLen !== undefined
      ? Math.max(6, Math.min(100, Math.round(snapshot.queueLen) * 6))
      : 12 + sim.wave * 16

  const heroMetrics = paperMode
    ? [
        {
          label: 'Paper balance',
          value: fmtBalance(activity?.paper_balance_usdc),
          detail: 'Starting stack $5,000.00',
          tone: 'neutral',
        },
        {
          label: 'Session P&L',
          value: pnlInfo.text,
          detail: 'Real markets, simulated capital',
          tone: pnlInfo.sign,
        },
        {
          label: 'Maker fills',
          value: String(stats?.maker_fills_session ?? 0),
          detail: 'Passive executions',
          tone: 'neutral',
        },
        {
          label: 'Taker fills',
          value: String(stats?.taker_fills_session ?? 0),
          detail: 'Aggressive sweeps',
          tone: 'neutral',
        },
        {
          label: 'Session volume',
          value: stats?.session_volume_usdc
            ? Number.parseFloat(stats.session_volume_usdc).toLocaleString('en-US', {
                style: 'currency',
                currency: 'USD',
                maximumFractionDigits: 2,
              })
            : '—',
          detail: 'Cumulative fill notional',
          tone: 'neutral',
        },
      ]
    : [
        {
          label: 'Live fills',
          value: String(totalFills),
          detail: 'Confirmed in session buffer',
          tone: 'neutral',
        },
        {
          label: 'Markets tracked',
          value: String(activity?.market_count ?? 0),
          detail: 'Gamma-discovered universe',
          tone: 'neutral',
        },
        {
          label: 'Events / sec',
          value: eventsPerSec !== null ? eventsPerSec.toFixed(0) : '—',
          detail: 'Current ingest throughput',
          tone: 'neutral',
        },
        {
          label: 'Decision cycle',
          value: snapshot?.decisionCycleMs !== undefined ? `${snapshot.decisionCycleMs.toFixed(1)} ms` : '—',
          detail: 'End-to-end loop time',
          tone: 'neutral',
        },
      ]

  const stageCards = STAGES.map((stage) => {
    if (stage.id === 'ingest') {
      return {
        ...stage,
        value: eventsPerSec !== null ? `${eventsPerSec.toFixed(0)} evt/s` : '—',
        note:
          snapshot?.eventsIngestedTotal !== undefined
            ? `${snapshot.eventsIngestedTotal.toFixed(0)} events total`
            : metricsOk
              ? 'Metrics link healthy'
              : 'Waiting for scrape',
      }
    }

    if (stage.id === 'signal') {
      return {
        ...stage,
        value: formatMetric(snapshot?.signalEdge, 6),
        note:
          snapshot?.chainlinkPythBasis !== undefined
            ? `${(snapshot.chainlinkPythBasis * 10000).toFixed(2)} bp basis`
            : 'No basis snapshot yet',
      }
    }

    if (stage.id === 'risk') {
      return {
        ...stage,
        value: formatMetric(snapshot?.adverseSelection, 6),
        note:
          snapshot?.queueLen !== undefined
            ? `Queue ${Math.round(snapshot.queueLen)}`
            : 'Queue depth pending',
      }
    }

    return {
      ...stage,
      value: String(totalFills),
      note: makerRatio !== undefined ? `${formatMakerPct(makerRatio)} maker share` : 'No fills yet',
    }
  })

  const commandStats = [
    { label: 'Events/s', value: eventsPerSec !== null ? eventsPerSec.toFixed(0) : '—' },
    {
      label: 'Cycle',
      value: snapshot?.decisionCycleMs !== undefined ? `${snapshot.decisionCycleMs.toFixed(1)}ms` : '—',
    },
    { label: 'Markets', value: String(activity?.market_count ?? '—') },
    { label: 'Fills', value: String(totalFills) },
  ]

  const systemCells = [
    {
      label: 'Latency',
      value: snapshot?.decisionCycleMs !== undefined ? `${snapshot.decisionCycleMs.toFixed(2)} ms` : '—',
      note: metricsOk ? `metrics ${metricsAge}` : scrapeError ?? 'metrics offline',
    },
    {
      label: 'Queue',
      value: snapshot?.queueLen !== undefined ? String(Math.round(snapshot.queueLen)) : '—',
      note: actOk ? `activity ${activityAge}` : activityStatusText,
    },
    {
      label: 'Maker share',
      value: formatMakerPct(makerRatio),
      note: `${stats?.maker_fills_session ?? 0} maker / ${stats?.taker_fills_session ?? 0} taker`,
    },
    {
      label: 'CL vs Pyth',
      value: formatBasisBp(snapshot?.chainlinkPythBasis),
      note: snapshot?.signalEdge !== undefined ? `edge ${snapshot.signalEdge.toFixed(6)}` : 'edge pending',
    },
  ]

  const signalCells = [
    {
      label: 'Signal edge',
      value: formatMetric(snapshot?.signalEdge, 6),
      note: 'Edge after fee-aware scoring',
    },
    {
      label: 'Adverse sel.',
      value: formatMetric(snapshot?.adverseSelection, 6),
      note: 'Online risk drag estimate',
    },
    {
      label: 'Trades buffer',
      value: String(stats?.trades_in_buffer ?? 0),
      note: 'Recent fills retained in memory',
    },
    {
      label: 'Metrics source',
      value: metricsUrl.replace(/^https?:\/\//, ''),
      note: 'Prometheus scrape endpoint',
    },
  ]

  const panelFeed = panel?.feed
  const feedCards: Array<{
    key: keyof NonNullable<typeof panelFeed>
    label: string
    tone: 'cyan' | 'blue' | 'amber' | 'lime' | 'default'
  }> = [
    { key: 'binance', label: 'Binance BTC', tone: 'cyan' },
    { key: 'coinbase', label: 'Coinbase BTC', tone: 'blue' },
    { key: 'pyth', label: 'Pyth BTC/USD', tone: 'amber' },
    { key: 'chainlink', label: 'Chainlink BTC', tone: 'lime' },
    { key: 'blended', label: 'Blended fair', tone: 'default' },
  ]

  return (
    <div className={styles.shell} style={shellStyle}>
      <div className={styles.shellBackdrop} aria-hidden />

      <header className={styles.commandBar}>
        <div className={styles.commandLead}>
          <div className={styles.brandMark}>PolyHFT</div>
          <div className={styles.brandInline}>
            <h1 className={styles.brandTitle}>Operator cockpit</h1>
            <a
              className={`${styles.panelLaunchLink} ${styles.inlineLaunchLink}`}
              href="http://localhost:3001"
            >
              Control Panel
            </a>
          </div>
        </div>

        <div className={styles.commandStats}>
          {commandStats.map((item) => (
            <div key={item.label} className={styles.commandStat}>
              <span className={styles.commandLabel}>{item.label}</span>
              <span className={styles.commandValue}>{item.value}</span>
            </div>
          ))}
        </div>

        <div className={styles.statusCluster}>
          <div
            className={styles.statusBadge}
            data-state={metricsOk ? 'ok' : 'err'}
            title={scrapeError ?? undefined}
          >
            <span className={styles.statusDot} />
            {metricsStatusText}
          </div>
          <div
            className={styles.statusBadge}
            data-state={actOk ? 'ok' : 'err'}
            title={activityError ?? undefined}
          >
            <span className={styles.statusDot} />
            {activityStatusText}
          </div>
          <div
            className={styles.statusBadge}
            data-state={panelOk ? 'ok' : 'err'}
            title={panelError ?? undefined}
          >
            <span className={styles.statusDot} />
            {panelStatusText}
          </div>
          <div className={styles.statusBadge} data-state={paperMode ? 'paper' : 'neutral'}>
            <span className={styles.statusDot} />
            {paperMode ? 'paper mode' : 'live monitor'}
          </div>
        </div>
      </header>

      <main className={styles.cockpit}>
        <aside className={styles.leftRail}>
          <section className={styles.panel}>
            <div className={styles.panelHeader}>
              <div>
                <div className={styles.panelEyebrow}>System</div>
                <h2 className={styles.panelTitle}>Health surface</h2>
              </div>
              <div className={styles.panelMeta}>{formatTime(activity?.server_time_ms ?? nowMs)}</div>
            </div>

            <div className={styles.systemHero}>
              <span className={styles.systemMode}>{paperMode ? 'Paper desk' : 'Live desk'}</span>
              <span className={styles.toneBadge} data-tone={metricsOk && actOk ? 'pos' : 'neg'}>
                {metricsOk && actOk ? 'stable' : 'degraded'}
              </span>
            </div>

            <div className={styles.systemGrid}>
              {systemCells.map((item) => (
                <div key={item.label} className={styles.systemCell}>
                  <span className={styles.systemLabel}>{item.label}</span>
                  <strong className={styles.systemValue}>{item.value}</strong>
                  <span className={styles.systemNote}>{item.note}</span>
                </div>
              ))}
            </div>
          </section>

          <section className={styles.panel}>
            <div className={styles.panelHeader}>
              <div>
                <div className={styles.panelEyebrow}>Signal</div>
                <h2 className={styles.panelTitle}>Bias + pressure</h2>
              </div>
              <div className={styles.panelMeta}>Theatre-linked pulse</div>
            </div>

            <div className={styles.systemGrid}>
              {signalCells.map((item) => (
                <div key={item.label} className={styles.systemCell}>
                  <span className={styles.systemLabel}>{item.label}</span>
                  <strong className={styles.systemValue}>{item.value}</strong>
                  <span className={styles.systemNote}>{item.note}</span>
                </div>
              ))}
            </div>

            <div className={styles.meterGroup}>
              <div className={styles.meterRow}>
                <span className={styles.systemLabel}>Signal heat</span>
                <span className={styles.panelMeta}>{signalHeatPct.toFixed(0)}%</span>
              </div>
              <div className={styles.meterTrack}>
                <div
                  className={styles.meterFill}
                  style={{ width: `${signalHeatPct}%` }}
                />
              </div>

              <div className={styles.meterRow}>
                <span className={styles.systemLabel}>Queue pressure</span>
                <span className={styles.panelMeta}>{queuePressurePct.toFixed(0)}%</span>
              </div>
              <div className={styles.meterTrack}>
                <div
                  className={styles.meterFill}
                  data-tone="warm"
                  style={{ width: `${queuePressurePct}%` }}
                />
              </div>
            </div>
          </section>

          <section className={styles.panel}>
            <div className={styles.panelHeader}>
              <div>
                <div className={styles.panelEyebrow}>Telemetry</div>
                <h2 className={styles.panelTitle}>Engine feed</h2>
              </div>
              <div className={styles.panelMeta}>{metricsFeed.length} lines</div>
            </div>

            {metricsFeed.length === 0 ? (
              <div className={styles.emptyState}>
                <div className={styles.emptySymbol}>·</div>
                <p>Waiting for metrics snapshots.</p>
              </div>
            ) : (
              <div className={styles.feedList} role="log">
                {metricsFeed.slice(0, 10).map((line) => (
                  <div key={line.id} className={styles.feedLine} data-level={line.level}>
                    <span className={styles.feedTimestamp}>{line.ts}</span>
                    <span className={styles.feedMessage}>{line.msg}</span>
                  </div>
                ))}
              </div>
            )}
          </section>
        </aside>

        <section className={styles.centerStage}>
          <section className={`${styles.panel} ${styles.heroPanel}`}>
            <div className={styles.heroTop}>
              <div>
                <div className={styles.heroKicker}>Desk summary</div>
                <div className={styles.heroMode}>
                  {paperMode ? 'Paper execution desk' : 'Live monitoring desk'}
                </div>
              </div>
              <div className={styles.heroNote}>
                {paperMode
                  ? 'Simulated fills on live Polymarket books. Real order flow, no real USDC spent.'
                  : 'Watching live metrics, fills, and market discovery without changing engine behavior.'}
              </div>
            </div>

            <div className={styles.heroMetrics}>
              {heroMetrics.map((item) => (
                <div key={item.label} className={styles.heroMetric}>
                  <span className={styles.heroMetricLabel}>{item.label}</span>
                  <strong className={styles.heroMetricValue} data-tone={item.tone}>
                    {item.value}
                  </strong>
                  <span className={styles.heroMetricDetail}>{item.detail}</span>
                </div>
              ))}
            </div>

            {paperMode && (
              <div className={styles.progressBlock}>
                <div className={styles.progressHeader}>
                  <span>Capital remaining</span>
                  <span>{balancePct.toFixed(1)}%</span>
                </div>
                <div className={styles.progressTrack}>
                  <div
                    className={styles.progressFill}
                    data-tone={balancePct < 20 ? 'neg' : balancePct < 50 ? 'warn' : 'pos'}
                    style={{ width: `${balancePct}%` }}
                  />
                </div>
              </div>
            )}
          </section>

          <section className={`${styles.panel} ${styles.pricePanel}`}>
            <div className={styles.panelHeader}>
              <div>
                <div className={styles.panelEyebrow}>BTC tape</div>
                <h2 className={styles.panelTitle}>Source prices</h2>
              </div>
              <div className={styles.panelMeta}>
                {panel?.last_update_ms ? formatTime(panel.last_update_ms) : 'waiting'}
              </div>
            </div>

            <div className={styles.priceGrid}>
              {feedCards.map((item) => {
                const point = panelFeed?.[item.key]
                return (
                  <div key={item.key} className={styles.priceTile} data-tone={item.tone}>
                    <span className={styles.priceTileLabel}>{item.label}</span>
                    <strong className={styles.priceTileValue}>{formatUsdPrice(point?.value)}</strong>
                    <span className={styles.priceTileMeta}>{formatFeedMeta(nowMs, point)}</span>
                  </div>
                )
              })}
            </div>
          </section>

          <section className={styles.panel}>
            <div className={styles.panelHeader}>
              <div>
                <div className={styles.panelEyebrow}>Execution</div>
                <h2 className={styles.panelTitle}>Pipeline</h2>
              </div>
              <div className={styles.panelMeta}>stage pulse {activeStageIdx + 1} / {STAGES.length}</div>
            </div>

            <div className={styles.stageGrid}>
              {stageCards.map((stage, index) => (
                <div
                  key={stage.id}
                  className={styles.stageCard}
                  data-active={index === activeStageIdx ? 'true' : 'false'}
                >
                  <span className={styles.stageBadge}>{stage.eyebrow}</span>
                  <strong className={styles.stageLabel}>{stage.label}</strong>
                  <div className={styles.stageValue}>{stage.value}</div>
                  <div className={styles.stageNote}>{stage.note}</div>
                  <div className={styles.stageFoot}>0{index + 1}</div>
                </div>
              ))}
            </div>
          </section>

          <section className={`${styles.panel} ${styles.fillsPanel}`}>
            <div className={styles.fillsHead}>
              <div>
                <div className={styles.panelEyebrow}>Execution tape</div>
                <h2 className={styles.panelTitle}>Live fills</h2>
              </div>
              <div className={styles.fillsNote}>
                <span className={styles.countPill}>{trades.length}</span>
                {newFills > 0 ? `+${newFills} new fills` : 'session buffer'}
              </div>
            </div>

            {trades.length === 0 ? (
              <div className={styles.emptyState}>
                <div className={styles.emptySymbol}>◌</div>
                <p>
                  {paperMode
                    ? 'Waiting for the first simulated fill. Feed input and edge thresholds must line up before the desk prints.'
                    : 'No fills in buffer yet. The desk is listening, but nothing has crossed yet.'}
                </p>
              </div>
            ) : (
              <div className={styles.tableWrap}>
                <table className={styles.fillsTable}>
                  <thead>
                    <tr>
                      <th>Time</th>
                      <th>Market</th>
                      <th>Leg</th>
                      <th>Side</th>
                      <th>Px</th>
                      <th>Size</th>
                      <th>Notional</th>
                      <th>Role</th>
                      <th>Reason</th>
                    </tr>
                  </thead>
                  <tbody>
                    {trades.map((trade, index) => (
                      <tr
                        key={`${trade.ts_ms}-${trade.order_id ?? 'x'}-${index}`}
                        className={index < newFills ? styles.rowNew : undefined}
                      >
                        <td className={styles.fillTime}>{formatTime(trade.ts_ms)}</td>
                        <td>
                          <a
                            className={styles.fillSlug}
                            href={`https://polymarket.com/event/${encodeURIComponent(trade.slug)}`}
                            target="_blank"
                            rel="noreferrer"
                          >
                            {compactSlug(trade.slug)}
                          </a>
                          <div className={styles.fillQuestion}>
                            {trade.question.length > 56 ? `${trade.question.slice(0, 56)}…` : trade.question}
                          </div>
                        </td>
                        <td>{trade.leg}</td>
                        <td>
                          <span
                            className={`${styles.sidePill} ${
                              trade.side === 'BUY' ? styles.sideBuy : styles.sideSell
                            }`}
                          >
                            {trade.side}
                          </span>
                        </td>
                        <td className={styles.fillNumber}>{trade.price}</td>
                        <td className={styles.fillNumber}>{trade.size}</td>
                        <td className={styles.fillNumber}>{formatNotional(trade.price, trade.size)}</td>
                        <td>
                          <span
                            className={`${styles.rolePill} ${
                              trade.role === 'MAKER' ? styles.roleMaker : styles.roleTaker
                            }`}
                          >
                            {trade.role}
                          </span>
                        </td>
                        <td className={styles.fillReason}>{compactReason(trade.reason)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </section>
        </section>

        <aside className={styles.rightRail}>
          <section className={styles.panel}>
            <div className={styles.panelHeader}>
              <div>
                <div className={styles.panelEyebrow}>Watchlist</div>
                <h2 className={styles.panelTitle}>Markets</h2>
              </div>
              <div className={styles.fillsNote}>
                <span className={styles.countPill}>{markets.length}</span>
              </div>
            </div>

            {markets.length === 0 ? (
              <div className={styles.emptyState}>
                <div className={styles.emptySymbol}>·</div>
                <p>Discovery is still warming up.</p>
              </div>
            ) : (
              <div className={styles.marketList}>
                {visibleMarkets.map((market) => (
                  <a
                    key={market.condition_id}
                    className={styles.marketItem}
                    href={`https://polymarket.com/event/${encodeURIComponent(market.slug)}`}
                    target="_blank"
                    rel="noreferrer"
                  >
                    <div className={styles.marketItemTop}>
                      <span className={styles.marketSlug}>{compactSlug(market.slug)}</span>
                      <span className={styles.marketMeta}>tick {market.tick_size}</span>
                    </div>
                    <div className={styles.marketQuestion}>
                      {market.question.length > 58 ? `${market.question.slice(0, 58)}…` : market.question}
                    </div>
                    <div className={styles.marketMeta}>min {market.min_order_size} sh</div>
                  </a>
                ))}
              </div>
            )}
          </section>

          <section className={styles.panel}>
            <div className={styles.panelHeader}>
              <div>
                <div className={styles.panelEyebrow}>Activity</div>
                <h2 className={styles.panelTitle}>Recent tape</h2>
              </div>
              <div className={styles.panelMeta}>{formatTime(nowMs)}</div>
            </div>

            {trades.length === 0 ? (
              <div className={styles.emptyState}>
                <div className={styles.emptySymbol}>·</div>
                <p>The tape will populate as soon as the first fill lands.</p>
              </div>
            ) : (
              <div className={styles.activityList}>
                {trades.slice(0, 8).map((trade, index) => (
                  <div key={`${trade.ts_ms}-${trade.order_id ?? 'x'}-${index}`} className={styles.activityItem}>
                    <div className={styles.activityTop}>
                      <span className={styles.activitySymbol}>{compactSlug(trade.slug)}</span>
                      <span className={styles.activityTime}>{formatTime(trade.ts_ms)}</span>
                    </div>
                    <div className={styles.activityMeta}>
                      <span
                        className={`${styles.sidePill} ${
                          trade.side === 'BUY' ? styles.sideBuy : styles.sideSell
                        }`}
                      >
                        {trade.side}
                      </span>
                      <span
                        className={`${styles.rolePill} ${
                          trade.role === 'MAKER' ? styles.roleMaker : styles.roleTaker
                        }`}
                      >
                        {trade.role}
                      </span>
                      <span>{trade.price}</span>
                      <span>{trade.size} sh</span>
                    </div>
                    <div className={styles.activityReason}>{compactReason(trade.reason)}</div>
                  </div>
                ))}
              </div>
            )}
          </section>
        </aside>
      </main>
    </div>
  )
}
