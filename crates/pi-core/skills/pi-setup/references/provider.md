# A model provider

Read this skill's own `settings.toml` for every field; this page is the steps.

## Steps

1. Read the host's own catalog before writing anything: `curl -s <base_url>/models`.
   What it returns decides the paths; the vendor's prose about its API does not.
2. Choose `format` by the API the host answers:
   `anthropic` is the Messages API (`/v1/messages`),
   `openai` the Responses API (`/responses`),
   `chat` Chat Completions (`/chat/completions`).
   A host advertised as "OpenAI-compatible" usually speaks `chat`, not `openai`.
3. Write `[provider.<name>]` with `base_url`, `format` and `api_key`.
   pi discovers the models from `<base_url>/models`, or `<base_url>/v1/models` for `anthropic`.
4. Map only the facts that differ from the defaults.
   The defaults are `data` for the array, `id`, `context_window` and `max_output_tokens` on each item.
5. Select the model as `<name>/<id>` — the provider prefix is pi's, and only the id reaches the host.

## A measured example

OpenRouter, read 2026-11-05 from `https://openrouter.ai/api/v1/models`:
a `data` array, `context_length`, the completion limit on the routed provider,
input types under `architecture`, and every rate quoted per token.

```toml
[provider.openrouter]
base_url          = "https://openrouter.ai/api/v1"
format            = "chat"
api_key           = "$OPENROUTER_API_KEY"
context_window    = "context_length"
max_output_tokens = "top_provider.max_completion_tokens"
vision            = "architecture.input_modalities"

[provider.openrouter.pricing]
unit                = "token"
input_per_mtok      = "pricing.prompt"
output_per_mtok     = "pricing.completion"
cache_read_per_mtok = "pricing.input_cache_read"
```

A model id that contains `/` keeps it:
`pi -m openrouter/deepseek/deepseek-v4.1-flash`.

## When it does not work

- The model does not appear: the catalog path or the id path is wrong. Print the catalog and compare with step 1.
- Every request is refused: `format` names an API the host does not serve. Try `chat`.
- The window is smaller than the model's: override `context_window` under `[models."<name>/<id>"]`, which also carries `vision`, `effort` and `pricing` overrides.
- The cost reads zero: the catalog quotes rates under other paths, or `unit = "token"` is missing.
- A capability or price fact is absent: a path that selects nothing leaves that fact unset rather than failing the load.

## When a list is read

pi reads each provider's catalog at a start, when `settings.toml` changes, and
when `/model` is used. `/model` applies the whole list, since it is being
chosen from; a list read for any other reason is applied when its model names
change or when a fact moves for a model a lane runs. A price that moved on a
model nothing runs waits for the next `/model`.
