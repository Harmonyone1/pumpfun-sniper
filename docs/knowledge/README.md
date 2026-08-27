# Pumpfun Sniper Knowledge Layer

This directory is the architectural memory for `Harmonyone1/pumpfun-sniper`.

It was created against commit `2158c64426de0f0c532221c578348c1dad1e075d` on 2026-08-27 after:
- repository-wide architecture review;
- runtime-path inspection;
- execution and position-state review;
- filter/strategy/model review;
- current Pump.fun, PumpPortal, Jito and Helius documentation review.

Start at:
- repository root `AGENTS.md` for agent routing;
- `00_INDEX.md` for human navigation.

## Critical usage rule

This layer deliberately separates:
1. **baseline reality** — what the code currently does;
2. **canonical architecture** — what the system is supposed to become;
3. **hypotheses** — trading/model ideas that require empirical validation.

Do not promote a hypothesis to live trading behavior just because it appears in this directory.

## Updating this layer

After each implementation merge:
1. pin the new repository SHA;
2. review diff;
3. update defect statuses;
4. update decision log if architecture changed;
5. revalidate time-sensitive provider/protocol facts when relevant;
6. issue future agent packets only after line numbers are recalculated against the new SHA.
