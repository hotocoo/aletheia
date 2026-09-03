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
# Every turn's wall time and how many model calls it cost, across one arm. Reset per arm, because a
# median that mixes the deterministic control arm's zero-millisecond turns with a model's would
# report a speed no configuration actually has.
declare -a TURN_MS=() TURN_CALLS=()

# The MIDDLE turn, not the mean: one cold start or one page fault moves a mean and does not move a
# median, and the number is here to describe what a turn usually costs.
median_of() {
  local -a xs=("$@")
  [ "${#xs[@]}" -eq 0 ] && { printf 'n/a'; return; }
  local -a sorted
  IFS=$'\n' read -r -d '' -a sorted < <(printf '%s\n' "${xs[@]}" | sort -n; printf '\0')
  printf '%s' "${sorted[$(( ${#sorted[@]} / 2 ))]}"
}

sum_of() {
  local t=0 x
  for x in "$@"; do t=$((t + x)); done
  printf '%s' "$t"
}
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

# Some targets need something done between power cycles that is not part of the argv. x86-64 under
# OVMF is the case that forced this: the firmware WRITES its NVRAM, so a second boot from the same
# vars file is not a second boot of the same machine, it is a boot of whatever the first one left
# behind. A leg sets PRE_OPEN to the name of a function; everything else leaves it empty.
PRE_OPEN=""

