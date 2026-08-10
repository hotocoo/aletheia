#!/usr/bin/env bash
# The model drives a MULTI-STEP session at a live console (REQ-AI-007, ADR-054).
#
# `scripts/console-ai-e2e.sh` proves one request becomes one validated command that a booted machine
# accepts. It cannot prove anything about a request whose answer is not reachable in one command,
# and it papered over that with a `case "$planned" in write*|rm*|mv*|cp*|touch*|append*)` list in
# shell — a second list of the kernel's commands, in the one language nothing in this repository
# tests, deciding when the model's picture of the machine was stale.
#
# This gate deletes that list by giving the model the machine's own answer instead. Per case:
#
#   1. the guest boots with `--features interactive` and is given a fixture through the console
#   2. the driver types `ls` once and captures it — the opening brief, read off the live machine
#   3. `aletheiad console agent` is asked for the NEXT command; it validates, authorizes and renders
#      exactly one line, and charges one step to the session's budget
#   4. the driver types that line and hands the console's reply straight back through
#      `--observation-file`; Aletheia admits it as data and shows it to the model
#   5. repeat until the model answers (exit 10), a bound refuses (exit 1), or the driver's own cap
#      trips — which would itself be a failure, because the budget is supposed to be the thing that
#      stops this
#
# The asserted claim per case is `must_type`: the LAST command of the sequence, the one that is
# meaningless unless the earlier ones ran. `cat backup` cannot be planned before `cp manifesto
# backup` has happened, so a session that typed it is a session in which the model saw the machine
# move and acted on it. That is the whole wave, and it is asserted against a live guest.
#
# Two arms, exactly like every other AI gate here. The DETERMINISTIC arm needs no model and must pass
# everywhere; it is an oracle for the loop, the rendering and the typing path, and it additionally
# asserts the answer's content, which a model's prose cannot be held to without turning this gate
# into a string-match on English. The MODEL arm runs only when a backend really is serving the
# selected model and SKIPs — never silently passes — when it is not.
#
# RUNNING THE MODEL ARM. Operator-started, like the single-step gate:
#
#     llama-server -m "$(aletheiad model status | sed -n 's/^weights: *present — //p')" \
#                  -c 8192 --port 8099 --host 127.0.0.1 --jinja
#     MODEL_ENDPOINT=http://127.0.0.1:8099 ./scripts/console-agent-e2e.sh
#
# `--jinja` is not optional: without it `llama-server` never parses the model's tool call, the agent
# sees a response with no call and no content, and a correct answer reads as no answer at all.
#
# What this does NOT claim: there is still no inference engine in kernel space. `kernel-core` remains
# `no_std` with no network and no model. Every model call happens on the host, and what crosses into
# the guest is a validated line of printable ASCII, indistinguishable from one a person typed.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

ALETHEIAD="$ROOT/aletheia/target/release/aletheiad"
BODY="the OS you can sit in front of"      # 30 bytes, matching console-e2e.sh
POEM="hello world!"                        # 12 bytes
# The driver's own cap on turns. It is deliberately HIGHER than the agent's default budget of 6:
# this number exists so a broken loop cannot hang a CI job, and if it is ever the thing that stops a
# session then the budget did not, which is a defect in the budget rather than a case that needed
# more room.
DRIVER_MAX_TURNS=10

fail=0
declare -a RESULTS=()
hr() { printf '========================================================================\n'; }

# ---------------------------------------------------------------------------------------------
# One long-lived console session, held open so the driver can have a conversation with it. Same
# mechanism as console-ai-e2e.sh, and the same reason: what is typed next depends on what the
# machine just said.
# ---------------------------------------------------------------------------------------------
SESSION_LOG=""
SESSION_PID=""
SESSION_FIFO=""
SESSION_WATCHDOG=""
CAPTURED=""

