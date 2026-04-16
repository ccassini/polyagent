import { defineConfig, type Plugin } from 'vite'
import react from '@vitejs/plugin-react'

const proxyTarget = (process.env.VITE_PROXY_PROMETHEUS ?? 'http://127.0.0.1:9901').replace(
  /\/$/,
  '',
)
const panelTarget = (process.env.VITE_PROXY_DASHBOARD ?? 'http://127.0.0.1:3001').replace(
  /\/$/,
  '',
)

const DEFAULT_ACTIVITY = 'http://127.0.0.1:9902'

/** When unset, we probe 9902..9930 so a second agent on a fallback port still works. */
let activityBaseCache: string | null =
  process.env.VITE_PROXY_ACTIVITY?.replace(/\/$/, '') ?? null
let activityProbePromise: Promise<string> | null = null

async function resolveActivityBase(): Promise<string> {
  const fromEnv = process.env.VITE_PROXY_ACTIVITY?.replace(/\/$/, '')
  if (fromEnv) return fromEnv
  if (activityBaseCache) return activityBaseCache
  if (!activityProbePromise) {
    activityProbePromise = (async () => {
      for (let p = 9902; p <= 9930; p++) {
        const base = `http://127.0.0.1:${p}`
        try {
          const ac = new AbortController()
          const timer = setTimeout(() => ac.abort(), 1500)
          const r = await fetch(`${base}/api/activity`, { signal: ac.signal })
          clearTimeout(timer)
          if (r.ok) {
            activityBaseCache = base
            return base
          }
        } catch {
          /* try next port */
        }
      }
      return DEFAULT_ACTIVITY
    })().finally(() => {
      activityProbePromise = null
    })
  }
  const resolved = await activityProbePromise
  return activityBaseCache ?? resolved
}

const METRICS_FALLBACK =
  '# polyhft: metrics backend unreachable (agent starting, retrying discovery, or stopped)\n'
const PANEL_FALLBACK = {
  last_update_ms: Date.now(),
  feed: {
    binance: { value: null, source_ts_ms: null, observed_ts_ms: null, stale: true },
    coinbase: { value: null, source_ts_ms: null, observed_ts_ms: null, stale: true },
    pyth: { value: null, source_ts_ms: null, observed_ts_ms: null, stale: true },
    chainlink: { value: null, source_ts_ms: null, observed_ts_ms: null, stale: true },
    blended: { value: null, source_ts_ms: null, observed_ts_ms: null, stale: true },
  },
  errors: ['panel API unreachable (dashboard starting or stopped)'],
}

/** Avoid Vite's http-proxy (it logs ECONNREFUSED on every poll). Fetch in middleware instead. */
function metricsMiddleware(): Plugin {
  return {
    name: 'polyhft-metrics-fetch',
    configureServer(server) {
      server.middlewares.use('/metrics', async (_req, res) => {
        const url = `${proxyTarget}/metrics`
        try {
          const ac = new AbortController()
          const timer = setTimeout(() => ac.abort(), 8_000)
          const r = await fetch(url, { signal: ac.signal })
          clearTimeout(timer)
          const text = await r.text()
          res.statusCode = r.ok ? 200 : r.status
          const ct = r.headers.get('content-type') ?? 'text/plain; version=0.0.4; charset=utf-8'
          res.setHeader('Content-Type', ct)
          res.end(text)
        } catch {
          res.statusCode = 200
          res.setHeader('Content-Type', 'text/plain; version=0.0.4; charset=utf-8')
          res.end(METRICS_FALLBACK)
        }
      })

      // Proxy panel endpoints from Rust dashboard (3001) to avoid CORS and keep one origin.
      // /panel-api/snapshot -> http://127.0.0.1:3001/api/panel/snapshot
      server.middlewares.use('/panel-api/', async (req, res) => {
        const path = (req.url ?? '/').replace(/^\//, '')
        const url = `${panelTarget}/api/panel/${path}`

        try {
          const ac = new AbortController()
          const timer = setTimeout(() => ac.abort(), 8_000)
          const r = await fetch(url, { signal: ac.signal })
          clearTimeout(timer)
          const text = await r.text()
          res.statusCode = r.ok ? 200 : r.status
          res.setHeader('Content-Type', 'application/json; charset=utf-8')
          res.end(text)
        } catch {
          res.statusCode = 200
          res.setHeader('Content-Type', 'application/json; charset=utf-8')
          res.end(JSON.stringify({ ...PANEL_FALLBACK, last_update_ms: Date.now() }))
        }
      })

      // Proxy all /api/* routes to the agent activity API (9902+).
      // This covers /api/activity, /api/orders, /api/trade, /api/orders/cancel, etc.
      server.middlewares.use('/api/', async (req, res) => {
        const base = await resolveActivityBase()
        const path = req.url ?? '/'
        const url = `${base}/api/${path.replace(/^\//, '')}`

        // Collect request body for POST/PUT methods.
        const method = req.method ?? 'GET'
        let body: Buffer | undefined
        if (method !== 'GET' && method !== 'HEAD') {
          body = await new Promise<Buffer>((resolve, reject) => {
            const chunks: Buffer[] = []
            req.on('data', (c: Buffer) => chunks.push(c))
            req.on('end', () => resolve(Buffer.concat(chunks)))
            req.on('error', reject)
          })
        }

        try {
          const ac = new AbortController()
          const timer = setTimeout(() => ac.abort(), 8_000)
          const r = await fetch(url, {
            method,
            headers: { 'Content-Type': 'application/json' },
            body: body?.length ? body : undefined,
            signal: ac.signal,
          })
          clearTimeout(timer)
          const text = await r.text()
          res.statusCode = r.ok ? 200 : r.status
          res.setHeader('Content-Type', 'application/json; charset=utf-8')
          res.end(text)
        } catch {
          // Activity endpoint: return a graceful empty response so the UI doesn't crash.
          if (path.startsWith('/activity') || path === '/') {
            res.statusCode = 200
            res.setHeader('Content-Type', 'application/json; charset=utf-8')
            res.end(
              JSON.stringify({
                server_time_ms: Date.now(),
                market_count: 0,
                markets: [],
                trades: [],
                stats: { maker_fills_session: 0, taker_fills_session: 0, trades_in_buffer: 0 },
                error: 'activity API unreachable (agent starting or stopped)',
              }),
            )
          } else {
            res.statusCode = 503
            res.setHeader('Content-Type', 'application/json; charset=utf-8')
            res.end(JSON.stringify({ ok: false, error: 'agent unreachable' }))
          }
        }
      })
    },
  }
}

// https://vite.dev/config/
export default defineConfig({
  plugins: [react(), metricsMiddleware()],
  server: {
    port: Number(process.env.VITE_DEV_PORT ?? 3000),
    strictPort: false,
    host: 'localhost',
  },
  preview: {
    port: Number(process.env.VITE_DEV_PORT ?? 3000),
    strictPort: false,
    host: 'localhost',
  },
})
