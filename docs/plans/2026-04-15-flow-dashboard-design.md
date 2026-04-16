# Flow Dashboard Cockpit Design

## Goal

Rebuild the existing `flow-theatre` dashboard into a black, ultra-wide operator cockpit that is easier to scan on a large personal display and feels more refined than a generic developer dashboard.

## Intent

- Prioritize wide-screen readability over dense card grids.
- Keep the existing data model and polling hooks unchanged.
- Reduce visual clutter while preserving live-trading context.
- Make the interface feel niche and technical without leaning into noisy "cyberpunk" styling.

## Layout

The screen is organized into three columns:

1. Left rail: system health, signal surface, and engine telemetry.
2. Center stage: the main operating area with account/session summary, execution pipeline, and the fills table.
3. Right rail: market watch and a compact activity tape for recent fills.

This layout is optimized for `1440px+` displays and collapses progressively for smaller screens.

## Visual Direction

- Black graphite base with subtle mint accenting from Theatre-controlled theme color.
- Large quiet panels instead of many small cards.
- Strong numeric typography for key values, muted secondary copy, minimal borders.
- Subtle depth through layered gradients, glow, and grid/noise textures.
- Motion stays present through small pulses and stage highlighting, not through constant decorative animation.

## Component Changes

### Command Strip

- Replace the current top bar with a slimmer command strip.
- Keep only the most important operator metrics visible at all times.
- Surface metrics status and activity status as compact badges.

### System Rail

- Group latency, queue, maker share, and basis into one health-focused panel.
- Add a lightweight signal meter and queue pressure meter.
- Move metrics feed output into its own quieter telemetry panel.

### Center Stage

- Keep paper/live mode handling intact.
- Convert the balance / PnL area into a more premium summary block.
- Present the execution pipeline as large stage cards rather than a thin strip.
- Keep the fills table as the dominant data surface.

### Right Rail

- Show a condensed market watch with question snippets and venue parameters.
- Add a compact recent activity tape for the latest fills.

## Constraints

- No changes to backend logic, hooks, or data contracts.
- No new product behavior.
- Verification should rely on `npm run build` and live render validation because the app does not currently ship with a frontend test harness.
