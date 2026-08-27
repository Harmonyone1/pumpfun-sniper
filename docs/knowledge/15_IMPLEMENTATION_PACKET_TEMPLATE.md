# Implementation Packet Template

This is the required format for coding-agent handoffs.

```text
IMPLEMENTATION TASK: <ID>
TITLE: <short exact title>

REPOSITORY
Harmonyone1/pumpfun-sniper

BASE BRANCH
<branch>

BASE COMMIT
<exact full SHA>

OBJECTIVE
<one closed-scope behavioral objective>

WHY THIS CHANGE EXISTS
<brief architecture reason; no exploration task>

PRECONDITIONS
- <previous task/commit that must already exist>
- <external API version/fact if relevant>

FILES PERMITTED TO CHANGE
1. <path>
2. <path>

FILES EXPLICITLY FORBIDDEN
- <path/subsystem>

GLOBAL DO-NOT-CHANGE
- thresholds unless listed
- position sizing unless listed
- wallet selection unless listed
- config names unless listed
- unrelated formatting/refactors
...

CANONICAL INVARIANTS APPLICABLE
- INV-...
- INV-...

CHANGE 1
File: <exact path>
Current lines: <exact start-end at BASE COMMIT>
Anchor: <function/type and 1-3 exact current-code lines>

Current behavior:
<exact behavior>

Required replacement:
<unambiguous behavior>

Exact data/type semantics:
<input and output units/types>

Required call-site changes:
- <path:function>
- <path:function>

Forbidden side effects:
- <explicitly list>

CHANGE 2
...

STATE TRANSITIONS
Before:
<state diagram>

After:
<state diagram>

ERROR HANDLING
- <exact error/failure cases>
- <what is persisted/retried>
- <what must never happen>

LOGGING/TELEMETRY
Add/modify:
- <event name and fields>
Do not log:
- secrets
- private keys

CONFIGURATION
- Added:
- Removed:
- Renamed:
- Default behavior:
If none: “No config changes.”

TESTS TO ADD
TEST-1:
File:
Scenario:
Input:
Expected:

TEST-2:
...

EXISTING TESTS THAT MUST REMAIN GREEN
- <tests/modules>

COMMANDS TO RUN
1. cargo fmt --check
2. cargo check
3. cargo test <specific module>
4. cargo test
5. <other>

ACCEPTANCE CRITERIA
[ ] ...
[ ] ...
[ ] No modifications outside permitted files.
[ ] No TODO used to defer required behavior.
[ ] No secret printed or committed.

REQUIRED COMPLETION REPORT
- base SHA used
- resulting commit SHA
- files changed
- acceptance checklist
- commands + pass/fail
- deviations
- newly discovered contradiction
```

## Line-number rule

The architecture/review layer must calculate line numbers against the exact `BASE COMMIT` immediately before issuing the packet.

Agents must not “find approximately where this lives” when the packet can specify it.

## Rework-prevention rule

If a required dependency or ambiguity is discovered during packet construction, resolve it **before** handing the packet to the coding agent.

The coding agent should not be asked to decide between architectures.
