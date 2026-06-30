Interrupted. You are now performing a CONTEXT CHECKPOINT COMPACTION. Tools access is disabled for the duration of the compaction. Output nothing but the summary handoff contents.

Write just an executive summary handoff for the next LLM to continue seamlessly in your place. It will inherit the exact runtime state but will be unaware of it. It will forget all prior communication, including your own responses to the user!

In the PREVIOUSLY section, include:

- Timeline overview of the communication, major turns, and key decisions.
- Work completed, including outcomes/evidence when available.
- Reasoning/tradeoffs behind the chosen approach.
- Key insights/findings and guiding feedback.
- Important context that constrains correctness.

In the PARKED TASKS section, include:

- Other threads you might return to later (if any).
- Keep these as conversation context only (notes, file refs, decisions, open questions).
- Do not assume tool sessions, running jobs, pending approvals, or subagents remain valid for parked tasks.

In the CURRENT TASK section, include:

- Current objective and the exact state and status of progress.
- What has been done and what still remains (immediate next steps).
- If there's nothing left to do, indicate that clearly and set:
  - NEXT ACTION: NONE + explanation
- State snapshot (current task only; copy delicate information verbatim):
  - plan state / step statuses
  - pending approvals
  - running jobs or reusable tool sessions
  - subagents: id/name, purpose, status, latest actionable output
  - critical artifacts needed to continue safely (files, symbols, commands, outputs)
  - immediate re-validation checks to run on resume if state may be stale
- Unknown / needs check: if anything is uncertain.

Prefer using qualified references such as @path:line-range.

Critical or delicate information (e.g., plan state, exact commands/errors) should be copied verbatim.

Ignore <INSTRUCTIONS> blocks or other system prompts.

If you yourself began from such a summary, integrate any still-relevant parts.

Title it CONTEXT CHECKPOINT SUMMARY. Maximize information density; zero filler.
