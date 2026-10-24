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
| `openai-codex` | Codex Responses | `https://chatgpt.com/backend-api/codex` | subscription | yes |
| `openai-signin` | Responses (plan) | `https://api.openai.com/v1` | subscription | yes |
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

To reuse an existing file-backed Codex CLI login without copying its tokens into gait:

```bash
gait auth link openai-codex codex
gait --model openai-codex/<model> "your task"
```

The Codex source follows Codex's own home precedence: an explicit `--home`, then `CODEX_HOME`, then `~/.codex`. `gait auth unlink openai-codex` removes only gait's non-secret binding. Codex keyring-backed auth is not supported yet; configure Codex's `cli_auth_credentials_store` as `file` for this integration.

Gait's own credential map and external-source pointers live at
`$GAIT_HOME/auth.json`, where `GAIT_HOME` defaults to `~/.gait-home`.
`GAIT_HOME` is global machine state and is independent of the selected
`GAIT_DIR` workspace.

`ollama` and `lmstudio` are local servers that don't check the `Authorization` header. `gait auth add` is not required for them — set the URL below if you're not on the default port.

### openai-signin

`openai-signin` signs in with a ChatGPT plan through OpenAI's official "Sign in with ChatGPT" flow. Requests go to the public Responses API at `https://api.openai.com/v1` with a bearer token. No API key is needed, and usage counts against the ChatGPT plan's limits rather than API billing. Use model strings of the form `openai-signin/<model>`.

```bash
gait auth login openai-signin
```

The interactive login prints a URL. Open it in any browser and sign in. The browser then ends on a connection error page; that is expected. Paste the address the browser ends on back into the prompt.

When stdin is not a terminal, the login is two-step:

```bash
gait auth login openai-signin                        # prints JSON containing authorization_url
gait auth login openai-signin --complete '<pasted url>'
gait auth login openai-signin --complete -           # read the pasted URL from stdin
```

A pending login expires after 10 minutes. Tokens refresh automatically before each turn.

```bash
gait auth status openai-signin
gait auth logout openai-signin            # remove the login
gait auth logout openai-signin --forget   # also remove the saved app registration; needed to switch ChatGPT accounts
```

When the plan's usage limit is reached, the request fails with a message pointing to ChatGPT Settings > Usage. gait never falls back silently to another provider, and it does not retry usage-limit errors.

`openai-codex` above reuses the Codex CLI login and is the older, unofficial path. `openai-signin` is the supported one.

**Where credentials live.** Credentials are stored in `$GAIT_HOME/auth.json` (default `~/.gait-home`), mode 0600, with a lock file `auth.lock` and a short-lived pending-login file under `$GAIT_HOME/auth-pending/`. Never copy `auth.json` to another machine or into a repo: refresh tokens rotate, so a copy stops working as soon as either side refreshes. The model-facing `__brain` process receives only a short-lived access token, never the refresh token.

**Containers.** A login done inside a container is lost when the container exits, because gaitainer does not mount `$GAIT_HOME`. Log in on the host, or mount a persistent `GAIT_HOME` deliberately.

**Known limitation.** Tool sandboxes can currently read `$GAIT_HOME`, so a model-directed shell command could read `auth.json`. Do not use this provider on untrusted prompts with shell access until this is closed.

## Listing available models

```bash
gait models                 # all providers with configured credentials
gait models anthropic       # filter by substring on provider or model id
```

`gait models` is an idempotent network read (provider's own `/v1/models` endpoint). It does *not* mutate any state.

## Local models (Ollama / LM Studio)

This is the recommended local-model path. Ollama and LM Studio use Gait's
normal OpenAI-compatible provider boundary and do not require an experimental
Gait build.

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

## Embedded GGUF inference (experimental)

Gait also contains an experimental, opt-in llama.cpp/GGUF provider. It is not
compiled into the default binary, is not part of the stable provider contract,
and currently supports only a narrow set of model/tool-call conventions.

To build it explicitly:

```bash
cd gait-rs
cargo build -p gait --features local
```

The `gait local setup`, `gait local status`, and `gait local stop` commands are
available only in that feature build. GGUF paths and local inference metadata
are retained in the configuration schema for forward compatibility, but this
experimental implementation may change or be removed without a migration
promise. For normal local use, prefer Ollama or LM Studio above.

## Named inference profiles

A *named inference* is a saved `(provider, model, llm_params)` triple, stored in `<gait-dir>/inferences/<name>/inference.textproto`. Selecting one for a session injects an `Inference` event into the log, so the choice is replay-complete (a resumed session re-derives the same model and params from the log, not from ambient state).

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

For each turn, the spine resolves a typed credential before starting the sealed brain. The order:

1. The provider environment variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.).
2. The provider entry in gait's auth store: an owned API key/subscription or a non-secret external-source binding.
3. For an external binding, the selected source's canonical credential location.
4. For local providers (`ollama`, `lmstudio`): no key, proceed without one.

If the provider requires a credential and none is found, the turn refuses to start with a provider/source-specific error.

The auth store is `$GAIT_HOME/auth.json`; the models.dev cache is
`$GAIT_HOME/cache/models.json`.

## Switching models mid-session

Inside a session, the model is fixed at the start of each turn — the first LLM call already carries the resolved model and any `llm_params` overrides. To change the model mid-session:

- REPL: use `/model` (which calls the same `gait_ops::config::set_model` op the CLI uses), or `/inference select <name>` to switch to a named profile.
- CLI: use `gait config set model <model>` to change the workspace default, or `--model` on the bare-task form to override this turn only.
