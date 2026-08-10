#!/usr/bin/env bash
# The model drives the console, end to end (REQ-AI-006, ADR-053).
#
# `scripts/console-e2e.sh` proves the console behaves when a scripted operator types at it.
# `aletheiad console bench` proves the model can turn a request into the right command. Neither
# proves the two halves join up, and a pipeline that is only ever tested in halves is a pipeline with
# an untested seam in the middle of it.
#
# So this gate closes the loop, once per case, against a booted machine:
#
#   1. the guest boots with `--features interactive` and is given a fixture through the console
#   2. the driver types `ls` and captures what came back — that is the CONTEXT BRIEF, read off the
#      live machine rather than assumed
#   3. `aletheiad console plan` is asked, in plain English, for the thing the case wants; the model
#      chooses a command, Aletheia validates it against the kernel's own table and renders ONE line
#   4. the driver types that line at the guest and asserts what the console printed
#   5. after a case that changes the namespace, the brief is re-read — the machine moved
#
# Two arms, exactly like the benchmark. The DETERMINISTIC arm needs no model and must pass
# everywhere, so a machine with no inference engine still gates the whole pipe. The MODEL arm runs
# only when a backend really is serving the selected model, and SKIPs — never silently passes — when
# it is not.
#
# What this does NOT claim: there is still no inference engine in kernel space. `kernel-core` remains
# `no_std` with no network and no model. Every model call happens on the host, and what crosses into
# the guest is a validated line of ASCII, indistinguishable from one a person typed.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

ALETHEIAD="$ROOT/aletheia/target/release/aletheiad"
BODY="the OS you can sit in front of"      # 30 bytes, matching console-e2e.sh
POEM="hello world!"                        # 12 bytes

fail=0
declare -a RESULTS=()
hr() { printf '========================================================================\n'; }

# ---------------------------------------------------------------------------------------------
# One long-lived console session the driver can hold a conversation with.
#
# console-e2e.sh types a fixed list and asserts the whole transcript at the end. This gate cannot:
# what it types NEXT depends on what the machine just said, because the brief is read off the guest
# and handed to a model. So the session is opened once and each line is typed, awaited and captured
# individually.
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
  # $@ = the QEMU argv
  SESSION_LOG="$(mktemp)"
  SESSION_FIFO="$(mktemp -u)"
  mkfifo "$SESSION_FIFO"
  : > "$SESSION_LOG"
  exec 3<>"$SESSION_FIFO"
  "$@" < "$SESSION_FIFO" > "$SESSION_LOG" 2>&1 &
  SESSION_PID=$!
  ( sleep 300; kill -9 "$SESSION_PID" 2>/dev/null ) &
  SESSION_WATCHDOG=$!
  local waited=0
  while ! grep -q "aletheia> " "$SESSION_LOG" 2>/dev/null; do
    kill -0 "$SESSION_PID" 2>/dev/null || return 1
    sleep 1; waited=$((waited + 1))
    [ "$waited" -ge 180 ] && return 1
  done
  return 0
}

# Type one line and capture everything the console printed in reply. Sets CAPTURED.
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

# The context brief: what `ls` says, right now, framed for the model as the namespace it is planning
# against. Read from the guest — never assumed — because a brief that describes a machine other than
# the one being driven is worse than no brief at all.
refresh_brief() {
  local brief_file="$1"
  session_type "ls"
  {
    printf '  objects on this machine:\n'
    # Drop the echoed command, the prompt and any blank line; what is left is the listing.
    # The carriage returns come first. A serial line ends every line with CR-LF, so the echoed
    # command arrives as `ls\r` and a `/^ls$/d` written without stripping it deletes nothing — the
    # brief then tells the model there is an object called `ls`, which is a lie about the machine
    # in the one place the model is trusting the machine.
    printf '%s\n' "$CAPTURED" \
      | tr -d '\r' \
      | sed -e 's/aletheia> //g' -e '/^ls$/d' -e '/^[[:space:]]*$/d' \
      | sed -e 's/^/    /'
  } > "$brief_file"
}