prompt_count() {
  # `grep -c` prints 0 and EXITS 1 with no match, so `|| echo 0` would print a SECOND zero and every
  # later integer test would die. Same trap console-e2e.sh documents.
  local n; n="$(grep -c "aletheia> " "$SESSION_LOG" 2>/dev/null)"
  printf '%s' "${n:-0}"
}

session_open() {
  SESSION_LOG="$(mktemp)"
  SESSION_FIFO="$(mktemp -u)"
  mkfifo "$SESSION_FIFO"
  : > "$SESSION_LOG"
  exec 3<>"$SESSION_FIFO"
  "$@" < "$SESSION_FIFO" > "$SESSION_LOG" 2>&1 &
  SESSION_PID=$!
  ( sleep 600; kill -9 "$SESSION_PID" 2>/dev/null ) &
  SESSION_WATCHDOG=$!
  local waited=0
  while ! grep -q "aletheia> " "$SESSION_LOG" 2>/dev/null; do
    kill -0 "$SESSION_PID" 2>/dev/null || return 1
    sleep 1; waited=$((waited + 1))
    [ "$waited" -ge 180 ] && return 1
  done
  return 0
}

session_type() {
  local line="$1" want cur spun=0 before
  before="$(wc -l < "$SESSION_LOG")"
  want=$(( $(prompt_count) + 1 ))
  printf '%s\r' "$line" >&3
  if [ "$line" = "halt" ]; then sleep 2; CAPTURED=""; return 0; fi
  while : ; do
    cur="$(prompt_count)"
    [ "$cur" -ge "$want" ] && break
    kill -0 "$SESSION_PID" 2>/dev/null || break
    sleep 1; spun=$((spun + 1))
    [ "$spun" -ge 30 ] && break
  done
  CAPTURED="$(tail -n "+$((before + 1))" "$SESSION_LOG")"
}

session_close() {
  session_type "halt" >/dev/null 2>&1
  wait "$SESSION_PID" 2>/dev/null; CONSOLE_RC=$?
  kill "$SESSION_WATCHDOG" 2>/dev/null
  exec 3>&- 2>/dev/null
  rm -f "$SESSION_FIFO"
}

# What the machine printed, with the wire's artifacts removed and NOTHING else: the trailing CR a
# serial line puts on every line, the prompt, and the echo of the command just typed. Aletheia does
# the rest — bounding it, stripping control bytes, marking a truncation — in `agent::admit_observation`,
# where there are tests. Doing any of that here would be the same mistake this gate exists to undo.
capture_to_observation() {
  local typed="$1" file="$2"
  printf '%s\n' "$CAPTURED" \
    | tr -d '\r' \
    | sed -e 's/aletheia> //g' -e "/^${typed//\//\\/}$/d" -e '/^[[:space:]]*$/d' \
    > "$file"
}

refresh_brief() {
  local brief_file="$1"
  session_type "ls"
  {
    printf '  objects on this machine:\n'
    printf '%s\n' "$CAPTURED" \
      | tr -d '\r' \
      | sed -e 's/aletheia> //g' -e '/^ls$/d' -e '/^[[:space:]]*$/d' \
      | sed -e 's/^/    /'
  } > "$brief_file"
}

