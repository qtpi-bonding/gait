# Source this (don't execute it) to prepare the throwaway project and gait-dir
# that demo.tape records. It exports GAIT_DIR, cd's into the demo repo, and
# shortens the prompt.
#
# Needs `gait` on PATH and a logged-in provider (for example
# `gait auth link openai-codex codex`). Override the model with
# GAIT_DEMO_MODEL=<provider>/<model-id> and the location with GAIT_DEMO_DIR.

# /tmp keeps the recorded paths free of any home directory.
GAIT_DEMO_DIR="${GAIT_DEMO_DIR:-/tmp/gait-demo}"
GAIT_DEMO_MODEL="${GAIT_DEMO_MODEL:-openai-codex/gpt-5.6-luna}"

# Only ever delete a directory this script created.
if [ -e "$GAIT_DEMO_DIR" ]; then
  if [ -f "$GAIT_DEMO_DIR/.gait-demo" ]; then
    rm -rf "$GAIT_DEMO_DIR"
  else
    echo "refusing to reuse $GAIT_DEMO_DIR: not created by this script" >&2
    return 1 2>/dev/null || exit 1
  fi
fi

mkdir -p "$GAIT_DEMO_DIR/repo"
touch "$GAIT_DEMO_DIR/.gait-demo"

cat > "$GAIT_DEMO_DIR/repo/main.py" <<'PY'
import argparse


def greet(name):
    return f"Hello, {name}!"


def main():
    parser = argparse.ArgumentParser(description="Greet someone.")
    parser.add_argument("name", help="who to greet")
    args = parser.parse_args()
    print(greet(args.name))


if __name__ == "__main__":
    main()
PY
git -C "$GAIT_DEMO_DIR/repo" init -q -b main
git -C "$GAIT_DEMO_DIR/repo" add -A
git -C "$GAIT_DEMO_DIR/repo" -c user.name=demo -c user.email=demo@example.com commit -q -m initial

export GAIT_DIR="$GAIT_DEMO_DIR/home"
gait init "$GAIT_DIR" > /dev/null
gait inference --gait-dir "$GAIT_DIR" --model "$GAIT_DEMO_MODEL" \
  --compaction-threshold-tokens 200000 add demo > /dev/null
gait config --gait-dir "$GAIT_DIR" set inference demo > /dev/null
# The REPL ignores --allow, so permissions come from this list.
gait config --gait-dir "$GAIT_DIR" set allow \
  '["read","glob","grep","ls","clarify","inform","write","apply_patch"]' > /dev/null

export PS1='$ '
cd "$GAIT_DEMO_DIR/repo" || return 1
