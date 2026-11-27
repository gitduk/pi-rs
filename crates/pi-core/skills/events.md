# fd 3 frames

A script talks to pi over file descriptor 3, one JSON object per line.
`PI_EVENTS_FD=3` carries the descriptor; `PI_SESSION_ID` names the captured
session; `PI_REQUEST_ID`, when set, names the model request that launched it.

## One rule: `id` decides the answer

| Line | Shape | Rule |
|---|---|---|
| Request | `{"id":7,"op":"send", …}` | answered exactly once |
| Notice | `{"op":"status", …}` | never answered |
| Reply | `{"id":7,"result":{…}}` / `{"id":7,"error":{"code":…,"message":…}}` | — |

Both directions use the same two shapes: pi sends requests and notices too.
The `id` is the sender's own name for the request, a number or a string; a
reply carries it back unchanged. For `event` and `service` requests, retrying
with the same `id` (or `key`) and payload does not do the work twice.

## Upstream: script → pi

| `op` | With `id` | Without `id` |
|---|---|---|
| `initialize` | required, handshake + capabilities | — |
| `info` | required, session + workspace + `busy` | — |
| `send` | waited for (`fd 3`: how the turn ended; `service`: admitted) | sent and let go |
| `event` | answered `{"accepted":true,"message":…}` | sent and let go |
| `reply` | answered `{"accepted":true,"message":…}` | sent and let go |
| `ask` | waited for: the answer comes back | put up; the pick becomes the person's next line |
| `listen` | required, subscription | — |
| `status` | — | replaces the status text (`null` or `""` clears it) |
| `stop` | — | stops the session's running turn |

### `initialize`

```json
{"id":1,"op":"initialize","protocol":5,"client":"sensor"}
{
  "id": 1,
  "result": {
    "protocol": 5,
    "mode": "background",
    "session": "…",
    "workspace": "/path/to/repo",
    "capabilities": ["initialize", "info", "send", "event", "reply", "ask", "listen:session", "listen:questions", "status", "stop"]
  }
}
```

Send it first. `client` is optional; it identifies your tool in logs and
namespaces `service` message ids. A build that speaks another revision answers
`{"id":1,"error":{"code":"protocol","message":"…"}}`; stop there rather than
guess. Available in `foreground`, `background`, and `service` modes (`session`
may be `null` in `background` when no session is captured).

### `info`

```json
{"id":2,"op":"info"}
{"id":2,"result":{"session":"…","workspace":"/path/to/repo","busy":false}}
```

Read-only snapshot of the connected session: its id, workspace root, and
whether a turn is currently running or queued (`busy`). Available in all modes.

### `send`

```json
{"id":7,"op":"send","text":"/simplify","note":"round 2 of a loop","more":true}
```

| Field | Required | Meaning |
|---|---|---|
| `text` | yes | Non-empty user input. Over `fd 3` it goes through the host input parser; `!shell` is denied. |
| `note` | no | Prose the model reads in the person's voice, filed ahead of the turn (`fd 3` only). |
| `more` | no | More turns follow: hold the checkout open between them. Dies with the script (`fd 3` only). |

Over `fd 3`, with an `id`, the reply arrives when the turn ends:

```json
{"id":7,"result":{"turn":19,"state":"done","text":"…","message":"m-42"}}
{"id":7,"result":{"turn":19,"state":"failed","text":"…","message":null}}
```

`state` is `done`, `stopped` or `failed`; `text` is the answer's tail, `message`
the id of the message holding it. Over a `service` socket, `send` admits the
input and answers `{"accepted":true,"message":"…"}` right away.

### `event`

```json
{"id":8,"op":"event","text":"disk /tmp 96%","source":"sensor","reason":"disk threshold","key":"disk:/tmp:96"}
{"id":8,"result":{"accepted":true,"message":"…"}}
```

| Field | Required | Meaning |
|---|---|---|
| `text` | yes | Non-empty event text for the model as an automatic input. |
| `source` | yes | Non-empty producer identifier. |
| `reason` | yes | Non-empty short reason why this event fired now. |
| `key` | no | Idempotency key for `(source, key)` deduplication; defaults to `id` when omitted. |

Admitted immediately as an automatic input; literal text, never parsed as host
commands.

### `reply` (`fd 3` `background` only)

```json
{"id":9,"op":"reply","to":"m-42","text":"continue","cancel":false}
{"id":9,"result":{"accepted":true,"message":"…"}}
```

| Field | Required | Meaning |
|---|---|---|
| `to` | yes | The model `ask` message id being answered (seen via `listen`). |
| `text` | unless `cancel` | Non-empty answer text. |
| `cancel` | no | Close that question without answering it. |

### `ask`

```json
{"id":8,"op":"ask","text":"Run this?","options":["yes","no"]}
{"id":8,"result":{"answer":"yes"}}
```

With an `id` the question goes up, the script waits, and the answer comes back
to it alone — it does not enter the conversation. A question that goes away
unanswered answers `{"id":8,"error":{"code":"closed"}}`. A background or
foreground script may ask this way; it blocks until the person answers.

Without an `id` (`background` only) the question is only put up: the person's
pick becomes their next line to the model, and the script never sees it.

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

### `status` and `stop`

```json
{"op":"status","text":"3/10"}
{"op":"status","text":null}
{"op":"stop"}
```

`status` updates the progress/service line; `text: null` or `""` (or omitting
`text`) clears it. `stop` interrupts the session's running turn.

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

The first usable answer wins; later ones get `{"code":"closed"}`.

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
`stderr` to the session as one automatic input. A conclusion belongs in an
`event`.

## Who may run what

Host parsing is a property of the operation, not a switch on the frame.

| Op | Plain text | `/command` | `!shell` |
|---|---|---|---|
| The local keyboard | yes | yes | yes |
| `send` (`fd 3`) | yes | yes | **no** — answered `{"code":"denied"}` |
| `send` (`service`) | yes, literal | literal | **no** — answered `{"code":"denied"}` |
| `event` | yes, literal | literal | literal |

## Services

A tool declaring `service = true` runs as one process per machine, started by
pi itself rather than offered to the model. It holds the lock pi passes at
`PI_SERVICE_LOCK_FD` for its lifetime — exit when `PI_SESSIONS_DIR` holds no
session socket you can connect to — and connects to every socket there,
speaking this protocol on each. `initialize`, `info`, `send`, `event`, `status`
and `listen` to `session` work as above; what a `status` sets is drawn by the
statusline's `services` field, not held on any execution's row. `reply`, `ask`,
`listen` to `questions`, `more` and `note` are refused; `stop` is ignored.

## Lifecycle

- `send` opens a turn; over `fd 3` with an `id`, its reply is how that turn ended.
- `more:true` holds the checkout open between turns: its tab reads working and
  the lane waits for this script's next line. It dies with the process.
- Nothing is retried across a host restart: resend with the same `id` (from a
  `service` with `client`) or `(source, key)` (on `event`), and resume `listen`
  from the saved cursor.
