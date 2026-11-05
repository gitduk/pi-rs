# What reaches the model

An image pasted with `ctrl+v` accompanies the message.
The message text includes its path.
`read` also opens PNG, JPEG, GIF, and WebP files as images.
The model receives images only with `vision = true` in its
`[models."provider/model-id"]` entry.
Otherwise, pi replaces each image with a line that explains the omission.

`fetch` supports HTTP and HTTPS.
It refuses loopback, private, and link-local addresses.
With [rtk](https://github.com/rtk-ai/rtk) on `PATH`, `bash` passes each command
through `rtk rewrite`. `RTK_DISABLED=1` disables this step.

Repeated command output reaches the model once.
pi replaces six or more byte-identical repeated lines with one line that
identifies the first copy.
For example, this applies to the same diff printed for each failing test.

When the transcript exceeds the context window, pi removes repeated and old tool
results first. It summarizes whole rounds last, reducing the context to half the
available room. This leaves room for many turns before the next compaction.
The saved session keeps everything. Only the model's view shrinks.
Long user inputs keep their beginning and end in the history sent to the
summarizer. The summary records skills that still apply and requirements
specific to the task.
The model may need to load a skill again when its instructions no longer remain
in context.