# ---------------------------------------------------------------------------------------------
# One case: drive the loop to an answer, then assert what was typed at the live machine.
# ---------------------------------------------------------------------------------------------
run_case() {
  local label="$1" arm="$2" natural="$3" scripted="$4" must_type="$5" console_says="$6" \
        answer_contains="$7" approved="$8" brief="$9"
  local request; [ "$arm" = "model" ] && request="$natural" || request="$scripted"
  local transcript obs err
  transcript="$(mktemp -u)"; obs="$(mktemp)"; err="$(mktemp)"
  local -a typed=()
  local turn=0 line rc answer="" saw_console_says=0 bad=0

  echo "--> \"$request\""
  while [ "$turn" -lt "$DRIVER_MAX_TURNS" ]; do
    local -a argv=("$ALETHEIAD" console agent --transcript "$transcript"
                   --interpreter "$arm" --context-file "$brief")
    [ "$approved" = "true" ] && argv+=(--approve)
    [ "$turn" -gt 0 ] && argv+=(--observation-file "$obs")
    line="$("${argv[@]}" "$request" 2>"$err")"; rc=$?
    case "$rc" in
      0)
        [ -z "$line" ] && { echo "  FAIL [$label/$arm] exit 0 with no line to type"; bad=1; break; }
        echo "    step $((turn + 1)): $line"
        session_type "$line"
        typed+=("$line")
        grep -qF "$console_says" <<<"$CAPTURED" && saw_console_says=1
        capture_to_observation "$line" "$obs"
        sed 's/^/        | /' "$obs"
        ;;
      10)
        answer="$(sed -n 's/^answer: //p' "$err" | tail -1)"
        echo "    answered: $answer"
        break
        ;;
      *)
        echo "  FAIL [$label/$arm] the session refused: $(sed -n 's/^refused: //p' "$err" | tail -1)"
        bad=1
        break
        ;;
    esac
    turn=$((turn + 1))
  done

  if [ "$bad" -eq 0 ]; then
    if [ -z "$answer" ]; then
      echo "  FAIL [$label/$arm] the session never answered within $DRIVER_MAX_TURNS turns — the budget did not stop it"
      bad=1
    fi
    # The claim: the last, dependent command really was typed at the machine.
    if ! printf '%s\n' "${typed[@]:-}" | grep -qxF "$must_type"; then
      echo "  FAIL [$label/$arm] the session never typed: $must_type"
      printf '    typed: %s\n' "${typed[@]:-<nothing>}"
      bad=1
    elif [ "$saw_console_says" -eq 0 ]; then
      echo "  FAIL [$label/$arm] the live console never printed: $console_says"
      bad=1
    fi
    # And the answer's content — control arm only. See the header.
    if [ "$arm" = "deterministic" ] && ! grep -qF "$answer_contains" <<<"$answer"; then
      echo "  FAIL [$label/$arm] the answer does not contain: $answer_contains"
      bad=1
    fi
  fi
  [ "$bad" -eq 0 ] && echo "    OK — reached \`$must_type\` and answered"
  rm -f "$transcript" "$obs" "$err"
  return "$bad"
}

