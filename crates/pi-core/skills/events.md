# fd 3 frames

A script talks to pi over file descriptor 3, one JSON object per line.
`PI_EVENTS_FD=3` carries the descriptor; `PI_SESSION_ID` names the captured
session; `PI_REQUEST_ID`, when set, names the model request that launched it.

## One rule: `id` decides the answer

| Line | Shape | Rule |
|---|---|---|
| Request | `{"id":7,"op":"say", …}` | answered exactly once |
| Notice | `{"op":"status", …}` | never answered |
| Reply | `{"id":7,"result":{…}}` / `{"id":7,"error":{"code":…,"message":…}}` | — |

Both directions use the same two shapes: pi sends requests and notices too.
The `id` is the sender's own name for the request, a number or a string; a
reply carries it back unchanged. Retrying a request with the same `id` and the
same payload does not do the work twice.

## Upstream: script → pi

| `op` | With `id` | Without `id` |
|---|---|---|
| `initialize` | required, handshake | — |
| `say` | waited for: the answer is how the turn ended | sent and let go |
| `ask` | waited for: the answer comes back | put up; the pick becomes the person's next line |
| `listen` | required, subscription | — |
| `status` | — | replaces the execution's progress line |
| `stop` | — | stops the session's running turn |

### `initialize`

```json
{"id":1,"op":"initialize","protocol":4}
{"id":1,"result":{"protocol":4}}
```

Send it first. A build that speaks another revision answers
`{"id":1,"error":{"code":"protocol","message":"…"}}`; stop there rather than
guess.

### `say`

```json
{"id":7,"op":"say","text":"/simplify","as":"person","note":"round 2 of a loop","more":true}
```

| Field | Required | Meaning |
|---|---|---|
| `text` | yes | The line. With `as:"person"` it goes through the host input parser. |
| `as` | no (`person`) | `person` — the person's words; `task` — a machine's. |
| `note` | no | Prose the model reads in the person's voice, filed ahead of the turn. |
| `more` | no | More turns follow: hold the checkout open between them. Dies with the script. |
| `re` | no | The message this answers (a model's `ask`, seen through a `message` notice). |
| `stop` | no | With `re`: close that question without answering it. |
| `source`, `reason` | for `task` | Where it came from and why now; both must be non-empty. |

With an `id`, the reply arrives when the turn ends:

```json
{"id":7,"result":{"turn":19,"state":"done","text":"…","message":"m-42"}}
{"id":7,"result":{"turn":19,"state":"failed","text":"…","message":null}}
```

`state` is `done`, `stopped` or `failed`; `text` is the answer's tail, `message`
the id of the message holding it. A `task` say is admitted at once and answers
`{"accepted":true}`; it is not owed a turn.

### `ask`

```json
{"id":8,"op":"ask","text":"Run this?","options":["yes","no"]}
{"id":8,"result":{"answer":"yes"}}
```

With an `id` the question goes up, the script waits, and the answer comes back
to it alone — it does not enter the conversation. A question that goes away
unanswered answers `{"id":8,"error":{"code":"closed"}}`. A background script
may ask this way; it blocks until the person answers.

Without an `id` the question is only put up: the person's pick becomes their
next line to the model, and the script never sees it.

### `listen`

```json
{"id":9,"op":"listen","to":"session","cursor":null}
{"id":9,"result":{"cursor":{"session":"…","next":12}}}
```

`session` (default): from then on, every message arrives as a `message` notice.
A null cursor starts at the oldest unfinished model input, or at the current
end when idle. Keep the cursor to resume; one connection listens to one
session, and listening again replaces the subscription.

```json
{"id":10,"op":"listen","to":"questions"}
{"id":10,"result":{}}
```

`questions`: pi will put every question raised on this checkout to the script
as an `ask` request, so it can relay it somewhere else and answer for the
person. Questions still up when the subscription starts are replayed.

## Downstream: pi → script

| Line | Shape |
|---|---|
| Session message (notice) | `{"op":"message","cursor":{…},"message":{…}}` |
| A question for you (request) | `{"id":3,"op":"ask","text":…,"options":[…]}` |

`message.kind` is one of `input` (a line for the model), `ask` (the model is
asking, with `options`) and `answer` (its `request` names what it answers: an
`input` means the model's answer, an `ask` means a person's).

An `ask` request is answered like any other:

```json
{"id":3,"result":{"picks":[1]}}
{"id":3,"result":{"text":"another directory"}}
{"id":3,"error":{"code":"declined","message":"no one is at the phone"}}
```

The first usable answer wins; later ones get `{"code":"taken"}`.

## Errors

| code | Whose | What a script does |
|---|---|---|
| `frame` | the script's | stop: the line did not decode or validate |
| `limit` | the script's | stop: the line was too big or came too fast |
| `protocol` | the script's | stop: the revisions do not match |
| `session` | nobody's | exit: this session takes no more input |
| `denied` | policy | report it: the line will not run |
| `busy` | nobody's | back off and retry |
| `closed` | nobody's | that question or turn is gone |
| `internal` | pi's | report it: the action failed |

## Exit

A process that never wrote a frame keeps the plain-command rule: its output is
what the model reads, and a clean exit says nothing new.

A process that spoke owns its words: `stdout` is a log nobody reads, the clean
exit only updates the running row, and a non-zero exit sends the tail of
`stderr` to the session as one `task` input. A conclusion belongs in a `say`.

## Who may run what

Host parsing is a property of the speaker, not a switch on the frame.

| Speaker | Plain text | `/command` | `!shell` |
|---|---|---|---|
| The local keyboard | yes | yes | yes |
| A script, `as:"person"` | yes | yes | **no** — answered `{"code":"denied"}` |
| A script, `as:"task"` | yes, literal | literal | literal |

## Lifecycle

- `say` opens a turn; with an `id`, its reply is how that turn ended.
- `more:true` holds the checkout open between turns: its tab reads working and
  the lane waits for this script's next line. It dies with the process.
- Nothing is retried across a host restart: resend with the same `id` and resume
  `listen` from the saved cursor.
