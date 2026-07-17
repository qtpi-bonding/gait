# Third-party notices

## OpenAI Codex login and model-provider references

Some forthcoming `gait-auth` and Codex-provider code is derived from or adapted
from [OpenAI Codex](https://github.com/openai/codex) at commit
`315195492c80fdade38e917c18f9584efd599304` (checked out 2026-07-16), licensed
under the Apache License 2.0.

Relevant upstream files and any adapted gait modules will be listed here as
they are added. See the upstream [`LICENSE`](https://github.com/openai/codex/blob/315195492c80fdade38e917c18f9584efd599304/LICENSE)
and [`NOTICE`](https://github.com/openai/codex/blob/315195492c80fdade38e917c18f9584efd599304/NOTICE).

- `crates/gait-auth/src/codex.rs` adapts the PKCE, device-code, token exchange,
  refresh, token-claim, and revoke patterns from the upstream files cited in
  its module header.
- `crates/gait-provider/src/codex.rs` adapts the subscription backend route
  and account-header patterns from `codex-rs/model-provider-info/src/lib.rs`
  and `codex-rs/model-provider/src/bearer_auth_provider.rs` at that revision.
