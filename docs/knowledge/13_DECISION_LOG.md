# Architecture Decision Log

This is not a changelog. It records decisions so later agents do not re-litigate them without new evidence.

## ADR-001 — Correctness before aggressiveness
**Status:** Accepted  
**Date:** 2026-08-27

Do not lower thresholds or increase sizing until unit, market-state, execution, reconciliation, and exit truth are corrected.

## ADR-002 — Two-model decision structure
**Status:** Accepted direction

Separate:
1. survival/catastrophic risk;
2. opportunity/expected net return.

Fatal risk remains outside weighted opportunity scoring.

## ADR-003 — Graduation is not the primary target
**Status:** Accepted direction

Optimize short-horizon executable net outcomes, not eventual token graduation.

## ADR-004 — Strong domain units
**Status:** Accepted

Replace ambient numeric interpretation with explicit units at boundaries.

## ADR-005 — Single canonical market state
**Status:** Accepted

All trade events update one per-token state. Kill switches, models and scoring consume it rather than independently reinterpreting provider events.

## ADR-006 — Single authoritative live exit engine
**Status:** Accepted

Consolidate strategy exit, position auto-sell, and inline CLI monitor behavior.

## ADR-007 — Reconciled fills define position/P&L truth
**Status:** Accepted

Estimated market cap/reserve prices are not cost-basis truth.

## ADR-008 — Missed entry can be preferable to bad fill
**Status:** Accepted

Do not widen entry slippage until a transaction lands. If the quote destroys expected edge, abandon entry.

## ADR-009 — Implementation agents receive closed specifications
**Status:** Accepted

Architecture/review layer does exploration and system reasoning. Coding agents receive exact base SHA, line ranges, changes, non-changes, invariants, tests and report format.

## ADR-010 — Static knowledge docs use anchors, implementation packets use exact lines
**Status:** Accepted

Line numbers drift after every patch. Knowledge docs reference stable path/type/function concepts. Each new packet must recalculate exact line ranges against its pinned base SHA.

## ADR-011 — Jito ShredStream is not a new canonical dependency
**Status:** Accepted

Official Jito docs publish service shutdown on 2026-09-05. Revalidate migration options before implementing a replacement.

## ADR-012 — Provider abstraction
**Status:** Accepted

Internal state is provider-neutral. PumpPortal, Helius, RPC, or future feeds are adapters.

## ADR-013 — Mayhem is an explicit regime
**Status:** Accepted direction

Do not train ordinary organic-volume behavior on Mayhem activity without a regime flag or separate model.

## ADR-014 — Boosted trees before neural model
**Status:** Current modeling preference, not permanent law

Structured features, missingness and rapid iteration favor CatBoost/LightGBM/XGBoost for the first validated predictive baseline. Revisit only after dataset/replay maturity.

## ADR-015 — No unvalidated randomization in capital-critical decisions
**Status:** Accepted

Randomly skipping valid entries, changing risk size, or adding entry latency is not considered adversarial protection by default. Such behavior must demonstrate positive net value in replay/shadow tests before activation.

## ADR-016 — New-file existence does not imply runtime readiness
**Status:** Accepted

Every added subsystem must be classified by runtime wiring and input truth. A registered/constructed module with missing or synthetic inputs remains dormant/unsafe, not “implemented”.
