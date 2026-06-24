#!/usr/bin/env bash
# gait-rs/scripts/refresh-models.sh
# Vendor a metadata-only models.dev snapshot. Run by hand; commit the output.
# SECURITY: the field allowlist below is the enforcement point — the snapshot
# must never carry api/headers/env/npm (routing/auth). See providers-as-data spec.
set -euo pipefail

OUT_DIR="$(cd "$(dirname "$0")/../src" && pwd)"
KEEP='["anthropic","minimax","openai","groq","mistral","openrouter","deepinfra","xai","cerebras","togetherai","fireworks","perplexity"]'

curl -fsSL https://models.dev/api.json \
  | jq --argjson keep "$KEEP" '
    to_entries
    | map(select(.key as $k | $keep | index($k)))
    | map({
        key: .key,
        value: {
          models: (.value.models | map_values({
            cost: .cost,
            limit: .limit,
            modalities: .modalities,
            reasoning: (.reasoning // false),
            tool_call: (.tool_call // false),
            attachment: (.attachment // false)
          }))
        }
      })
    | from_entries
  ' > "$OUT_DIR/models_snapshot.json"

date -u +"source: models.dev  fetched: %Y-%m-%dT%H:%M:%SZ" > "$OUT_DIR/models_snapshot.meta"
echo "wrote $OUT_DIR/models_snapshot.json ($(jq 'length' "$OUT_DIR/models_snapshot.json") providers)"
