# Pumpfun Sniper — Agent Entry Contract

> **Knowledge-layer version:** 1.0  
> **Repository:** `Harmonyone1/pumpfun-sniper`  
> **Baseline commit inspected:** `2158c64426de0f0c532221c578348c1dad1e075d`  
> **Baseline verification date:** 2026-08-27  
> **Status:** The baseline is the current `master` at the time this layer was created. Revalidate after every merge.

This file is the mandatory starting point for any coding agent working on this repository.

## 1. Operating rule

**Do not explore the repository broadly unless the implementation packet explicitly authorizes exploration.**

The architecture/review layer is responsible for deciding what should change. The coding agent is responsible for making the specified patch, running the specified checks, and reporting evidence.

If an implementation packet conflicts with this knowledge layer:

1. Stop the conflicting edit.
2. Report the exact conflict.
3. Do not invent a compromise.
4. The architecture/review layer must update either the packet or the canonical knowledge.

## 2. Never infer canonical behavior from legacy code

This repository contains:
- working code,
- partially integrated code,
- stale comments,
- dormant subsystems,
- duplicated decision/exit logic,
- contradictory unit assumptions,
- placeholder market data,
- historical experiments.

Therefore **existing behavior is not automatically a requirement**.

Canonical behavior is defined under `docs/knowledge/`.

## 3. Minimal-context routing

Read only the common files plus the subsystem-specific files below.

### Always read
1. `docs/knowledge/00_INDEX.md`
2. `docs/knowledge/01_SYSTEM_CHARTER.md`
3. `docs/knowledge/02_CANONICAL_INVARIANTS.md`
4. The implementation packet supplied for the task.

### If editing market ingestion / PumpPortal / Helius / stream code
Read:
- `05_MARKET_DATA.md`
- `06_DOMAIN_TYPES_AND_UNITS.md`
- `07_RUNTIME_FLOWS.md`
- `14_EXTERNAL_PROTOCOL_REGISTRY.md`

### If editing filtering / scoring / token selection / model features
Read:
- `08_ENTRY_AND_MODEL.md`
- `05_MARKET_DATA.md`
- `06_DOMAIN_TYPES_AND_UNITS.md`
- `11_KNOWN_DEFECTS.md`

### If editing buy/sell execution
Read:
- `09_EXECUTION_AND_RECONCILIATION.md`
- `06_DOMAIN_TYPES_AND_UNITS.md`
- `10_POSITION_RISK_AND_EXIT.md`
- `14_EXTERNAL_PROTOCOL_REGISTRY.md`

### If editing positions / P&L / stop loss / take profit / kill switches
Read:
- `10_POSITION_RISK_AND_EXIT.md`
- `09_EXECUTION_AND_RECONCILIATION.md`
- `06_DOMAIN_TYPES_AND_UNITS.md`

### If editing strategy engine / sizing / regime / fatal risk
Read:
- `08_ENTRY_AND_MODEL.md`
- `10_POSITION_RISK_AND_EXIT.md`
- `03_CURRENT_REPOSITORY_MAP.md`
- `11_KNOWN_DEFECTS.md`

### If editing wallets / multi-wallet / vault
Read:
- `10_POSITION_RISK_AND_EXIT.md`
- `03_CURRENT_REPOSITORY_MAP.md`
- the packet's exact wallet requirements.

### If changing configuration
Read:
- all documents named by the affected subsystem,
- `12_VALIDATION_AND_METRICS.md`,
- `13_DECISION_LOG.md`.

## 4. Implementation-agent prohibitions

Unless the packet explicitly instructs otherwise, do not:
- tune thresholds;
- make the bot more aggressive;
- widen slippage to solve failed fills;
- add fallback buys;
- create a second entry or exit engine;
- silently convert missing data to zero/false;
- reinterpret SOL as lamports or vice versa;
- estimate fills when actual transaction reconciliation is available;
- remove a position merely because a sell failed;
- change capital sizing;
- change wallet selection;
- introduce new provider dependencies;
- revive Jito ShredStream;
- commit credentials, API keys, private keys, or wallet secrets;
- refactor unrelated modules;
- reformat unrelated files;
- change public config names unless instructed;
- claim profitability from unit tests.

## 5. Required completion report

Every implementation response must include:
- base commit SHA actually used;
- resulting commit SHA;
- files changed;
- exact requested changes completed;
- tests/checks run with command and result;
- any packet item not completed;
- any newly discovered contradiction;
- any behavior changed outside the packet (expected answer: none).

Do not replace this evidence with a prose summary.
