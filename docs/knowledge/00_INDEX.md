# Knowledge Layer Index

**Purpose:** give humans and implementation agents a single canonical description of the Pump.fun sniper system without forcing repeated repository exploration.

**Pinned baseline:** `2158c64426de0f0c532221c578348c1dad1e075d`  
**Verified:** 2026-08-27

## Document map

| File | Purpose |
|---|---|
| `01_SYSTEM_CHARTER.md` | Mission, optimization objective, definitions of success and non-goals |
| `02_CANONICAL_INVARIANTS.md` | Rules that may not be violated by implementation |
| `03_CURRENT_REPOSITORY_MAP.md` | What exists today and which modules are live, dormant, duplicated, or suspect |
| `04_TARGET_ARCHITECTURE.md` | Canonical architecture the codebase is being migrated toward |
| `05_MARKET_DATA.md` | Event sources, candidate state, feature timing, provenance, freshness |
| `06_DOMAIN_TYPES_AND_UNITS.md` | SOL/lamport/token/price/percentage type contract |
| `07_RUNTIME_FLOWS.md` | New-token, observation, entry, buy, fill, position, exit, reconciliation flows |
| `08_ENTRY_AND_MODEL.md` | Risk model, opportunity model, candidate lifecycle, labels, feature families |
| `09_EXECUTION_AND_RECONCILIATION.md` | Quote, slippage, priority, route, pending tx, fill truth, failure handling |
| `10_POSITION_RISK_AND_EXIT.md` | Position state machine, portfolio risk, exit hierarchy, wallet ownership |
| `11_KNOWN_DEFECTS.md` | Baseline defects and migration hazards that agents must not preserve |
| `12_VALIDATION_AND_METRICS.md` | Replay, shadow trading, metrics, acceptance and promotion gates |
| `13_DECISION_LOG.md` | Canonical architectural decisions and rejected alternatives |
| `14_EXTERNAL_PROTOCOL_REGISTRY.md` | Time-sensitive Pump/PumpPortal/Jito/Helius facts and revalidation sources |
| `15_IMPLEMENTATION_PACKET_TEMPLATE.md` | Exact format for agent handoffs |
| `16_REVALIDATION_CHECKLIST.md` | What must be rechecked after a merge or external API/protocol change |
| `17_ROADMAP.md` | Dependency-ordered implementation roadmap |
| `18_SOURCE_MAP.md` | Repository and external source map |
| `19_ADDED_MODULE_REVIEW.md` | Provenance/runtime-status review of modules added after the initial commit |
| `knowledge_manifest.yaml` | Machine-readable routing, status, ownership, invariants, defect IDs |

## Truth precedence

When sources disagree, use this order:

1. **Explicit implementation packet**, if it is based on the latest repository SHA.
2. **Canonical invariants and target architecture** in this layer.
3. **Current official protocol/provider documentation**, after revalidation.
4. **Current repository implementation**.
5. Comments, old commit messages, backups, and historical experiments.

Repository behavior ranks below canonical architecture because the baseline contains known broken and dormant paths.

## Confidence vocabulary

- **Certain:** directly verified in repository code or current official documentation.
- **Likely:** strongly supported but requires runtime evidence or post-merge revalidation.
- **Hypothesis:** candidate trading/model relationship that must be tested statistically.

No hypothesis may be converted into live capital behavior merely by documenting it here.
