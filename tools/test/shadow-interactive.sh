#!/usr/bin/env bash
# Joint live check of the client against the local shadow server: Xvfb :77 with the test
# desktop and the X11 input probe, freerdp-shadow-cli (NLA, random test password),
# rmc in fullscreen (or --test-window). At the end the probe log shows which keys,
# buttons and cursor changes reached the server.
#
#   tools/test/shadow-interactive.sh [max seconds] [extra rmc args...]
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=${FREERDP_PREFIX:-$HOME/.local/opt/freerdp-rmc}
SECS=${1:-300}
shift $(( $# > 0 ? 1 : 0 )) || true
W=${W:-3440}; H=${H:-1440}; DISP=${DISP:-:77}; PORT=${PORT:-3391}
TMP=$(mktemp -d); PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; }
trap cleanup EXIT INT TERM
cd "$REPO"
cargo build -q --release -p rmc -p rdpgfx-fixtures
mkdir -p target/xprobe && gcc -O2 -o target/xprobe/xprobe tools/xprobe/xprobe.c -lX11
Xvfb "$DISP" -screen 0 "${W}x${H}x24" -nolisten tcp >/dev/null 2>&1 & PIDS+=($!)
sleep 1
target/release/gen-fixtures still "$TMP/source.ppm" --width "$W" --height "$H" >/dev/null
DISPLAY="$DISP" target/xprobe/xprobe "$TMP/source.ppm" >"$TMP/probe.log" 2>&1 & PIDS+=($!)
sleep 1
PW=$(head -c 24 /dev/urandom | base64 | tr -d '/+=' | head -c 20)
"$PREFIX/bin/winpr-hash" -u rdptest -p "$PW" -f sam >"$TMP/sam"
DISPLAY="$DISP" WAYLAND_DISPLAY= LD_LIBRARY_PATH="$PREFIX/lib64" "$PREFIX/bin/freerdp-shadow-cli" \
	/bind-address:127.0.0.1 "/port:$PORT" /sec:nla "/sam-file:$TMP/sam" +server-side-cursor \
	>"$TMP/shadow.log" 2>&1 & PIDS+=($!)
sleep 2
set +e
RDP_PASSWORD="$PW" target/release/rmc --host 127.0.0.1 --port "$PORT" --user rdptest \
	--ignore-cert --duration "$SECS" "$@" 2>&1 | grep -vE "^\[.*\] \[(WARN|ERROR)\]\[com\.(winpr\.sspi|freerdp\.crypto|freerdp\.core\.rdp)\]"
set -e
echo
echo "== Auf dem Server angekommen (X11-Sonde):"
grep -E "^KEY press" "$TMP/probe.log" | sed -E 's/.*keysym=//' | tr '\n' ' ' | fold -w 100; echo
echo "Tasten gedrückt: $(grep -c '^KEY press' "$TMP/probe.log"), Klicks: $(grep -c '^BUTTON press' "$TMP/probe.log"), Cursorwechsel: $(grep -c '^CURSOR' "$TMP/probe.log")"
echo "Alt+Tab angekommen: $(grep -E 'keysym=(Alt_L|Tab)$' "$TMP/probe.log" | grep -c Tab)x Tab, Meta: $(grep -cE 'keysym=(Super_L|Super_R|Meta_L)$' "$TMP/probe.log")x"
cp "$TMP/probe.log" "$REPO/local/shadow-interactive-probe.log" 2>/dev/null || true
rm -rf "$TMP"