session_open() {
  [ -n "$PRE_OPEN" ] && "$PRE_OPEN"
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

# Wait until the machine stops talking. The console prints its prompt as part of a banner that
# is still arriving when the first command is typed, so a `want = prompts + 1` computed mid-burst
# can be satisfied by a prompt the machine had ALREADY decided to print — the wait then ends
# before the command ran and the capture misses its output (seen on a CI runner 2026-09-03: the
# object was written, and the gate said it was not). Settling first makes the count mean what it
# says: one more prompt than the machine has printed by the time the line goes out.
session_settle() {
  local a b spun=0
  a="$(wc -l < "$SESSION_LOG")"
  while : ; do
    sleep 0.5
    b="$(wc -l < "$SESSION_LOG")"
    [ "$a" = "$b" ] && return 0
    a="$b"; spun=$((spun + 1))
    [ "$spun" -ge 40 ] && return 0   # 20 s of continuous output: type anyway, and let the
                                      # per-command wait below report what actually happened
  done
}

session_type() {
  local line="$1" want cur spun=0 before
  session_settle
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
  local turn=0 line rc answer="" bad=0 reached=""

  # A case may be reachable more than one way. The two columns are `|`-separated and INDEX-ALIGNED:
  # alternative i is proved by typing MT[i] AND seeing SAYS[i] on the live console. Checking the two
  # independently would let a session pass by typing one alternative and printing another's reply.
  local -a MT=() SAYS=() SEEN=()
  IFS='|' read -r -a MT <<< "$must_type"
  IFS='|' read -r -a SAYS <<< "$console_says"
  local i
  for i in "${!MT[@]}"; do SEEN[$i]=0; done

  echo "--> \"$request\""
  while [ "$turn" -lt "$DRIVER_MAX_TURNS" ]; do
    local -a argv=("$ALETHEIAD" console agent --transcript "$transcript"
                   --interpreter "$arm" --context-file "$brief")
    [ "$approved" = "true" ] && argv+=(--approve)
    [ "$turn" -gt 0 ] && argv+=(--observation-file "$obs")
    line="$("${argv[@]}" "$request" 2>"$err")"; rc=$?
    # Proposals Aletheia refused and re-asked. Nothing was typed for these, so they are shown
    # BEFORE the step they preceded -- a turn that quietly cost three model calls is a turn nobody
    # can debug from a log that only records the one that worked.
    sed -n 's/^corrected: /        ~ re-asked: /p' "$err"
    # How long the whole turn took, including the model calls that were corrected and never typed.
    # Collected so the summary can report a MEDIAN rather than a claim about speed.
    while read -r ms calls; do
      TURN_MS+=("$ms"); TURN_CALLS+=("$calls")
    done < <(sed -n 's/^turn-ms: \([0-9]*\) calls: \([0-9]*\)$/\1 \2/p' "$err")
    case "$rc" in
      0)
        [ -z "$line" ] && { echo "  FAIL [$label/$arm] exit 0 with no line to type"; bad=1; break; }
        echo "    step $((turn + 1)): $line"
        session_type "$line"
        typed+=("$line")
        for i in "${!SAYS[@]}"; do
          grep -qF "${SAYS[$i]}" <<<"$CAPTURED" && SEEN[$i]=1
        done
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
    # The claim: SOME dependent command really was typed at the machine, and the machine answered it.
    # Both halves of one alternative, or the alternative is not proved.
    reached=""
    for i in "${!MT[@]}"; do
      if printf '%s\n' "${typed[@]:-}" | grep -qxF "${MT[$i]}" && [ "${SEEN[$i]}" -eq 1 ]; then
        reached="${MT[$i]}"; break
      fi
    done
    if [ -z "$reached" ]; then
      echo "  FAIL [$label/$arm] no way of reaching the answer was both typed and confirmed by the console"
      for i in "${!MT[@]}"; do
        printf '    alternative: %-24s typed=%s console-said=%s\n' "${MT[$i]}" \
          "$(printf '%s\n' "${typed[@]:-}" | grep -qxF "${MT[$i]}" && echo yes || echo no)" \
          "$([ "${SEEN[$i]}" -eq 1 ] && echo yes || echo no)"
      done
      printf '    typed: %s\n' "${typed[@]:-<nothing>}"
      bad=1
    fi
    # And the answer's content — control arm only. See the header.
    if [ "$arm" = "deterministic" ] && ! grep -qF "$answer_contains" <<<"$answer"; then
      echo "  FAIL [$label/$arm] the answer does not contain: $answer_contains"
      bad=1
    fi
  fi
  [ "$bad" -eq 0 ] && echo "    OK — reached \`$reached\` and answered"
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

  # 1. Destructive with no pre-approval: the loop ASKS (ALET-P2-046 / ADR-059 on this surface).
  #    Stdout stays EMPTY until a human answers — if a line ever leaks to stdout here the driver
  #    types it, which is the one failure mode this whole contract exists to prevent. Every record
  #    lives in a SCRATCH data dir: grading governance in the operator's real ~/.aletheia would
  #    inherit yesterday's answers, and a gate that reads old answers is not a gate.
  t="$(mktemp -u)"; local adir obs; adir="$(mktemp -d)"; obs="$(mktemp)"
  session_type "write scratch temporary"
  grep -q "wrote 9 bytes to scratch" <<<"$CAPTURED" \
    || { echo "  FAIL [$label/$arm] fixture: scratch was not written"; bad=1; }
  out="$($ALETHEIAD console agent --transcript "$t" --interpreter "$arm" --data "$adir" \
        --context-file "$brief" "rm scratch" 2>"$adir/err")"; rc=$?
  local id1 id2 id3
  id1="$(sed -n 's/^approval required \[\([^]]*\)\].*/\1/p' < "$adir/err" | head -1)"
  if [ "$rc" -ne 7 ] || [ -n "$out" ] || [ -z "$id1" ]; then
    echo "  FAIL [$label/$arm] an unapproved destructive step did not ASK (rc=$rc out='$out')"
    sed 's/^/      | /' < "$adir/err"; bad=1
  fi
  $ALETHEIAD approvals list --data "$adir" | grep -q "^$id1 .*Pending.*rm scratch" \
    || { echo "  FAIL [$label/$arm] the pending question is not listed"; bad=1; }

  # A DENIAL is terminal for its record; asking again opens a NEW question, which is what gets
  # granted — and the grant binds EXACTLY 'rm scratch'.
  $ALETHEIAD approvals deny "$id1" --data "$adir" >/dev/null \
    || { echo "  FAIL [$label/$arm] deny failed"; bad=1; }
  out="$($ALETHEIAD console agent --transcript "$t" --interpreter "$arm" --data "$adir" \
        --context-file "$brief" "rm scratch" 2>"$adir/err")"; rc=$?
  id2="$(sed -n 's/^approval required \[\([^]]*\)\].*/\1/p' < "$adir/err" | head -1)"
  if [ "$rc" -ne 7 ] || [ -z "$id2" ] || [ "$id2" = "$id1" ]; then
    echo "  FAIL [$label/$arm] after denial expected a fresh ask, got rc=$rc id=$id2"; bad=1
  fi
  $ALETHEIAD approvals grant "$id2" --data "$adir" >/dev/null \
    || { echo "  FAIL [$label/$arm] grant failed"; bad=1; }

  # The granted line types EXACTLY once, the machine executes it, and the spend is recorded —
  # replaying or re-running cannot get a second typing out of one yes.
  out="$($ALETHEIAD console agent --transcript "$t" --interpreter "$arm" --data "$adir" \
        --context-file "$brief" "rm scratch" 2>/dev/null)"; rc=$?
  if [ "$rc" -ne 0 ] || [ "$out" != "rm scratch" ]; then
    echo "  FAIL [$label/$arm] a granted step did not type cleanly (rc=$rc out='$out')"; bad=1
  fi
  session_type "rm scratch"
  grep -q "removed scratch" <<<"$CAPTURED" \
    || { echo "  FAIL [$label/$arm] the machine did not execute the approved removal"; bad=1; }
  # Complete the spent turn BEFORE moving on: a line in flight without its observation is a
  # session that cannot legally take another step — skipping this hand-back once produced exactly
  # that driver bug on all three targets.
  capture_to_observation "$out" "$obs"
  # One yes bought one typing. Proved on a FRESH session asking for the same line: the spent grant
  # does not cover it, so the loop must ASK again (a finished session would only answer, which says
  # nothing about the store).
  local t2; t2="$(mktemp -u)"
  out="$($ALETHEIAD console agent --transcript "$t2" --interpreter "$arm" --data "$adir" \
        --context-file "$brief" "rm scratch" 2>"$adir/err")"; rc=$?
  if [ "$rc" -ne 7 ] || [ -n "$out" ]; then
    echo "  FAIL [$label/$arm] a consumed grant still typed (rc=$rc out='$out')"; bad=1
  fi
  $ALETHEIAD approvals list --data "$adir" | grep -q "^$id2 .*Consumed" \
    || { echo "  FAIL [$label/$arm] the spent grant is not recorded as Consumed"; bad=1; }
  rm -f "$t2"
  echo "    unapproved destructive step ASKS; denial records; grant types exactly once"
  rm -f "$t" "$obs"; rm -rf "$adir"

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
  TURN_MS=(); TURN_CALLS=()

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
  # What a turn cost, measured rather than claimed. `calls` is the interesting one: it is 1 when the
  # model wrote a valid command first time and more when it had to be corrected, so a rising median
  # here is the signal that a prompt change has made the model worse at writing commands.
  printf -- '--- %s/%s: %s turns, median %s ms/turn, %s model calls total\n' \
    "$label" "$arm" "${#TURN_MS[@]}" "$(median_of "${TURN_MS[@]:-}")" \
    "$(sum_of "${TURN_CALLS[@]:-0}")"
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

hr; echo "==> building the hosted agent"; hr
( cd "$ROOT/aletheia" && cargo build --release ) || { echo "FAIL: aletheiad did not build"; exit 1; }

# TWO disks per target: the console picks the SECOND virtio-blk device as its persistent medium and
# uses the first as scratch. Booted with one, every read-only case still passes and nothing survives
# a reboot -- which would make the reboot leg pass for the wrong reason.
SCRATCH=""; PERSIST=""
fresh_disks() {
  rm -f "$SCRATCH" "$PERSIST"
  dd if=/dev/zero of="$SCRATCH" bs=1048576 count=1 2>/dev/null
  dd if=/dev/zero of="$PERSIST" bs=1048576 count=1 2>/dev/null
}

# Both arms of one target, with a fresh medium for each. Split out because the three legs differ only
# in how a machine is started, and a leg that also had to remember to re-make its disks between arms
# is a leg where the second arm silently inherits the first arm's namespace.
run_target() {
  local label="$1"; shift
  local -a argv=("$@")
  fresh_disks
  run_arm "$label" deterministic "${argv[@]}"
  RESULTS+=("$label / deterministic : $([ $? -eq 0 ] && echo PASS || echo FAIL)")
  fresh_disks
  if model_available; then
    run_arm "$label" model "${argv[@]}"
    RESULTS+=("$label / model         : $([ $? -eq 0 ] && echo PASS || echo FAIL)")
  else
    RESULTS+=("$label / model         : SKIP (no backend is serving the selected model)")
    echo "SKIP: the model arm needs the selected model actually being served — see \`aletheiad model status\`"
  fi
}

# ---------------------------------------------------------------------------------------------
# aarch64 and RISC-V: a direct `-kernel` boot, serial on stdio, virtio over MMIO. The SAME dispatcher
# runs on both -- `kernel_core::shell` is shared -- which is exactly why running the model against
# only one of them proved nothing about the other (ALET-P2-047).
# ---------------------------------------------------------------------------------------------
mmio_target() {
  local label="$1" dir="$2" triple="$3" bin="$4"; shift 4
  local -a qemu=("$@")
  hr; echo "==> $label: building the kernel WITH the interactive console"; hr
  ( cd "$ROOT/$dir" && cargo build --features interactive ) \
    || { echo "FAIL: $label kernel did not build"; fail=1; RESULTS+=("$label : FAIL (build)"); return 1; }
  SCRATCH="$ROOT/$dir/target/console-agent-scratch.img"
  PERSIST="$ROOT/$dir/target/console-agent-persistent.img"
  PRE_OPEN=""
  run_target "$label" "${qemu[@]}" -kernel "$ROOT/$dir/target/$triple/debug/$bin" \
    -global virtio-mmio.force-legacy=false \
    -drive "if=none,format=raw,file=$SCRATCH,id=blk0" -device virtio-blk-device,drive=blk0 \
    -drive "if=none,format=raw,file=$PERSIST,id=blk1" -device virtio-blk-device,drive=blk1
}

mmio_target "aarch64" kernel aarch64-unknown-none-softfloat aletheia-kernel \
  qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 128M -nographic \
  -semihosting-config enable=on,target=native

mmio_target "riscv64" kernel-riscv64 riscv64gc-unknown-none-elf aletheia-kernel-riscv64 \
  qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -nographic -bios default

# ---------------------------------------------------------------------------------------------
# x86-64: a UEFI disk image under OVMF, virtio over PCI. SKIPs -- never silently passes -- when the
# host has no firmware to boot it with.
# ---------------------------------------------------------------------------------------------
x86_target() {
  local label="x86-64" code="" vars=""
  for c in "${OVMF_CODE:-}" /opt/homebrew/share/qemu/edk2-x86_64-code.fd \
           /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd \
           /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
    [ -n "$c" ] && [ -f "$c" ] && { code="$c"; break; }
  done
  for v in "${OVMF_VARS:-}" /opt/homebrew/share/qemu/edk2-i386-vars.fd \
           /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd \
           /usr/share/edk2/x64/OVMF_VARS.4m.fd; do
    [ -n "$v" ] && [ -f "$v" ] && { vars="$v"; break; }
  done
  if ! command -v qemu-system-x86_64 >/dev/null 2>&1 || ! command -v mformat >/dev/null 2>&1 \
     || [ -z "$code" ] || [ -z "$vars" ]; then
    echo "  x86-64 agent leg unavailable on this host (needs qemu-system-x86_64 + mtools + OVMF) — SKIPPED (never a silent pass)."
    RESULTS+=("x86-64 / deterministic : SKIP (no OVMF/mtools on this host)")
    RESULTS+=("x86-64 / model         : SKIP (no OVMF/mtools on this host)")
    return 0
  fi

  hr; echo "==> $label: building the UEFI image WITH the interactive console"; hr
  local img="$ROOT/kernel-x86_64/build/aletheia-x86_64-interactive.img"
  CARGO_FEATURES=interactive IMG="$img" bash "$ROOT/kernel-x86_64/scripts/build-image-linux.sh" \
    || { echo "FAIL: $label image did not build"; fail=1; RESULTS+=("x86-64 : FAIL (build)"); return 1; }

  local work; work="$(mktemp -d)"
  SCRATCH="$work/scratch.img"
  PERSIST="$work/persist.img"
  # OVMF WRITES its NVRAM. Every boot gets a fresh copy of the template; the DISKS are kept, which
  # is what makes the reboot leg a real power cycle of the same machine rather than a new one.
  OVMF_VARS_TEMPLATE="$vars" OVMF_VARS_LIVE="$work/vars.fd"
  refresh_ovmf_vars() { cp "$OVMF_VARS_TEMPLATE" "$OVMF_VARS_LIVE"; }
  PRE_OPEN=refresh_ovmf_vars

  run_target "$label" qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep -nographic \
    -drive "if=pflash,format=raw,unit=0,file=$code,readonly=on" \
    -drive "if=pflash,format=raw,unit=1,file=$work/vars.fd" \
    -drive "format=raw,file=$img" \
    -drive "if=none,format=raw,file=$SCRATCH,id=blk0" -device virtio-blk-pci,drive=blk0 \
    -drive "if=none,format=raw,file=$PERSIST,id=blk1" -device virtio-blk-pci,drive=blk1 \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 -no-reboot
  PRE_OPEN=""
  rm -rf "$work"
}

x86_target

hr; printf '%s\n' "${RESULTS[@]}"; hr
[ "$fail" -eq 0 ] && echo "console-agent-e2e: PASS" || echo "console-agent-e2e: FAIL"
exit "$fail"