# ---------------------------------------------------------------------------------------------
# The bounds, at the CLI, against the same booted machine. Each one is a claim that a session
# REFUSES, and a refusal that only holds in a unit test is a refusal nobody has seen the system make.
#
# ALWAYS driven by the DETERMINISTIC arm, whichever arm the cases are running under, and that is not
# a convenience. These are properties of `agent::advance` — of Aletheia — and proving them through a
# language model proves them about one model's mood. The first live run said so out loud: asked for
# `"rm poem"`, the model proposed something harmless instead, a line was correctly rendered, and this
# section reported *"a destructive step was rendered without approval"* — an alarm about Aletheia
# raised by the model declining to be destructive. A bound asserted through a model is a bound
# re-litigated every time the model changes its mind.
# ---------------------------------------------------------------------------------------------
run_bounds() {
  local label="$1" brief="$2" bad=0 t out rc
  local arm=deterministic
  echo "--- the bounds (always the deterministic arm: these are properties of Aletheia)"

  # 1. Destructive with no approval: refused, and stdout is EMPTY. If a line ever leaks to stdout
  #    here the driver types it, which is the one failure mode this whole contract exists to prevent.
  t="$(mktemp -u)"
  out="$("$ALETHEIAD" console agent --transcript "$t" --interpreter "$arm" --context-file "$brief" \
        "rm poem" 2>/dev/null)"; rc=$?
  if [ "$rc" -eq 0 ] || [ -n "$out" ]; then
    echo "  FAIL [$label/$arm] a destructive step was rendered without approval"; bad=1
  else
    echo "    unapproved destructive step refused, nothing on stdout"
  fi
  rm -f "$t"

  # 2. Stopping the machine: refused even WITH approval, because the session could not observe it.
  t="$(mktemp -u)"
  out="$("$ALETHEIAD" console agent --transcript "$t" --interpreter "$arm" --context-file "$brief" \
        --approve "halt" 2>/dev/null)"; rc=$?
  if [ "$rc" -eq 0 ] || [ -n "$out" ]; then
    echo "  FAIL [$label/$arm] a session was allowed to stop the machine it is reading"; bad=1
  else
    echo "    halt refused even with approval"
  fi
  rm -f "$t"

  # 3. The budget. One step is granted for a two-step task; the second turn must refuse rather than
  #    run, and it must refuse without typing anything.
  t="$(mktemp -u)"; local o; o="$(mktemp)"
  out="$("$ALETHEIAD" console agent --transcript "$t" --interpreter "$arm" --context-file "$brief" \
        --budget 1 "ls ; wc poem" 2>/dev/null)"; rc=$?
  if [ "$rc" -ne 0 ] || [ -z "$out" ]; then
    echo "  FAIL [$label/$arm] the first step of a budget-1 session did not render"; bad=1
  else
    session_type "$out"
    capture_to_observation "$out" "$o"
    out="$("$ALETHEIAD" console agent --transcript "$t" --interpreter "$arm" --context-file "$brief" \
          --budget 1 --observation-file "$o" "ls ; wc poem" 2>/dev/null)"; rc=$?
    if [ "$rc" -eq 0 ] || [ -n "$out" ]; then
      echo "  FAIL [$label/$arm] a session ran past its budget"; bad=1
    else
      echo "    the budget stopped the session, and it typed nothing more"
    fi
  fi
  rm -f "$t" "$o"

  # 4. No progress: the same command twice. The second one is refused before it costs a step.
  t="$(mktemp -u)"; o="$(mktemp)"
  out="$("$ALETHEIAD" console agent --transcript "$t" --interpreter "$arm" --context-file "$brief" \
        "ls ; ls" 2>/dev/null)"; rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "  FAIL [$label/$arm] the first step of the repeat case did not render"; bad=1
  else
    session_type "$out"
    capture_to_observation "$out" "$o"
    out="$("$ALETHEIAD" console agent --transcript "$t" --interpreter "$arm" --context-file "$brief" \
          --observation-file "$o" "ls ; ls" 2>/dev/null)"; rc=$?
    if [ "$rc" -eq 0 ] || [ -n "$out" ]; then
      echo "  FAIL [$label/$arm] a command whose answer was already in the transcript ran again"; bad=1
    else
      echo "    a repeated command was refused as no progress"
    fi
  fi
  rm -f "$t" "$o"

  return "$bad"
}

