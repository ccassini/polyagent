# Algo Stabilization Design

## Goal

Stop immediate PnL bleed in the Polymarket BTC/ETH/SOL short-duration strategy before attempting more ambitious alpha improvements.

## Root Cause Summary

Current paper logs show repeated taker executions on the same market and leg at sub-second cadence. The strategy is paying fees over and over on near-identical directional signals, especially in paper mode where the existing execution-layer taker cooldown is bypassed.

The main failure modes are:

- directional taker entries are emitted too often on nearly unchanged signals,
- paper mode does not inherit the execution-layer taker cooldown,
- live execution cooldown is keyed too coarsely by condition, which can interfere with legitimate paired-leg strategies,
- effective runtime configuration is not surfaced clearly enough to detect wrong presets quickly.

## First Iteration Scope

This iteration focuses on "stop the bleeding", not on inventing a new alpha model.

### Shared taker cooldown

Move a cooldown gate to a shared pre-execution layer so both paper mode and live mode respect it. The cooldown should be keyed by token rather than condition so repeated same-leg entries are suppressed while legitimate YES+NO paired entries remain possible.

### Duplicate taker suppression

Reject repeated same-token takers inside the cooldown window before they reach paper simulation or live execution.

### Runtime config visibility

Log the effective directional/taker configuration at startup so it is obvious which thresholds the process is actually using.

## Non-Goals

- No new predictive model.
- No ML feature redesign.
- No deep fair-value overhaul.
- No major market-discovery refactor.

Those can come after the current engine is made sane.
