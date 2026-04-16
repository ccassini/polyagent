# Flow Dashboard Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Turn the `flow-theatre` dashboard into a black, ultra-wide operator cockpit with clearer hierarchy and simpler panel grouping.

**Architecture:** Keep existing polling hooks and Theatre-driven accent values intact, and rebuild only the presentation layer in `FlowDashboard` plus the supporting global/CSS module styles. The new layout should use a three-column cockpit structure with responsive collapse behavior and a stronger visual system for large displays.

**Tech Stack:** React 19, TypeScript, Vite, CSS Modules, Theatre.js state sync

---

### Task 1: Save the approved design context

**Files:**
- Create: `docs/plans/2026-04-15-flow-dashboard-design.md`
- Create: `docs/plans/2026-04-15-flow-dashboard-implementation.md`

**Step 1: Write the design and implementation documents**

Capture the approved operator-cockpit direction, scope, and constraints in the plan files above.

**Step 2: Verify the files exist**

Run: `rg --files docs/plans`
Expected: both `2026-04-15-flow-dashboard-*.md` files are listed.

**Step 3: Commit**

If the workspace has git metadata, commit the plan files. If it does not, note that the repo is not initialized here and continue.

### Task 2: Rebuild the dashboard component structure

**Files:**
- Modify: `apps/flow-theatre/src/components/FlowDashboard.tsx`

**Step 1: Capture a baseline build**

Run: `npm run build`
Expected: existing app builds successfully before the refactor.

**Step 2: Rewrite the component layout**

- Replace the top-level stacked layout with a three-column cockpit.
- Keep the existing data hooks and derived values.
- Add grouped sections for command strip, system rail, center stage, market watch, and activity tape.

**Step 3: Re-run the build**

Run: `npm run build`
Expected: the component refactor compiles cleanly.

### Task 3: Rebuild the visual system

**Files:**
- Modify: `apps/flow-theatre/src/components/FlowDashboard.module.css`
- Modify: `apps/flow-theatre/src/index.css`

**Step 1: Replace the old dashboard skin**

- Introduce black graphite theme tokens.
- Increase horizontal breathing room.
- Reduce card clutter and use stronger typography.
- Improve table, list, and panel styling for wide screens.

**Step 2: Verify the app still builds**

Run: `npm run build`
Expected: CSS and JSX compile together without module/class-name errors.

**Step 3: Check the rendered UI**

Run a local preview and verify that:

- the command strip stays compact,
- the center fills surface remains dominant,
- the right rail shows markets and recent fills clearly,
- the layout collapses without overlap on narrower widths.

### Task 4: Final verification

**Files:**
- Modify: `apps/flow-theatre/src/components/FlowDashboard.tsx`
- Modify: `apps/flow-theatre/src/components/FlowDashboard.module.css`
- Modify: `apps/flow-theatre/src/index.css`

**Step 1: Run the final build**

Run: `npm run build`
Expected: success with exit code `0`.

**Step 2: Summarize actual verification evidence**

Report which commands ran, whether build passed, and whether live preview/render inspection was completed.

**Step 3: Commit**

If git metadata is available, create a focused commit for the dashboard refactor. If not, state that no commit could be created from this workspace.
