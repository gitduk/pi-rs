Summarize a coding session's history for compaction.
Everything below already happened. This history will be removed from the agent's context.
Your summary is the only record that remains in that context.

Write for the agent that will continue the work.

Output exactly these sections, in this order, headings included:

## Objective

State what the user wants to accomplish in one or two sentences.

## Constraints and decisions

Record decisions and their reasons so the agent does not reconsider them without cause.
Include the user's preferences, prohibitions, and corrections.
Include codebase facts that required investigation, such as locations, connections between components, and test assertions.

## Done

List finished work and verified results.

## In progress

State the work in progress and its current state.

## Blocked and ruled out

List failed approaches so the agent does not repeat them.
Include failing commands, unresolved questions, and work waiting on the user.

## Next

State the next action. Include the following action if known.

## Files

List each file touched by path and describe its changes.

Keep every section, including empty sections.
For an empty section, write only `(none)` on its own line.
Do not merge a section into another section's prose.
This structure preserves blockers through later compactions and prevents repeated failing commands.

Drop:

- Verbatim tool output. State the result it established.
- Narration of steps. Record outcomes instead.
- Content the agent can read again in one call. Name the file instead.

Keep names and numbers that make a finding specific.
For example, write `mistral_id pads to 9 chars` instead of `fixed the id helper`.
Use short bullets instead of prose paragraphs. Keep the entire summary under 600 words. Do not add a preamble.

If the history starts with an earlier summary, that summary covers events before the remaining history.
The earlier summary will also be discarded. Produce one combined summary of both sources.
Preserve objectives, constraints, and unfinished work even when the newer history does not repeat them.
Remove work from the unfinished list only when the newer history shows that it finished.

When the sources disagree, keep the newer fact and remove the old claim.
Move completed work from "In progress" to "Done".
