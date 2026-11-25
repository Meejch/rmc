#!/usr/bin/env bash
# The client against the local shadow server, shown in a nested invisible KWin, screenshot
# after <seconds> (tools/ui/nested-shot.sh). For checking the bar and overlays.
#
#   tools/ui/shadow-shot.sh <out.png> <seconds> [rmc args...]
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=${FREERDP_PREFIX:-$HOME/.local/opt/freerdp-rmc}
OUT=$1; SECS=$2; shift 2
DISP=${DISP:-:79}; PORT=${PORT:-3393}
TMP=$(mktemp -d); PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; rm -rf "$TMP"; }
trap cleanup EXIT INT TERM
cd "$REPO"
Xvfb "$DISP" -screen 0 1600x896x24 -nolisten tcp >/dev/null 2>&1 & PIDS+=($!)
sleep 1
target/release/gen-fixtures still "$TMP/source.ppm" --width 1600 --height 896 >/dev/null
DISPLAY="$DISP" target/xprobe/xprobe "$TMP/source.ppm" >/dev/null 2>&1 & PIDS+=($!)
PW=$(head -c 24 /dev/urandom | base64 | tr -d '/+=' | head -c 20)
"$PREFIX/bin/winpr-hash" -u rdptest -p "$PW" -f sam >"$TMP/sam"
DISPLAY="$DISP" WAYLAND_DISPLAY= LD_LIBRARY_PATH="$PREFIX/lib64" "$PREFIX/bin/freerdp-shadow-cli" \
	/bind-address:127.0.0.1 "/port:$PORT" /sec:nla "/sam-file:$TMP/sam" >/dev/null 2>&1 & PIDS+=($!)
sleep 2
RDP_PASSWORD="$PW" NESTED_LOG=${NESTED_LOG:-/dev/null} tools/ui/nested-shot.sh "$OUT" "$SECS" -- \
	target/release/rmc --host 127.0.0.1 --port "$PORT" --user rdptest --ignore-cert \
	--windowed --duration $((SECS + 20)) "$@"
