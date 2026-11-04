# Messages, deliveries and agent identity (gait-acp)

## Flags (process-level)

| Flag | Meaning |
|---|---|
| `--require-attestation` | Refuse prompts that do not carry a verified Delivery. Needs an identity and a state dir. |
| `--trusted-deliverer p256:<hex>` | Repeatable. A principal allowed to deliver messages to this agent. The Delivery carries the signer's root-to-device delegation. |
| `--state-dir <dir>` | Where replay and dedupe state live (`<dir>/attestation/<agent id>/`). Falls back to `$GAIT_DIR`. |
| `--reply-instructions-file <path>` | Text added to the self-identity block (how replies leave). `GAIT_AGENT_NAME` supplies the name. |

`--trusted-multimatrix-key` and `--trusted-recipient-key` were removed and are refused at start.

## Identity modes

- `GAIT_AGENT_DIR` set: load only. The folder is never minted over, and renewed only if gait minted it.
- Unset, with a state dir: the top gait-acp process mints a split identity: device folder `<state>/identity`, root `<state>/identity-root` (outside every sandbox grant). It exports `GAIT_AGENT_DIR`; role children only load. Delegations last 30 days and are renewed when 15 days remain.
- Plain `gait` CLI: no identity.

## Log fields

`IncomingMessage` stores `message_id`, `to`, `reply_to`, `references`, `mentions`, `via` and `render_version`; the author is `Provenance.author_id`. The model sees `<incoming-message …>` rendered from those fields at fold time (an unknown `render_version` or an unstructured message is left out of context). `Provenance.context` was removed.
