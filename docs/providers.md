# Providers

gait talks to model providers through a small registry in `crates/gait-provider/src/providers.rs`. Each provider is a row in a `PROVIDERS` table — wire protocol, base URL, env-var name, whether a real key is required. The vendored models.dev snapshot is metadata-only and must never set routing or auth.

## Model strings

Models are referenced everywhere as `provider/model-id`:

- `anthropic/claude-sonnet-4-6`
- `openai/gpt-4o`
- `minimax/MiniMax-M3`
- `ollama/llama3`
- `lmstudio/qwen2.5-coder`

gait validates the format on every turn — `model` must contain `/`; otherwise the turn refuses to start.

## The registry

This is the actual list shipped in the binary. Adding a new OpenAI-compatible provider is one row in `PROVIDERS`.

| Provider | Wire | Base URL | Env var | Requires key? |
|---|---|---|---|---|
| `anthropic` | Anthropic | (default) | `ANTHROPIC_API_KEY` | yes |
| `minimax` | Anthropic | `https://api.minimax.io/anthropic/v1` | `MINIMAX_API_KEY` | yes |
| `openai` | OpenAI | (default) | `OPENAI_API_KEY` | yes |
| `openai-codex` | Codex Responses | `https://chatgpt.com/backend-api/codex` | `OPENAI_CODEX_API_KEY` | yes |
| `groq` | OpenAI | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` | yes |
| `mistral` | OpenAI | `https://api.mistral.ai/v1` | `MISTRAL_API_KEY` | yes |
| `openrouter` | OpenAI | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | yes |
| `deepinfra` | OpenAI | `https://api.deepinfra.com/v1/openai` | `DEEPINFRA_API_KEY` | yes |
| `xai` | OpenAI | `https://api.x.ai/v1` | `XAI_API_KEY` | yes |
| `cerebras` | OpenAI | `https://api.cerebras.ai/v1` | `CEREBRAS_API_KEY` | yes |
| `togetherai` | OpenAI | `https://api.together.xyz/v1` | `TOGETHER_API_KEY` | yes |
| `fireworks` | OpenAI | `https://api.fireworks.ai/inference/v1` | `FIREWORKS_API_KEY` | yes |
| `perplexity` | OpenAI | `https://api.perplexity.ai` | `PERPLEXITY_API_KEY` | yes |
| `ollama` | OpenAI | `http://localhost:11434/v1` | `OLLAMA_API_KEY` | **no** |
| `lmstudio` | OpenAI | `http://localhost:1234/v1` | `LMSTUDIO_API_KEY` | **no** |

`openai-codex` is a distinct namespace from `openai`: it talks to the ChatGPT subscription backend, not the public API-key endpoint, so users with a ChatGPT Plus/Pro subscription can use it without an OpenAI API key. The CLI form is `gait auth login openai-codex` (device code) rather than `gait auth add openai-codex`.

`ollama` and `lmstudio` are local servers that don't check the `Authorization` header. `gait auth add` is not required for them — set the URL below if you're not on the default port.

## Listing available models

```bash
gait models                 # all providers with configured credentials
gait models anthropic       # filter by substring on provider or model id
```

`gait models` is an idempotent network read (provider's own `/v1/models` endpoint). It does *not* mutate any state.

## Local models (Ollama / LM Studio)

```bash
# 1. Start the local server
ollama serve                # default: http://localhost:11434/v1
ollama pull llama3          # or any other model

# 2. Point gait at it
gait --model ollama/llama3 "explain this repo"
```

Or set it as the default in `gait.jsonc`:

```jsonc
{ "model": "ollama/llama3" }
```

Remote box, Docker, or custom port? Override with `{PROVIDER}_BASE_URL`:

```bash
export OLLAMA_BASE_URL=http://192.168.1.50:11500/v1
export LMSTUDIO_BASE_URL=http://my-gpu-box:1234/v1
```

The env var is read at provider-construction time, so changes take effect on the next turn.

## Named inference profiles

A *named inference* is a saved `(provider, model, llm_params)` triple, stored in `gait.jsonc` under the `inferences` key. Selecting one for a session injects an `Inference` event into the log, so the choice is replay-complete (a resumed session re-derives the same model and params from the log, not from ambient state).

Use them when:

- You want one gait workspace to drive multiple model configurations without rewriting `gait.jsonc` between turns.
- You want to bind a model to a specific session for its lifetime.
- You're driving gait from another program and want a stable handle to a configuration.

```bash
gait inference list
gait inference show deep
gait inference select deep --session s1a2b3c4 --gait-dir .
gait inference clear --session s1a2b3c4 --gait-dir .
```

The REPL equivalent is `/inference`. Selecting a named profile is a `gait.jsonc` mutation, so the workspace's default still points at whatever `model` is set to.

## Auth resolution

For each turn, the provider adapter asks `gait-core::auth::resolve_api_key(provider)` for a key. The order:

1. The OS credential store, keyed by provider id (what `gait auth add` writes).
2. The provider's `env_var` from the environment (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.).
3. For local providers (`ollama`, `lmstudio`): no key, proceed without one.

If the provider requires a key and none is found, the turn refuses to start with `[gait] no API key for 'anthropic'; run gait auth add anthropic`.

## Switching models mid-session

Inside a session, the model is fixed at the start of each turn — the first LLM call already carries the resolved model and any `llm_params` overrides. To change the model mid-session:

- REPL: use `/model` (which calls the same `gait_ops::config::set_model` op the CLI uses), or `/inference select <name>` to switch to a named profile.
- CLI: use `gait config set model <model>` to change the workspace default, or `--model` on the bare-task form to override this turn only.