# ---------------------------------------------------------------------------------------------
run_arm() {
  local label="$1" arm="$2"; shift 2
  local bad=0
  hr; echo "==> $label / $arm arm"; hr

  if ! session_open "$@"; then
    echo "  FAIL [$label/$arm] the machine never reached a prompt"
    session_close; return 1
  fi

  # The fixture, typed by the DRIVER: a gate whose setup depends on the thing under test cannot
  # report which of the two broke.
  session_type "write manifesto $BODY"
  grep -q "wrote 30 bytes to manifesto" <<<"$CAPTURED" \
    || { echo "  FAIL [$label/$arm] fixture: manifesto was not written"; bad=1; }
  session_type "write poem $POEM"
  grep -q "wrote 12 bytes to poem" <<<"$CAPTURED" \
    || { echo "  FAIL [$label/$arm] fixture: poem was not written"; bad=1; }

  local brief; brief="$(mktemp)"
  refresh_brief "$brief"
  grep -q "manifesto" "$brief" || { echo "  FAIL [$label/$arm] the brief did not see the namespace"; bad=1; }
  echo "--- opening brief, read off the live machine:"; sed 's/^/    /' "$brief"

  local natural scripted must_type says answer_contains approved
  while IFS=$'\t' read -r natural scripted must_type says answer_contains approved; do
    [ -z "${natural:-}" ] && continue
    run_case "$label" "$arm" "$natural" "$scripted" "$must_type" "$says" \
             "$answer_contains" "$approved" "$brief" || bad=1
  done < <("$ALETHEIAD" console agent-cases)

  run_bounds "$label" "$brief" || bad=1

  # The fixture survived every refusal above.
  session_type "cat poem"
  grep -qF "$POEM" <<<"$CAPTURED" \
    || { echo "  FAIL [$label/$arm] poem did not survive the refused sessions"; bad=1; }

  session_close

  # The reboot leg: what an AGENT changed has to survive a power cycle, or "the agent copied an
  # object" means only "the agent copied an object into RAM".
  if ! session_open "$@"; then
    echo "  FAIL [$label/$arm] the machine did not come back for the reboot leg"; bad=1
  else
    grep -q "persistent virtio-blk device" "$SESSION_LOG" \
      || { echo "  FAIL [$label/$arm] the second boot did not mount the persistent device"; bad=1; }
    session_type "cat backup"
    grep -qF "$BODY" <<<"$CAPTURED" \
      || { echo "  FAIL [$label/$arm] the copy the agent made did NOT survive the reboot"; bad=1; }
    echo "--> after a power cycle: the object the agent created is still there"
    session_close
  fi

  rm -f "$brief" "$SESSION_LOG"
  [ "$bad" -eq 0 ] || fail=1
  return $bad
}

model_available() {
  "$ALETHEIAD" console bench >/dev/null 2>&1
  local rc=$?
  # 2 = refused before measuring (nothing serving, or serving something else). 0 or 1 both mean a
  # model answered.
  [ "$rc" -ne 2 ]
}

hr; echo "==> building the hosted agent and the interactive kernel"; hr
( cd "$ROOT/aletheia" && cargo build --release ) || { echo "FAIL: aletheiad did not build"; exit 1; }
( cd "$ROOT/kernel" && cargo build --features interactive ) || { echo "FAIL: kernel did not build"; exit 1; }

# TWO disks: the console picks the SECOND virtio-blk device as its persistent medium and uses the
# first as scratch. Booted with one, every read-only case still passes and nothing survives a reboot.
SCRATCH="$ROOT/kernel/target/console-agent-scratch.img"
PERSIST="$ROOT/kernel/target/console-agent-persistent.img"
fresh_disks() {
  rm -f "$SCRATCH" "$PERSIST"
  dd if=/dev/zero of="$SCRATCH" bs=1048576 count=1 2>/dev/null
  dd if=/dev/zero of="$PERSIST" bs=1048576 count=1 2>/dev/null
}
fresh_disks
ELF="$ROOT/kernel/target/aarch64-unknown-none-softfloat/debug/aletheia-kernel"
QEMU=(qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 128M -nographic
  -semihosting-config enable=on,target=native -kernel "$ELF"
  -global virtio-mmio.force-legacy=false
  -drive "if=none,format=raw,file=$SCRATCH,id=blk0" -device virtio-blk-device,drive=blk0
  -drive "if=none,format=raw,file=$PERSIST,id=blk1" -device virtio-blk-device,drive=blk1)

run_arm "aarch64" deterministic "${QEMU[@]}"
RESULTS+=("aarch64 / deterministic : $([ $? -eq 0 ] && echo PASS || echo FAIL)")

fresh_disks
if model_available; then
  run_arm "aarch64" model "${QEMU[@]}"
  RESULTS+=("aarch64 / model         : $([ $? -eq 0 ] && echo PASS || echo FAIL)")
else
  RESULTS+=("aarch64 / model         : SKIP (no backend is serving the selected model)")
  echo "SKIP: the model arm needs the selected model actually being served — see \`aletheiad model status\`"
fi

hr; printf '%s\n' "${RESULTS[@]}"; hr
[ "$fail" -eq 0 ] && echo "console-agent-e2e: PASS" || echo "console-agent-e2e: FAIL"
exit "$fail"
