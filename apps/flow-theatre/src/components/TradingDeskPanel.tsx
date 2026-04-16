import { useMemo, useRef, useState } from 'react'
import { useActivityFeed, type ActivityMarket, type ActivityTrade } from '../hooks/useActivityFeed'
import styles from './TradingDeskPanel.module.css'

function pmEventUrl(slug: string) {
  const s = encodeURIComponent(slug)
  return `https://polymarket.com/event/${s}`
}

function formatClock(ts: number) {
  if (!ts) return '—'
  return new Date(ts).toLocaleTimeString(undefined, {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  })
}

function relAge(ts: number, serverNow: number) {
  if (!ts || !serverNow) return '—'
  const sec = Math.floor((serverNow - ts) / 1000)
  if (sec < 0) return '0s'
  if (sec < 60) return `${sec}s`
  if (sec < 3600) return `${Math.floor(sec / 60)}m`
  return `${Math.floor(sec / 3600)}h`
}

function formatUsd(val: string | undefined): string {
  if (!val) return '—'
  const n = parseFloat(val)
  if (isNaN(n)) return val
  const abs = Math.abs(n)
  const sign = n < 0 ? '−' : n > 0 ? '+' : ''
  if (abs >= 1_000) return `${sign}$${(abs / 1_000).toFixed(2)}k`
  return `${sign}$${abs.toFixed(2)}`
}

function pnlAttr(pnl: string | undefined): 'pos' | 'neg' | 'zero' {
  if (!pnl) return 'zero'
  const n = parseFloat(pnl)
  if (n > 0.005) return 'pos'
  if (n < -0.005) return 'neg'
  return 'zero'
}

function balancePct(balance: string | undefined, start = 5000): number {
  if (!balance) return 100
  const n = parseFloat(balance)
  if (isNaN(n)) return 100
  return Math.max(0, Math.min(100, (n / start) * 100))
}

type RoleFilter = 'all' | 'MAKER' | 'TAKER'

