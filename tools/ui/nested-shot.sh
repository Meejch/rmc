#!/usr/bin/env bash
# Screenshot of a Wayland program in a nested, invisible KWin (virtual backend, own D-Bus):
# nothing appears on the real desktop.
#
#   tools/ui/nested-shot.sh <out.png> <seconds> [WxH] -- <command...>
#
# The command runs with WAYLAND_DISPLAY of the nested KWin; after <seconds> spectacle takes
# a full screenshot. Everything started here is ended afterwards.
set -euo pipefail
OUT=$(realpath -m "$1"); SECS=$2; shift 2
SIZE=1600x900
if [[ "${1:-}" != "--" ]]; then SIZE=$1; shift; fi
[[ "${1:-}" == "--" ]] && shift
W=${SIZE%x*}; H=${SIZE#*x}
rm -f "$OUT"
LOG=${NESTED_LOG:-/dev/null}
SOCK=rmc-nested-$$
inner() {
	kwin_wayland --virtual --no-lockscreen --width "$W" --height "$H" --socket "$SOCK" >>"$LOG" 2>&1 &
	for _ in $(seq 100); do [[ -S "$XDG_RUNTIME_DIR/$SOCK" ]] && break; sleep 0.1; done
	WAYLAND_DISPLAY=$SOCK "$@" >>"$LOG" 2>&1 &
	sleep "$SECS"
	WAYLAND_DISPLAY=$SOCK timeout 15 spectacle -b -n -f -o "$OUT" >>"$LOG" 2>&1 || true
}
export -f inner
export W H SOCK OUT SECS LOG
setsid bash -c 'dbus-run-session -- bash -c "inner \"\$@\"" _ "$@" 2>>"$LOG"' _ "$@" &
PG=$!
for _ in $(seq $((SECS + 25))); do
	[[ -s "$OUT" ]] && break
	sleep 1
done
sleep 1
kill -- -"$PG" 2>/dev/null || true
sleep 1
kill -9 -- -"$PG" 2>/dev/null || true
[[ -s "$OUT" ]] && echo "$OUT"
