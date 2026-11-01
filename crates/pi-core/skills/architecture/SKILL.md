---
name: architecture
description: Design or change the structure of code before you build it. Use when you add a feature that does not fit the current structure, define or change data types, split or merge modules, parse external input, share mutable state, persist data, or make an operation that can be retried.
---

# Architecture

Design the structure first.
Then build it in steps that you can verify.
When the user asks only for a design, stop after the plan and report it.

## Process

1. State the problem, the requirement, and the checks that prove the outcome.
2. Map the current structure.
   Record the modules, the data that each module owns, the direction of each dependency, and each place that writes shared state.
3. Find where the requirement does not fit.
   Special cases, flag parameters, and copied branches are signs of a misfit.
4. Design the data shape before the logic.
   Ask what you would build if the requirement had existed from the start.
5. Compare two or three options.
   For each option, state which errors it makes impossible, which later changes it makes expensive, and how much it costs to undo.
   Choose one and give the reason.
6. Plan the change as a sequence of steps.
   Put first the steps that make the later steps easier.
   After each step, the code builds and its tests pass.
   A step that breaks the code for a time is acceptable only when you planned it and can undo it.
7. Build the steps in order.
   Check each step before you start the next.

Report the chosen design, the options that you rejected and why, and the steps.

## Restructuring

- Before you move or reshape code, pin its current behavior with tests.
- Change the structure in a separate step from a change in behavior.
- After the change, run the same tests to prove that the behavior did not change.
- When a change repeats across many places, make one change by hand.
  Then write a script or use a refactoring tool that makes the same change, and compare its output with your manual change.

## Principles

### Model the domain

- Make illegal states impossible to represent in the types.
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
- Delete code that nothing uses.

### Reversibility

- Spend design effort in proportion to the cost of undoing the decision.
- Data formats, public interfaces, and persisted state are expensive to undo.
- Internal functions are cheap to undo.
