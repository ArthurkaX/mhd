# Tier Downgrade — Concept

A design for deciding, per request, whether an expensive-tier (Opus/Sonnet)
call can be safely routed to a cheaper model — without hurting the quality of
real work.

## The problem

The proxy tries to guess from a **single HTTP request** whether it needs the
expensive model. But a request is not a task. In Claude Code a *task* is the
whole **session**: dozens of round-trips of `think → tool_use → tool_result →
think`. Within one task the per-turn difficulty swings wildly — one turn is a
trivial `ls`, the next is "synthesize an architecture from five files".

This conflates two different questions:

- **"Is this a serious task?"** — a property of the *session*. Decided once.
- **"Does *this turn* need a brain?"** — a property of the *turn*. Changes every
  round-trip.

### Why the old heuristic fails

The previous rule was: *"last assistant message contained `tool_use` →
downgrade."* In agentic coding **every turn after the first contains
`tool_use`**, so the signal has ~zero discriminative power. In practice the rule
means "downgrade the entire session after turn 1."

Evidence from `proxy.db` — one Claude Code session, every turn dropped to Sonnet:

```
01:03:13 → 01:05:28   12 consecutive DOWNGRADE (Opus→Sonnet), all "tool_use continuation"
01:07:03              KEEP "real task"  ← a plain-text answer again
```

Two more findings from the raw logs:

- Claude Code always sends `thinking=adaptive`, never `"enabled"`. The old
  heuristic only matched `"enabled"`, so its strongest signal was **dead code**
  for Claude Code.
- "Previous turn had `tool_use`" tells you nothing about whether *this* turn is
  heavy, because in an agent loop it is always true.

## The reframe

The real question is:

> **Is a human waiting on this response, or is the agent talking to itself?**

- Human just typed → they care about quality → **keep on Opus**.
- Agent is spinning a tool loop → intermediate step, nobody is watching → **cheap
  is fine**.

The best proxy for "a human just typed" is a **pause**: they read the previous
answer, thought, and typed back. A tool loop comes back in 5–10 seconds.

## Evidence: time separates the two cleanly

Inter-request gaps for each decision in `proxy.db`:

| Preceding gap before…        | gap_s values                                  |
|------------------------------|-----------------------------------------------|
| `real task` (human typed)    | 43, 572, 95, 40, 53, 35, 353 — **always > 34s** |
| `tool_use continuation` loop | 6, 6, 6, 5, 10, 10, 7, 6, 6, 6, 5, 8, 14, 15 — **almost always < 17s** |

There is an **empty valley between ~17s and ~34s**. Time discriminates "human"
from "loop" better than any structural signal.

## The decision rule (structure × time)

|                          | gap < ~25s                          | gap > ~25s                                |
|--------------------------|--------------------------------------|-------------------------------------------|
| last assistant = `tool_use` | **DOWNGRADE** (loop, nobody waiting) | KEEP (human returned / heavy turn ahead)  |
| last assistant = text       | KEEP                                 | KEEP (human read it and replied)          |

Collapses to a single rule:

> **Downgrade only if (the previous assistant turn was `tool_use`) AND (the gap
> since the previous request was short).** Everything else stays on Opus.

## Why time does double duty

This is the source of robustness. The pause measures **not only human presence
but also turn weight**:

- fast `tool_result` (small `ls`, quick grep) → small gap → genuinely trivial
  turn → downgrade ✓
- slow `tool_result` (a build, an 800-line file, a compiler error) → large gap →
  heavy analysis ahead → keep on Opus ✓

Heavy turns give themselves away through time. One signal covers both axes.

It also fixes a concrete bug in the data: **id 84** — a `tool_use continuation`
with a 55s gap. The old logic downgraded it. The new rule **keeps** it,
correctly: 55s means either the human came back, or something heavy was
computed.

## Fit with manual control

The user already manually switches the proxy between native and cheap models
when they know a task is simple — they are the best classifier. So the auto
system should not decide *for* them; it should only close the turns they cannot
be bothered to manage: the mechanical round-trips inside a session.

The manual target is the **ceiling**. Auto-downgrade operates only *below* it and
only on confidently-mechanical turns:

- user set a cheap model → they already decided "simple" → auto does nothing.
- user left it on Opus → auto only nibbles fast loop turns, and **never touches a
  turn the user is personally waiting on** (those always have a large gap → keep).

Safe by construction: worst case it slightly degrades one intermediate
mechanical step, which is corrected on the next human turn anyway.

## Serving all three goals

- **Cost** — tool loops run on the cheap model.
- **Speed** — mechanical turns answer on the faster model.
- **Rate limits** — the time threshold can react to Opus 429s (see below).

## Extensions along the time axis

1. **Warm-start cooldown.** Right after a human turn (kept on Opus), give the
   *next* turn a grace period on Opus too — the agent's first step after an
   instruction usually sets the plan (heavy). Let it drift cheap as the loop
   tightens. "Warm start, cool down."
2. **Rate-limit-aware threshold (goal #3).** Keep a windowed count of Opus 429s.
   When near the limit, *raise* the pause threshold (downgrade more
   aggressively). When the limit is free, keep the threshold low and protect
   quality. A time-window response instead of preventive downgrading.
3. **Adaptive threshold.** The ~25s constant comes from this one database.
   Instead of hardcoding it, track a rolling median of the user's pauses so the
   system adapts to their typing cadence.

## State required

One new piece of state: **the timestamp of the previous request in this
session**. Session key = a hash of the first user message (+ system prefix),
which is stable within a conversation. Map `session_hash → last_ts` with TTL
cleanup. Cheap, no external dependencies.

## Explicitly not doing

- Guessing from `/compact`, `/retry`, or user-text length — brittle proxy signals
  that break whenever Claude Code updates.
- Trusting the `thinking` flag as before — Claude Code always sends `adaptive`,
  so either fix the check (treat `adaptive` as keep) or drop the branch so it
  does not create a false sense of a working signal.