# ---------------------------------------------------------------------------------------------
# The gate proper: one arm (deterministic or model) against one booted target.
# ---------------------------------------------------------------------------------------------
run_arm() {
  # $1 = target label, $2 = arm (deterministic|model), then the QEMU argv
  local label="$1" arm="$2"; shift 2
  local bad=0
  hr; echo "==> $label / $arm arm"; hr

  if ! session_open "$@"; then
    echo "  FAIL [$label/$arm] the machine never reached a prompt"
    session_close; return 1
  fi

  # The fixture. Typed by the DRIVER, not planned by the model: a gate whose setup depends on the
  # thing under test cannot report which of the two broke.
  session_type "write manifesto $BODY"
  grep -q "wrote 30 bytes to manifesto" <<<"$CAPTURED" \
    || { echo "  FAIL [$label/$arm] fixture: manifesto was not written"; bad=1; }
  session_type "write poem $POEM"
  grep -q "wrote 12 bytes to poem" <<<"$CAPTURED" \
    || { echo "  FAIL [$label/$arm] fixture: poem was not written"; bad=1; }

  local brief; brief="$(mktemp)"
  refresh_brief "$brief"
  grep -q "manifesto" "$brief" || { echo "  FAIL [$label/$arm] the brief did not see the namespace"; bad=1; }
  echo "--- context brief read off the live machine:"; sed 's/^/    /' "$brief"

  # Every case, from the SAME table the benchmark uses.
  local natural literal expect approved says request planned rc
  while IFS=$'\t' read -r natural literal expect approved says; do
    [ -z "${natural:-}" ] && continue
    if [ "$arm" = "model" ]; then request="$natural"; else request="$literal"; fi
    local -a plan_argv=("$ALETHEIAD" console plan --interpreter "$arm" --context-file "$brief")
    [ "$approved" = "true" ] && plan_argv+=(--approve)
    planned="$("${plan_argv[@]}" "$request" 2>/dev/null)"; rc=$?
    if [ "$rc" -ne 0 ] || [ -z "$planned" ]; then
      echo "  FAIL [$label/$arm] no plan for: $request"; bad=1; continue
    fi
    echo "--> \"$request\""
    echo "    planned: $planned"
    # A plan is one or more lines; type each and assert against the LAST reply, which is the one the
    # case describes.
    while IFS= read -r cmd; do
      [ -z "$cmd" ] && continue
      session_type "$cmd"
    done <<<"$planned"
    if grep -qF "$says" <<<"$CAPTURED"; then
      echo "    console said: $says   OK"
    else
      echo "  FAIL [$label/$arm] the console never printed: $says"
      printf '%s\n' "$CAPTURED" | sed 's/^/      | /'
      bad=1
    fi
    # A case that changed the namespace invalidates the brief. Re-read it rather than carry a
    # description of a machine that has moved on — which is exactly the defect that made the
    # benchmark plan `find notes` for "remove notes".
    case "$planned" in
      write*|rm*|mv*|cp*|touch*|append*) refresh_brief "$brief" ;;
    esac
  done < <("$ALETHEIAD" console cases)

  # The negative path, at the CLI, not only in a unit test: a destructive request with no approval
  # must refuse BEFORE anything is rendered, and must print no console line at all. If this ever
  # passes silently, every other row above is worthless.
  local refused rc2
  refused="$("$ALETHEIAD" console plan --interpreter "$arm" --context-file "$brief" "rm manifesto" 2>&1 >/dev/null)"; rc2=$?
  local emitted; emitted="$("$ALETHEIAD" console plan --interpreter "$arm" --context-file "$brief" "rm manifesto" 2>/dev/null)"
  if [ "$rc2" -eq 0 ] || [ -n "$emitted" ]; then
    echo "  FAIL [$label/$arm] a destructive plan was rendered without approval"; bad=1
  else
    echo "--> unapproved destructive request refused: $(printf '%s' "$refused" | tail -1)"
  fi
  # And the object it would have destroyed is still there.
  session_type "cat manifesto"
  grep -qF "$BODY" <<<"$CAPTURED" \
    || { echo "  FAIL [$label/$arm] manifesto did not survive the refused plan"; bad=1; }

  session_close

  # The reboot leg. `console-e2e.sh` proves a typed write survives a power cycle; a MODEL-planned
  # write has to prove the same thing, or "the model wrote an object" means only "the model wrote an
  # object into RAM". The machine comes back on the same disk and is asked for the fixture.
  if ! session_open "$@"; then
    echo "  FAIL [$label/$arm] the machine did not come back for the reboot leg"; bad=1
  else
    grep -q "persistent virtio-blk device" "$SESSION_LOG" \
      || { echo "  FAIL [$label/$arm] the second boot did not mount the persistent device"; bad=1; }
    session_type "cat manifesto"
    grep -qF "$BODY" <<<"$CAPTURED" \
      || { echo "  FAIL [$label/$arm] what the console wrote did NOT survive the reboot"; bad=1; }
    # And what the model REMOVED is still gone: a delete that a reboot undoes is not a delete.
    session_type "cat notes"
    grep -q "no such object" <<<"$CAPTURED" \
      || { echo "  FAIL [$label/$arm] the removed object came back after the reboot"; bad=1; }
    echo "--> after a power cycle: the fixture is intact and the removed object is still gone"
    session_close
  fi

  rm -f "$brief" "$SESSION_LOG"
  [ "$bad" -eq 0 ] || fail=1
  return $bad
}

# ---------------------------------------------------------------------------------------------
# Is a model really there? Asked once, and answered by the same identity check the benchmark uses:
# the endpoint is a port, and any process can hold a port.
# ---------------------------------------------------------------------------------------------
model_available() {
  "$ALETHEIAD" console bench >/dev/null 2>&1
  local rc=$?
  # 2 = refused before measuring (nothing serving, or serving something else). 0 or 1 both mean a
  # model answered; whether it scored perfectly is the benchmark's business, not this gate's.
  [ "$rc" -ne 2 ]
}

hr; echo "==> building the hosted planner and the interactive kernel"; hr
( cd "$ROOT/aletheia" && cargo build --release ) || { echo "FAIL: aletheiad did not build"; exit 1; }
( cd "$ROOT/kernel" && cargo build --features interactive ) || { echo "FAIL: kernel did not build"; exit 1; }

# TWO disks, exactly as console-e2e.sh provisions them, and not as an accident of copying: the
# console picks the SECOND virtio-blk device as its persistent medium and uses the first as scratch.
# Booted with one disk the console still works and every read-only case still passes — and nothing
# survives a reboot, which is how the first run of this gate failed. A persistence claim needs the
# disk the persistence is on.
SCRATCH="$ROOT/kernel/target/console-ai-scratch.img"
PERSIST="$ROOT/kernel/target/console-ai-persistent.img"
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
[ "$fail" -eq 0 ] && echo "console-ai-e2e: PASS" || echo "console-ai-e2e: FAIL"
exit "$fail"
