# Algo Stabilization Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Stop repeated low-quality taker entries from destroying PnL in both paper and live modes.

**Architecture:** Add a shared taker cooldown gate in `RiskManager` so paper-mode simulated fills and live execution both respect the same anti-spam rules. Keep execution-layer cooldown logic aligned with token-level semantics and log effective strategy config at startup for faster operator diagnosis.

**Tech Stack:** Rust, Cargo workspace, DashMap, existing config/types infrastructure

---

### Task 1: Document the stabilization approach

**Files:**
- Create: `docs/plans/2026-04-15-algo-stabilization-design.md`
- Create: `docs/plans/2026-04-15-algo-stabilization-implementation.md`

**Step 1: Write the design and implementation docs**

Capture the root cause, the first-iteration scope, and the exact files that will change.

**Step 2: Verify the plan files exist**

Run: `rg --files docs/plans`
Expected: both `2026-04-15-algo-stabilization-*.md` files are listed.

### Task 2: Add failing cooldown regression tests

**Files:**
- Modify: `crates/risk/src/lib.rs`

**Step 1: Write a failing test for repeated same-leg takers**

Add a test proving that two buy takers on the same token across nearby decision ticks should not both pass risk filtering.

**Step 2: Run the targeted test and verify it fails**

Run: `cargo test -p polyhft-risk taker_cooldown -- --nocapture`
Expected: FAIL because the risk layer currently allows both entries.

**Step 3: Write a failing test for paired-leg preservation**

Add a test proving YES and NO takers in the same market can still both pass in one batch when they are on different tokens.

### Task 3: Implement shared taker anti-spam controls

**Files:**
- Modify: `crates/risk/src/lib.rs`
- Modify: `crates/execution/src/lib.rs`

**Step 1: Add shared cooldown state to risk**

- Store the configured taker cooldown duration from `AppConfig.execution.taker_rebalance_cooldown_ms`.
- Track last accepted taker timestamp per token.
- Apply cooldown before accepting same-token taker buys.

**Step 2: Align execution-layer cooldown semantics**

- Use token-level cooldown tracking in live execution as well so spread-farm / paired-leg batches are not accidentally blocked at the condition level.

**Step 3: Run the targeted risk tests**

Run: `cargo test -p polyhft-risk taker_cooldown -- --nocapture`
Expected: PASS

### Task 4: Surface effective runtime config

**Files:**
- Modify: `apps/live-agent/src/main.rs`

**Step 1: Log directional/taker runtime knobs at startup**

Emit an operator-facing startup log with the config path and key values such as directional taker enablement, bias threshold, ask-sum gate, quote size, directional size, and taker cooldown.

**Step 2: Verify the workspace still compiles**

Run: `cargo check -p polyhft-live-agent -p polyhft-risk`
Expected: success

### Task 5: Final verification

**Files:**
- Modify: `crates/risk/src/lib.rs`
- Modify: `crates/execution/src/lib.rs`
- Modify: `apps/live-agent/src/main.rs`

**Step 1: Run targeted tests**

Run: `cargo test -p polyhft-risk`
Expected: PASS

**Step 2: Run compile verification**

Run: `cargo check -p polyhft-live-agent -p polyhft-risk -p polyhft-execution`
Expected: success

**Step 3: Report evidence**

Summarize the commands run and the observed results. If runtime log inspection is not performed with a real agent process, state that clearly.
