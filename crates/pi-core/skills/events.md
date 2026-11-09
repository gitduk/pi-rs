# fd 3 frames

Scripts exchange one JSON object per line through fd 3. `PI_EVENTS_FD=3`
indicates the connection. `PI_SESSION_ID` identifies the captured session;
`PI_REQUEST_ID`, when set, identifies the model request that launched the tool.
A restart retains the original ownership. Never route to a newly active session.
Only the frames below are accepted. There is no legacy decoder.

## Model messages

A background script submits the same message structure used inside the agent:

```json
{"type":"message","message":{"id":"unique-input-id","session":"session-id","by":null,"at":1790000000000,"kind":{"type":"request","value":{"direction":"to_model","request":"user"}},"body":[{"type":"text","value":"Hello"}]}}
```

Use `user` only for a person's input, regardless of its source. Text is literal;
it does not execute host commands. For task or timer input, use `automatic` and
include a data block `{"type":"data","value":{"source":"timer-id","reason":"timer elapsed","event":"tick-id"}}`.
`source` identifies the producer, `reason` explains the trigger, and `event`
identifies this occurrence.
Assign a unique message ID before sending. A retry must retain the ID and payload.
`at` is Unix time in milliseconds. `by` is an optional causal message ID.

Acceptance returns `{"type":"accepted","id":"...","request":"...","cursor":{"session":"...","next":0},"response":null}`.
A completed retry may include its full stored `response`. Rejection returns
`{"type":"error","id":"...","request":"...","message":"..."}`.
Acceptance does not mean that execution has started or finished.

Subscribe with `{"type":"observe","session":"...","cursor":null}`.
The connection first receives `{"type":"observing","cursor":{...}}`, then
`{"type":"message","cursor":{...},"message":{...}}` for each complete message.
With no cursor, observation starts at the oldest unfinished model input, or at
the current end when idle. Use the acceptance cursor to include a submitted
input even if it completed before subscription. Save each received cursor to
resume later. A cursor describes the next position. One connection observes one
session; a new subscription replaces the previous one.

Messages retain their original ID, body, request association, and status.
A final model response contains the **complete** answer, including text produced
before tool calls. It is not a remainder after progress. Render each message once
by ID. Do not echo tool results as model answers. Observe the original input
request to associate its final response; do not treat every response as a root answer.

A model request to collect user input has this kind:

```json
{"type":"request","value":{"direction":"from_model","request":{"type":"ask","options":["Allow","Deny"]}}}
```

Show its body and options while the parent input is running. Submit a response
whose kind is `{"type":"response","value":{"request":"original-ask-id","status":{"type":"done"}}}`.
Use the same message envelope with a new ID, the original session, and text or
image body. Selected options are their original text, not numeric indexes.
Free text is also valid. Closing without an answer uses `{"type":"stopped"}`
and an empty body. The first valid response wins. Observe the accepted response
to close other displays. Model and tool responses cannot be supplied by a channel.

## Runtime operations

Runtime management is separate from model messages:

| Frame | Meaning |
|---|---|
| `{"type":"runtime","command":{"type":"status","text":"..."}}` | Replace execution progress. Also supported in foreground mode. |
| `{"type":"runtime","command":{"type":"serve"}}` | Declare a channel or producer service. Send immediately on startup. Exit updates execution status without creating another model input. |
| `{"type":"runtime","command":{"type":"interrupt"}}` | Stop the captured session's active run. |
| `{"type":"runtime","command":{"type":"command","line":"..."}}` | Explicitly invoke the host input parser. Use only for a deliberate host operation. |
| `{"type":"runtime","command":{"type":"watch"}}` | Observe internal runtime prompts, separately from model requests. |
| `{"type":"runtime","command":{"type":"prompt","text":"...","options":["allow","deny"],"back":true}}` | Collect internal input for a foreground tool or hook. Waits without consuming its execution timeout. |
| `{"type":"runtime","command":{"type":"answer","question":17,"picks":[0]}}` | Answer an internal runtime prompt by option index. |
| `{"type":"runtime","command":{"type":"answer_text","question":17,"text":"..."}}` | Answer an internal runtime prompt with text. |
| `{"type":"runtime","command":{"type":"drive","enabled":true}}` | Claim continuing work for this execution; release with `false`. |

Foreground scripts accept only `status` and `prompt`. A background script can
publish a nonwaiting internal prompt with `back:false`; it cannot wait on a
foreground prompt. Internal prompts are not model-owned `Ask` requests.

Runtime events use `{"type":"runtime","event":{...}}`:

- `{"type":"prompted"}` confirms publication of a nonwaiting internal prompt.
- `{"type":"answer","text":"allow"}` returns a waiting tool's answer;
  `text:null` means it was closed. Failure uses `{"type":"error","message":"..."}`.
- Watchers receive `{"type":"prompt","question":17,"text":"...","options":[...],"back":true}`,
  then `{"type":"closed","question":17}`. A rejected answer produces
  `{"type":"refused","question":17,"message":"..."}`.
- Host operation events carry `type:"operation"`, numeric `operation`, and a
  `phase` with `type:"queued"`, `"started"`, `"progress"`, or `"finished"`.
  A finished phase carries text, typed status, and optional numeric `exit_code`.
  A `linked` event associates a host operation with its semantic input request.
  Completion marked `model:true` carries no answer; use the semantic response.

Ordinary background commands report stdout at exit as one automatic input;
a failing exit also includes stderr. A service that submits its own messages
must declare `serve` to avoid an additional exit input. Startup, reconnect,
observation, and status updates never open a model turn.

Frames are bounded in size and count. A slow receiver is disconnected; accepted
requests remain owned by the session. Reconnect with a saved cursor or retry the
original message. This does not promise automatic retries across host restarts.
