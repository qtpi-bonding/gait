# README demo

`demo.tape` is a [VHS](https://github.com/charmbracelet/vhs) script that records a short REPL session: two
turns, then `/undo 1` to roll the second one back.

```bash
brew install vhs        # also installs ttyd and ffmpeg
export PATH=/path/to/gait/target/debug:$PATH
vhs docs/demo/demo.tape # from the repo root; writes docs/demo/demo.gif
```

`setup.sh` builds a throwaway repo and gait-dir in `/tmp/gait-demo` (override with `GAIT_DEMO_DIR`) and uses
`GAIT_DEMO_MODEL` (default `openai-codex/gpt-5.6-luna`), so you need a logged-in provider. The run makes real
model calls; the tape hides the waiting time. Check the rendered GIF for stray paths or credentials before
publishing it.
