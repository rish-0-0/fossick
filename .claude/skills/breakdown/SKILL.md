---
name: breakdown
description: Decompose a large feature or ROADMAP phase item into a written plan of week-sized, independently shippable milestones (docs/plans/<item>.md). Use whenever a task spans more than one module or more than ~1 week — BEFORE writing any code.
---

# breakdown — from big ask to buildable plan

Big features fail when built breadth-first. This skill turns "build
timechart" or "build the app platform" into a sequence of modules you can
finish one at a time to the AGENTS.md quality bar.

## Output

A file `docs/plans/<kebab-case-item>.md` (create `docs/plans/` if missing),
committed before implementation starts, structured exactly like this:

```markdown
# Plan: <item>            (ROADMAP: Phase N — <item name>)

## Goal
One paragraph: the user-visible outcome and how we'll know it works
(the exit criteria from ROADMAP.md, made concrete).

## Non-goals
What this deliberately does NOT include (and which phase it's deferred to).
This list is what keeps the work week-sized. Be aggressive.

## Current state
What exists today that this builds on — name the files/modules you read
to establish this. (If you haven't read them, you aren't ready to plan.)

## Design decisions
The 2–5 choices that shape everything (on-disk format? API shape? where
does the module boundary sit?). For each: the options considered, the
pick, and one sentence of why. Flag any decision you want the maintainer
to confirm before you start.

## Milestones
Each milestone: ≤ 1 week, ends with main green and something testably
true that wasn't before. Format:

### M1: <name> (~N days)
- Deliverable: <what exists after>
- Touches: <files/modules>
- Verify: <the specific test(s) that prove it — written first>
- Risk: <what could invalidate this milestone, if anything>

## Compatibility & invariants
Which AGENTS.md invariants this touches (format versions? pushdown
equivalence? crash safety?) and the specific test for each.

## Open questions
Things only the maintainer can answer. If this list is non-empty, ask
before starting M1.
```

## How to decompose well

- **Slice vertically, not horizontally.** Each milestone should produce a
  thin working slice (parse → execute → test for ONE case), not a layer
  ("all the parsing"). A milestone whose verify step is "code compiles" is
  a horizontal slice — redo it.
- **First milestone = walking skeleton.** The smallest end-to-end version
  of the feature, however crude, with the integration test in place. Every
  later milestone fattens it.
- **Order by risk.** Put the milestone that could invalidate the design
  (the new file format, the mmap trick, the API shape) first or second,
  never last.
- **Make verify steps executable.** "Verify: `cargo test -p fossick-spl
  timechart_span_counts`" — a named test, a curl command, a loggen+query
  recipe. If you can't write the verify step, you don't understand the
  milestone yet.
- **Checkpoint the plan.** After each milestone, re-read the plan: reality
  diverged? Update the plan doc in the same commit. A stale plan is worse
  than none.
- **Six+ milestones?** The item is really two ROADMAP items. Split it in
  ROADMAP.md first.

## After the plan

Get it confirmed by the maintainer (especially flagged decisions and open
questions), then execute milestone by milestone with the `depth-first`
skill. Check milestones off in the plan doc as you land them.