export function TradingDeskPanel() {
  const { data, error, lastOkAtMs } = useActivityFeed()
  const [q, setQ] = useState('')
  const [roleFilter, setRoleFilter] = useState<RoleFilter>('all')
  const prevTradeCount = useRef(0)

  const markets = data?.markets ?? []
  const trades = data?.trades ?? []
  const serverNow = data?.server_time_ms ?? Date.now()

  const filteredMarkets = useMemo(() => {
    const needle = q.trim().toLowerCase()
    if (!needle) return markets
    return markets.filter(
      (m) =>
        m.slug.toLowerCase().includes(needle) ||
        m.question.toLowerCase().includes(needle) ||
        m.condition_id.toLowerCase().includes(needle),
    )
  }, [markets, q])

  const filteredTrades = useMemo(() => {
    if (roleFilter === 'all') return trades
    return trades.filter((t) => t.role === roleFilter)
  }, [trades, roleFilter])

  // Track how many trades are newly arrived this render cycle.
  const newCount = Math.max(0, filteredTrades.length - prevTradeCount.current)
  prevTradeCount.current = filteredTrades.length

  const stats = data?.stats
  const connOk = !error && data && !data.error

  const paperMode = data?.paper_mode === true
  const paperBalance = data?.paper_balance_usdc
  const paperPnl = data?.paper_pnl_usdc
  const pct = balancePct(paperBalance)

  return (
    <section className={styles.desk} aria-label="Trading activity and scanned markets">
      <header className={styles.head}>
        <div className={styles.headText}>
          <span className={styles.chip}>
            <span className={styles.chipDot} aria-hidden />
            {paperMode ? 'Paper trading' : 'Live book'}
          </span>
          <h2>Trading desk</h2>
          <p>
            {paperMode
              ? 'Simulated fills on real Polymarket order books. No real USDC is spent.'
              : 'Markets the agent subscribed after Gamma discovery, and fills this process reported (maker placement acks + taker FOK + user-channel matches when enabled).'}
          </p>
        </div>
        <div className={styles.stats}>
          <div className={styles.statPill}>
            <span className={styles.statLab}>Markets</span>
            <span className={`${styles.statVal} ${styles.accent}`}>{data?.market_count ?? '—'}</span>
          </div>
          <div className={styles.statPill}>
            <span className={styles.statLab}>Maker</span>
            <span className={styles.statVal}>{stats?.maker_fills_session ?? '—'}</span>
          </div>
          <div className={styles.statPill}>
            <span className={styles.statLab}>Taker</span>
            <span className={styles.statVal}>{stats?.taker_fills_session ?? '—'}</span>
          </div>
          <div className={styles.statPill}>
            <span className={styles.statLab}>Buffer</span>
            <span className={styles.statVal}>{stats?.trades_in_buffer ?? '—'}</span>
          </div>
          <div className={styles.statPill}>
            <span className={styles.statLab}>Volume</span>
            <span className={styles.statVal}>
              {stats?.session_volume_usdc ? formatUsd(stats.session_volume_usdc) : '—'}
            </span>
          </div>
          <div
            className={styles.badge}
            data-state={connOk ? 'ok' : 'err'}
            title={error ?? data?.error ?? undefined}
          >
            <span className={styles.dot} />
            {connOk
              ? `activity API · ${lastOkAtMs ? `${Math.max(0, Math.floor((Date.now() - lastOkAtMs) / 1000))}s` : 'live'}`
              : error ?? data?.error ?? 'offline'}
          </div>
        </div>
      </header>

      {paperMode && (
        <div className={styles.paperBar} role="status" aria-label="Paper trading balance">
          <span className={styles.paperBadge}>
            <svg width="10" height="10" viewBox="0 0 10 10" fill="none" aria-hidden>
              <circle cx="5" cy="5" r="4" stroke="currentColor" strokeWidth="1.5" />
              <path d="M5 3v2.5l1.5 1" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
            </svg>
            Paper
          </span>
          <div className={styles.paperMetrics}>
            <div className={styles.paperMetric}>
              <span className={styles.paperMetricLab}>Balance</span>
              <span className={styles.paperMetricVal}>
                {paperBalance ? `$${parseFloat(paperBalance).toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}` : '$5,000.00'}
              </span>
            </div>
            <div className={styles.paperMetric}>
              <span className={styles.paperMetricLab}>P&amp;L</span>
              <span className={styles.paperMetricVal} data-pnl={pnlAttr(paperPnl)}>
                {formatUsd(paperPnl)}
              </span>
            </div>
            <div className={styles.paperMetric}>
              <span className={styles.paperMetricLab}>Volume</span>
              <span className={styles.paperMetricVal}>
                {stats?.session_volume_usdc ? formatUsd(stats.session_volume_usdc) : '—'}
              </span>
            </div>
            <div className={`${styles.paperMetric} ${styles.balanceBar}`}>
              <span className={styles.paperMetricLab}>Balance remaining</span>
              <div className={styles.balanceTrack} title={`${pct.toFixed(1)}% of starting $5,000`}>
                <div
                  className={styles.balanceFill}
                  style={{ width: `${pct}%` }}
                  data-warn={pct < 50 && pct >= 20 ? 'true' : undefined}
                  data-danger={pct < 20 ? 'true' : undefined}
                />
              </div>
            </div>
          </div>
        </div>
      )}

      <div className={styles.grid}>
        <div className={styles.col}>
          <div className={styles.colHead}>
            <h3 className={styles.colTitle}>Scanned markets ({filteredMarkets.length})</h3>
            <input
              className={styles.search}
              type="search"
              placeholder="Filter slug / question…"
              value={q}
              onChange={(e) => setQ(e.target.value)}
              aria-label="Filter markets"
            />
          </div>
          <div className={styles.scroll}>
            {filteredMarkets.length === 0 ? (
              <div className={styles.empty}>
                <strong>No markets</strong>
                <br />
                Agent discovery may still be running, or filters exclude everything.
              </div>
            ) : (
              <table className={styles.table}>
                <thead>
                  <tr>
                    <th>Slug / link</th>
                    <th>Min · tick</th>
                  </tr>
                </thead>
                <tbody>
                  {filteredMarkets.map((m: ActivityMarket) => (
                    <tr key={m.condition_id}>
                      <td>
                        <div className={styles.slug}>
                          <a
                            className={styles.extLink}
                            href={pmEventUrl(m.slug)}
                            target="_blank"
                            rel="noreferrer"
                          >
                            {m.slug}
                          </a>
                        </div>
                        <div className={styles.q}>{m.question}</div>
                        <div className={styles.meta}>{m.condition_id.slice(0, 18)}…</div>
                      </td>
                      <td>
                        {m.min_order_size} sh
                        <br />
                        <span className={styles.meta}>tick {m.tick_size}</span>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        </div>

        <div className={styles.col}>
          <div className={styles.colHead}>
            <h3 className={styles.colTitle}>Session fills ({filteredTrades.length})</h3>
            <div className={styles.filters}>
              {(['all', 'MAKER', 'TAKER'] as const).map((r) => (
                <button
                  key={r}
                  type="button"
                  className={styles.filterBtn}
                  data-on={roleFilter === r ? 'true' : 'false'}
                  onClick={() => setRoleFilter(r)}
                >
                  {r}
                </button>
              ))}
            </div>
          </div>
          <div className={styles.scroll}>
            {filteredTrades.length === 0 ? (
              <div className={styles.empty}>
                <strong>No fills in buffer yet</strong>
                <br />
                {paperMode
                  ? 'Waiting for signal — Pyth price feeds must arrive and bias must exceed the configured threshold.'
                  : 'Maker quotes need matching counterparty; enable user WS for more match visibility.'}
              </div>
            ) : (
              <table className={styles.table}>
                <thead>
                  <tr>
                    <th>Time</th>
                    <th>Market</th>
                    <th>Leg</th>
                    <th>Side</th>
                    <th>Px · sz</th>
                    <th>Role</th>
                    <th>Note</th>
                  </tr>
                </thead>
                <tbody>
                  {filteredTrades.map((t: ActivityTrade, i: number) => (
                    <tr
                      key={`${t.ts_ms}-${t.order_id ?? 'x'}-${i}`}
                      className={i < newCount ? styles.rowNew : undefined}
                    >
                      <td className={styles.time}>
                        {formatClock(t.ts_ms)}
                        <div className={styles.meta}>{relAge(t.ts_ms, serverNow)}</div>
                      </td>
                      <td>
                        <div className={styles.slug}>
                          <a
                            className={styles.extLink}
                            href={pmEventUrl(t.slug)}
                            target="_blank"
                            rel="noreferrer"
                          >
                            {t.slug}
                          </a>
                        </div>
                        <div className={styles.meta}>{t.question}</div>
                      </td>
                      <td className={styles.leg}>{t.leg}</td>
                      <td>
                        <span className={t.side === 'BUY' ? styles.pillBuy : styles.pillSell}>
                          {t.side}
                        </span>
                      </td>
                      <td>
                        {t.price}
                        <br />
                        <span className={styles.meta}>{t.size} sh</span>
                      </td>
                      <td>
                        <span className={t.role === 'MAKER' ? styles.pillMaker : styles.pillTaker}>
                          {t.role}
                        </span>
                      </td>
                      <td>
                        <div className={styles.reason} title={t.reason}>
                          {t.reason}
                        </div>
                        {t.order_id ? (
                          <div className={styles.meta} title={t.order_id}>
                            {t.order_id.slice(0, 14)}…
                          </div>
                        ) : null}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        </div>
      </div>
    </section>
  )
}
