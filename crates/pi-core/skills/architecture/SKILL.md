---
name: architecture
description: Review, design, or change code structure, data types, module boundaries, or dependencies. Also covers external input, shared mutable state, persistence, and retryable operations.
---

# Architecture

To change code, design the structure first.
Then build it in steps that you can check.
When the user asks only for a design, stop after the plan and report it.
When the user asks for a review, follow Review and do not change files.
Match the depth of the analysis to the scope and the cost of reversing the change.

## Review

1. Map the current structure as in step 2 of Process.
2. Compare the structure with each section of Principles.
3. For each problem, report:
   - The principle that it breaks.
   - The evidence, with file paths and line numbers.
   - The effect on later changes.
   - A recommended fix.
   A type the design adds that only wraps a field or groups others is
   over-modelling: report it as one.
4. Order the problems by their effect, the largest first.
5. Report the parts that follow the principles in one short list.
   Do not report a principle that you could not check as a pass.

## Process

1. State the problem, the requirement, and the checks that prove the outcome.
2. Map the current structure.
   Record the modules, the data that each module owns, the direction of each dependency, and each place that writes shared state.
3. Find where the requirement does not fit.
   Special cases, flag parameters, and copied branches are signs of a misfit.
4. Design the data shape before the logic.
   Consider how the design would include this requirement from the start.
   Write the fewest types that cover it, and name the error each one makes
   impossible. A type that wraps one field, or only groups others, is that
   field; a flat list with a marker value usually beats nested wrappers.
   Display rules — row limits, fold thresholds, markers — are not data.
5. Compare two or three options when the choice has meaningful tradeoffs.
   For each option, state which errors it makes impossible, which later changes it makes expensive, and how much it costs to undo.
   Choose one and give the reason.
6. Plan the change as a sequence of steps.
   Put first the steps that make the later steps easier.
   After each step, the code builds and its tests pass.
   A step that breaks the code for a time is acceptable only when you planned it and can undo it.
7. Build the steps in order.
   Check each step before you start the next.

Report the chosen design and the steps.
If you compared options, explain why you rejected the alternatives.

## Restructuring

- Before you move or reshape code, identify the tests that cover its current behavior.
  Add tests when the existing checks leave meaningful behavior unprotected.
- Change the structure in a separate step from a change in behavior.
- After the change, run the same tests to prove that the behavior did not change.
- When a change repeats across many places, make one change by hand.
  Then write a script or use a refactoring tool that makes the same change, and compare its output with your manual change.

## Principles

### Model the domain

- Make illegal states impossible to represent in the types.
- Every type must make a class of error impossible to write.
  A wrapper around one field, or a type that only groups others, is deleted
  and its field used directly.
- Every variant needs a second real value: a variant with one value is a
  constant, and a field that is always set is not optional.
- Count the types against the concepts. Two shapes that both make the errors
  impossible: take the one with fewer types.
- Give each concept one name, and use that name everywhere.
- Do not bypass the type checker with casts, suppressions, or untyped values to make a design fit.
- Leave clear, local code as it is.

### Boundaries

- Parse external data once, where it enters.
- Inside the boundary, trust the parsed types.
- Keep business logic in pure functions.
- Keep effects such as I/O, time, and randomness at the edges.

### Dependencies

- Make dependencies point in one direction.
- A lower layer does not know about the layers above it.
  It does not name their types, commands, or entry points, even in comments or messages.
- When a lower layer needs knowledge from above, the upper layer supplies it through a parameter, an interface, or a callback.

### One decision in one place

- Each rule, constant, and format has one definition.
- Other code refers to that definition and does not copy it.

### State

- If two parts do not need the same mutable data, give each part its own copy.
- If they must share it, allow one writer, and make the structure enforce that.
- Make operations idempotent.
  Running an operation twice, or again after a crash partway through, gives the same result.

### Subtract before you add

- Remove complexity that the new design makes unnecessary before you add new parts.
- Remove code that nothing uses.
- Keep display rules where they are drawn: a row limit, a fold threshold or a
  marker is not part of a data shape, and a constant that only a renderer
  reads belongs to the renderer.

### Reversibility

- Spend design effort in proportion to the cost of undoing the decision.
- Data formats, public interfaces, and persisted state are expensive to undo.
- Internal functions are cheap to undo.
