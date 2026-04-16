import { getProject, types as t } from '@theatre/core'

export const FLOW_PROJECT_ID = 'polyhft-flow'

const project = getProject(FLOW_PROJECT_ID)

export const flowSheet = project.sheet('Pipeline', 'live')

/** Code-driven sim values; updated each frame from the dashboard. */
export const simObject = flowSheet.object('Simulation', {
  packetPhase: t.number(0, { range: [0, 1], label: 'Packet position' }),
  wave: t.number(0, { range: [0, 1], label: 'Ambient wave' }),
  scanSweep: t.number(0, { range: [0, 1], label: 'Scan beam' }),
  throughput: t.number(1840, { range: [0, 8000], label: 'Events / s' }),
  latencyMs: t.number(8.5, { range: [0, 120], label: 'Decision ms' }),
  queueDepth: t.number(4, { range: [0, 64], label: 'Queue depth' }),
  spineGlow: t.number(0.35, { range: [0, 1], label: 'Spine intensity' }),
  makerRatio: t.number(0.58, { range: [0, 1], label: 'Maker ratio' }),
  basisBp: t.number(2.4, { range: [-50, 50], label: 'Basis bp' }),
})

/** Tweakable in Theatre Studio (colors, layout accents). */
export const themeObject = flowSheet.object('Theme', {
  accent: t.rgba({ r: 0, g: 230, b: 255, a: 1 }, { label: 'Accent' }),
  spine: t.rgba({ r: 40, g: 120, b: 255, a: 0.55 }, { label: 'Spine' }),
  nodeIdle: t.rgba({ r: 28, g: 32, b: 42, a: 1 }, { label: 'Node idle' }),
  nodeHot: t.rgba({ r: 0, g: 200, b: 255, a: 0.22 }, { label: 'Node hot' }),
})

export { project }
