# UI Module Extraction — Notes

Extract a `ui/` module so `chat.rs` becomes a thin orchestration loop and each widget owns its
terminal state cleanly.

## Proposed layout

```
src/ui/
  mod.rs          — re-exports
  spinner.rs      — Spinner { start(), stop() }
  renderer.rs     — move from render.rs
  permission.rs   — PermissionPrompt { ask() -> Choice }
  clarify.rs      — ClarifyPrompt { ask() -> Vec<String> }
  inform.rs       — InformPrompt { show() }
```

Each widget owns its terminal state (enter/exit cleanly). `chat.rs` becomes a thin orchestration
loop that assembles widgets.

## Also: replace spinner with timing display

Instead of the braille spinner, show a live-updating timing line while the model is thinking:

```
thinking… [4s]
```

Updated in-place each second. Simpler, no animation, more useful.

## Status

Not started. Capture for future planning.
