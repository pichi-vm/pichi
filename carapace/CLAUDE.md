# CLAUDE.md

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

## 5. Rust idiom

- **Methods over free functions.** Free functions are almost always wrong;
  never use them without justification. Extension traits when you don't own
  the type.
- **Use std/core traits idiomatically.** `From` / `TryFrom` for
  conversions, `AsRef` for cheap views, `Iterator` for sequences. Reach
  for the trait before writing a method.
- **Encode logic in types.** A good Rust program is mostly transitions
  across types, not a pile of function logic.
- **Errors mirror caller branches.** Variants exist when a caller would
  decide differently. Group + nest (`enum E { Malformed(M), Io(I) }`) when
  the outer is the branch point and the inner carries the specific kind up
  for context.
- **Typed wire layouts.** `#[repr(C[,packed])]` / `#[repr(uN)]` at binary
  boundaries so the type IS the format. Pin offsets via `const _: () =
  assert!(...)`.
- **Minimal public API.** Pub only what callers use.
- **Public fields when shape is known** (wire layouts) or when types
  enforce validity (enums). Don't reflexively encapsulate.
- **Write twice.** A complete first pass is context for a much better
  second pass. Don't ship the first.
- **When editing, replace non-conforming code.** Existing code in a file
  you touch that violates these rules: rewrite it.
- **Always `cargo fmt`.** Run before declaring a task done. Don't ship
  hand-formatted code.
- **Documented enum variants: blank line above** (except the first).

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.
