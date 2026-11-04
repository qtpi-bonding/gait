# Messages, deliveries and agent identity (gait-acp)

## Flags (process-level)

| Flag | Meaning |
|---|---|
| `--require-attestation` | Refuse prompts that do not carry a verified Delivery. Needs an identity and a state dir. |
| `--trusted-deliverer p256:<hex>` | Repeatable. A principal allowed to deliver messages to this agent. The Delivery carries the signer's root-to-device delegation. |
| `--state-dir <dir>` | Where replay and dedupe state live (`<dir>/attestation/<agent id>/`). Falls back to `$GAIT_DIR`. |
| `--reply-instructions-file <path>` | Text added to the self-identity block (how replies leave). `GAIT_AGENT_NAME` supplies the name. |
| `--send-command <program>` | Enables the `send_message` reply tool. Repeat `--send-arg <arg>` for its argv (values taken verbatim, no shell). Requires `--send-audience`. |
| `--send-audience p256:<hex>` | The deliverer principal every send is addressed to (separate from `--trusted-deliverer`). Requires `--send-command`. |

`send_message` is offered only when configured. The model gives targets and a body; the spine signs with the agent's device session (never the root) and runs the command with only the allowlisted environment (no `GAIT_AGENT_DIR`, no provider keys). Each send goes through the normal permission gate (`send_message` pattern), so `--auto` runs and `--allow send_message` send without a prompt.

`--trusted-multimatrix-key` and `--trusted-recipient-key` were removed and are refused at start.

## Identity modes

- `GAIT_AGENT_DIR` set: load only. The folder is never minted over, and renewed only if gait minted it.
- Unset, with a state dir: the top gait-acp process mints a split identity: device folder `<state>/identity`, root `<state>/identity-root` (outside every sandbox grant). It exports `GAIT_AGENT_DIR`; role children only load. Delegations last 30 days and are renewed when 15 days remain.
- Plain `gait` CLI: no identity.

## Log fields

`IncomingMessage` stores `message_id`, `to`, `reply_to`, `references`, `mentions`, `via` and `render_version`; the author is `Provenance.author_id`. The model sees `<incoming-message …>` rendered from those fields at fold time (an unknown `render_version` or an unstructured message is left out of context). `Provenance.context` was removed.

## Known test gaps

Not written yet (covered only by the concordium batch stack run):

- Unit tests for `attestation.rs` itself (its verify paths are exercised by `crates/gait-acp/tests/prompt_provenance.rs`).
- Two gait-acp starts racing on one state dir mint a single identity.
- An event signed by a device renewed after start verifies after `DirectVerifier::refresh`.
- Integration tests for the self-identity block (once per session; absent without identity). Only the block builder is unit-tested.
- `send_message`: the send child is not enrolled in gait's cascade-kill group (the library runs it in its own process group with a timeout and kills it on exit).
- A send cut off by a timeout or an aborted turn may already have been accepted; a retry mints a new message id, so the recipient can see a duplicate.

## Identity folders are denied to the model (macOS only; Linux is exposed)

**macOS.** The device folder and the minted root folder are in the hand's `read_deny` and `write_deny` (the hand builds these itself, never from the brain). The spine names a handed `GAIT_AGENT_DIR` through `GAIT_IDENTITY_DENY`; `<gait_dir>/identity` and `<gait_dir>/identity-root` are always covered. The Seatbelt profile emits `deny file-read* file-write*` for deny subpaths after the allows. This is tested with a real `sandbox-exec`, NOT with the in-process `sandbox_apply` path gait uses; an earlier kernel deny attempt was reverted after a macOS sandbox-compiler quirk silently dropped trailing `deny` rules, so the in-process path needs its own check.

**Linux: the key is exposed to the model.** Accepted for the MVP (single user, private tailnet); fix before anything untrusted reaches an agent.

- The Landlock sandbox is a stub (`LandlockSandbox::apply` returns `Unavailable`); the hand and brain log that and run unsealed.
- The only guard is the hand's pre-check, which covers the read, write and apply_patch tools only. `shell` is not pre-checked, so `cat <GAIT_AGENT_DIR>/device.jwk.json` succeeds. The pre-check also matches path prefixes without resolving symlinks or `..`.
- An unsealed shell running as the same uid can read `/proc/<spine pid>/environ`, which holds `GAIT_AGENT_DIR` and the provider API keys.

Follow-up options (tracked in concordium's PLAN.md): run the hands as a different uid, or move signing into a separate process under another uid.
