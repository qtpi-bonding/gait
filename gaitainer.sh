#!/usr/bin/env bash
# gaitainer.sh — run gait inside its Docker container.
#
# Commands:
#   gaitainer.sh build               build (or rebuild) the image
#   gaitainer.sh chat  [gait-chat-args...]   interactive REPL in a container
#   gaitainer.sh run   [gait-run-args...]    headless CLI turn in a container
#
# Examples:
#   ./gaitainer.sh build
#   ./gaitainer.sh chat --gait-dir ~/gait-trace --codebase ~/myrepo
#   ./gaitainer.sh run  --gait-dir ~/gait-trace --codebase ~/myrepo --task "..." --auto

set -euo pipefail

IMAGE="${GAIT_IMAGE:-gait}"
REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"

cmd="${1:-}"
shift || true

case "$cmd" in
  build)
    exec docker build -t "$IMAGE" "$REPO_ROOT"
    ;;

  chat|run)
    # Parse --gait-dir and --codebase so we can bind-mount them.
    GAIT_DIR="" CODEBASE=""
    args=("$@")
    for i in "${!args[@]}"; do
      case "${args[$i]}" in
        --gait-dir) GAIT_DIR="${args[$((i+1))]:-}" ;;
        --codebase) CODEBASE="${args[$((i+1))]:-}" ;;
      esac
    done

    MOUNTS=()
    [[ -n "$GAIT_DIR"  ]] && MOUNTS+=(-v "$(realpath "$GAIT_DIR"):$(realpath "$GAIT_DIR")")
    [[ -n "$CODEBASE"  ]] && MOUNTS+=(-v "$(realpath "$CODEBASE"):$(realpath "$CODEBASE")")

    ENV_FLAGS=()
    for var in MINIMAX_API_KEY ANTHROPIC_API_KEY OPENAI_API_KEY; do
      [[ -n "${!var:-}" ]] && ENV_FLAGS+=(-e "$var=${!var}")
    done

    TTY_FLAGS=()
    [[ "$cmd" == "chat" ]] && TTY_FLAGS=(-it)

    exec docker run --rm \
      "${TTY_FLAGS[@]}" \
      "${MOUNTS[@]}" \
      "${ENV_FLAGS[@]}" \
      "$IMAGE" "$cmd" "$@"
    ;;

  *)
    echo "usage: gaitainer.sh <build|chat|run> [args...]" >&2
    exit 1
    ;;
esac
