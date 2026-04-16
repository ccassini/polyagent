/** Parse Prometheus text exposition (subset: unlabeled lines). */

export type ParsedMetrics = Map<string, number>

export function parsePrometheusText(input: string): ParsedMetrics {
  const map = new Map<string, number>()
  for (const line of input.split('\n')) {
    const t = line.trim()
    if (!t || t.startsWith('#')) continue
    const parts = t.split(/\s+/)
    if (parts.length < 2) continue
    const nameWithLabels = parts[0]
    const valueRaw = parts[1]
    const v = Number.parseFloat(valueRaw)
    if (!Number.isFinite(v)) continue
    const name = nameWithLabels.split('{')[0] ?? nameWithLabels
    map.set(name, v)
  }
  return map
}

function pick(map: ParsedMetrics, names: string[]): number | undefined {
  for (const n of names) {
    const v = map.get(n)
    if (v !== undefined) return v
  }
  return undefined
}

export type AgentMetricSnapshot = {
  makerReportsTotal?: number
  takerReportsTotal?: number
  makerRatio?: number
  signalEdge?: number
  chainlinkPythBasis?: number
  adverseSelection?: number
  decisionCycleMs?: number
  queueLen?: number
  eventsIngestedTotal?: number
}

export function snapshotFromPrometheus(map: ParsedMetrics): AgentMetricSnapshot {
  const makerReportsTotal = pick(map, [
    'polyhft_maker_reports_total',
    'polyhft_maker_reports_total_total',
  ])
  const takerReportsTotal = pick(map, [
    'polyhft_taker_reports_total',
    'polyhft_taker_reports_total_total',
  ])

  let makerRatio = pick(map, ['polyhft_maker_ratio'])
  if (makerRatio === undefined && makerReportsTotal !== undefined && takerReportsTotal !== undefined) {
    const sum = makerReportsTotal + takerReportsTotal
    if (sum > 0) makerRatio = makerReportsTotal / sum
  }

  return {
    makerReportsTotal,
    takerReportsTotal,
    makerRatio,
    signalEdge: pick(map, ['polyhft_signal_edge']),
    chainlinkPythBasis: pick(map, ['polyhft_chainlink_pyth_basis']),
    adverseSelection: pick(map, ['polyhft_adverse_selection']),
    decisionCycleMs: pick(map, ['polyhft_decision_cycle_ms']),
    queueLen: pick(map, ['polyhft_event_queue_len']),
    eventsIngestedTotal: pick(map, [
      'polyhft_events_ingested_total',
      'polyhft_events_ingested_total_total',
    ]),
  }
}
