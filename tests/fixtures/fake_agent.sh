#!/usr/bin/env bash
# Fake interactive coding-agent TUI for meguri integration tests.
#
# Behavior is driven by single-line commands typed at its prompt:
#   work <secs>            print activity for <secs> seconds, then prompt again
#   block                  show a permission-question screen until any input
#   result <status>        write .meguri/result.json for the current turn and prompt
#   do <prompt-trigger>    the meguri-style trigger line: reads the referenced
#                          prompt file; obeys FAKE_AGENT_SCRIPT (see below)
#   exit                   quit
#
# FAKE_AGENT_SCRIPT (env): comma-separated actions performed on `do`, e.g.
#   "work:2,result:success"          work 2s then write a success result
#   "work:1,block,result:success"    work, ask a question, then (after human
#                                     answers) write the result
# The turn id is read from the prompt file line `turn_id: <uuid>`.

set -u
MEGURI_DIR=".meguri"
CURRENT_TURN=""

banner() {
  echo "┌──────────────────────────────┐"
  echo "│  fake-agent v0.1 (interactive)│"
  echo "└──────────────────────────────┘"
}

write_result() {
  local status="$1"
  mkdir -p "$MEGURI_DIR"
  printf '{"turn_id":"%s","status":"%s","summary":"fake agent finished with %s"}\n' \
    "$CURRENT_TURN" "$status" "$status" > "$MEGURI_DIR/result.json"
  echo "[fake-agent] wrote result.json (status=$status, turn=$CURRENT_TURN)"
}

do_work() {
  local secs="${1:-2}"
  for ((i = 0; i < secs * 2; i++)); do
    echo "[fake-agent] working... step $i $(date +%s%N | tail -c 6)"
    sleep 0.5
  done
}

do_block() {
  echo ""
  echo "Do you want to allow this tool call?"
  echo "❯ 1. Yes"
  echo "  2. No"
  # shellcheck disable=SC2162
  read answer
  # Real agent TUIs redraw the dialog away once answered; emulate with a clear.
  printf '\033[2J\033[H'
  echo "[fake-agent] unblocked by: $answer"
}

handle_do() {
  local trigger="$*"
  # extract the prompt file path from the trigger line
  local file
  file=$(echo "$trigger" | grep -o '\.meguri/prompt-[a-zA-Z0-9-]*\.md' | head -1)
  if [[ -n "$file" && -f "$file" ]]; then
    CURRENT_TURN=$(grep -o 'turn_id: [a-zA-Z0-9-]*' "$file" | head -1 | cut -d' ' -f2)
    echo "[fake-agent] read prompt file $file (turn=$CURRENT_TURN)"
  else
    echo "[fake-agent] no prompt file found in: $trigger"
  fi
  local script="${FAKE_AGENT_SCRIPT:-work:1,result:success}"
  IFS=',' read -ra actions <<< "$script"
  for action in "${actions[@]}"; do
    case "$action" in
      work:*) do_work "${action#work:}" ;;
      block) do_block ;;
      result:*) write_result "${action#result:}" ;;
      silent) echo "[fake-agent] going silent (no result)" ;;
    esac
  done
}

banner
# Real agent CLIs accept an initial prompt as an argument (turn 1 spawns the
# pane with the trigger line as argv); emulate that here.
if [[ $# -gt 0 ]]; then
  echo "> $*"
  case "$1" in
    do|Read|read) handle_do "$*" ;;
    *) handle_do "$*" ;;
  esac
fi
while true; do
  printf '> '
  # shellcheck disable=SC2162
  if ! read line; then
    break
  fi
  case "$line" in
    exit) break ;;
    work\ *) do_work "${line#work }" ;;
    work) do_work 2 ;;
    block) do_block ;;
    result\ *) write_result "${line#result }" ;;
    do\ *|Read*|read*) handle_do "$line" ;;
    "") ;;
    *) echo "[fake-agent] unknown: $line" ;;
  esac
done
echo "[fake-agent] bye"
