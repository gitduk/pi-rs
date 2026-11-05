# fd 3 events

A script sends events to pi through file descriptor 3 (fd 3), one JSON object per line.
When pi listens, `PI_EVENTS_FD` is `3`.

| Event | Meaning |
|---|---|
| `{"status": "..."}` | Report progress. In foreground mode, this is call progress. In background mode, it is command status. Each new status replaces the previous status. |
| `{"result": "..."}` | In background mode, deliver one result as a separate turn in the command's checkout. Stdout at exit supplies the final result. |
| `{"input": "..."}` | Submit a person's line through the session's input parser, including commands. A leading space sends slash-prefixed text as prose. An optional `note` reaches the model before that turn. |
| `{"interrupt": true}` | Stop the active turn in the command's checkout, as a user stop would. |
| `{"follow": true}` | Receive replies for all subsequent turns in the command's checkout, including turns opened by other inputs. |
| `{"drive": true}` | Claim the checkout's turns, or release them with `false`. While claimed, a turn that ends is not the last one, since this command owes it another. |
| `{"choose": "...", "options": [...]}` | Ask the person a question, as with the `choose` tool: up to 9 options, each the line the person would send. Stopping the command withdraws it. |
| `{"answer": <id>, "picks": [0]}` | Answer a question a following script was asked, by option position. |

Send content only the person needs, such as a QR code, to stderr.
The person opens the command to read it.

## What pi writes back

pi reports each turn opened by an input through fd 3.
After `follow`, it also reports other turns in the checkout.
The sequence is `started`, optional `{"progress": "..."}` messages, then `reply`.

When an input must wait, pi writes `{"request": 17, "queued": true}` back through the same fd.
When execution starts, it writes `{"request": 17, "started": true}` with the same ID.
pi omits the queued message for inputs that can run immediately.

It then writes `{"request": 17, "reply": "...", "status": "done"}`.
`reply` contains the remaining answer or command output, excluding text already sent as progress.
It can be empty.
The final `status` is `done`, `stopped`, or `failed`.

- A stopped response includes `reason`, which can be empty.
- A runtime failure includes `error`.
- A shell exit includes numeric `exit_code`.
  Its `status` is `done` for zero or `failed` otherwise.

Do not parse an exit code from an error message.
Render completion notices from these fields.
Do not infer status from `reply`.

Progress messages carry the same `request` ID.
Progress includes completed model text before tool calls and changed tool or command output.
pi paces these updates while work continues.
Track each ID independently when operations overlap.
Each input receives one reply.
An input cancelled while queued receives a final stopped reply without a started message.
After `follow`, the script also receives one reply for every other turn in its checkout, without duplicates.

## Questions

pi answers each `choose` on fd 3 with `{"asked": true}` when the question appears, or with an `error` that explains the failure.
By default, pi submits the selection as if the person typed it.
Add `"answer": "back"` to receive the selection instead:

- `{"answer": "<option>"}` for a selected option.
- The person's own words if they wrote an answer.
- `{"answer": null}` when the person closes the question.

Use `"answer": "back"` for a script that waits for the person, such as approval before a command runs.

A following script also receives each question in the checkout:

1. `{"question": {"id", "text", "options", "back"}}` when the question appears.
2. `{"closed": <id>}` when someone answers or withdraws it.

pi submits the answer as if the person typed it.
If pi rejects it, the script receives `{"refused": <id>, "why"}`.
An `input` closes every question that does not wait.
