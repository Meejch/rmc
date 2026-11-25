#!/usr/bin/env bash
# Sustained motion against the local shadow server (Xvfb with a moving test picture,
# NLA with a random test password). The client runs headless or in the test window
# (app id rdp-replay-test: no focus; a KWin window rule can place it on a spare monitor).
#
#   tools/test/shadow-motion.sh [mode] [seconds] [extra rmc args...]
# modes: headless | window (own client), sdl-sw | sdl-cuda | xfreerdp-sw | xfreerdp-cuda
# (FreeRDP's clients from tools/freerdp on an invisible Xvfb :78; CPU of the client process
# measured over the same time).
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=${FREERDP_PREFIX:-$HOME/.local/opt/freerdp-rmc}
MODE=${1:-headless}
SECS=${2:-20}
shift $(( $# > 2 ? 2 : $# )) || true
W=${W:-3440}; H=${H:-1440}; DISP=${DISP:-:77}; PORT=${PORT:-3391}
TMP=$(mktemp -d); PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; rm -rf "$TMP"; }
trap cleanup EXIT INT TERM
cd "$REPO"
cargo build -q --release -p rmc
Xvfb "$DISP" -screen 0 "${W}x${H}x24" -nolisten tcp >/dev/null 2>&1 & PIDS+=($!)
sleep 1
DISPLAY="$DISP" SDL_VIDEODRIVER=x11 WAYLAND_DISPLAY= ffplay -loglevel error -fs -an -f lavfi \
	-i "testsrc2=size=${W}x${H}:rate=60" & PIDS+=($!)
sleep 2
PW=$(head -c 24 /dev/urandom | base64 | tr -d '/+=' | head -c 20)
"$PREFIX/bin/winpr-hash" -u rdptest -p "$PW" -f sam >"$TMP/sam"
DISPLAY="$DISP" WAYLAND_DISPLAY= LD_LIBRARY_PATH="$PREFIX/lib64" "$PREFIX/bin/freerdp-shadow-cli" \
	/bind-address:127.0.0.1 "/port:$PORT" /sec:nla "/sam-file:$TMP/sam" +server-side-cursor >"$TMP/shadow.log" 2>&1 & PIDS+=($!)
sleep 2
cpu_ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }

freerdp_client() { # binary decoder
	local bin=$1 dec=$2
	local json='{ "useHwDecoder": false }'
	[ "$dec" = cuda ] && json='{ "useHwDecoder": true, "hwDeviceType": "cuda", "hwDeviceName": "0" }'
	# The config directory depends on the client's vendor/product strings; cover all of them.
	for d in freerdp FreeRDP FreeRDP3 xfreerdp sdl-freerdp SDL3-Client; do
		mkdir -p "$TMP/config/$d"
		echo "$json" >"$TMP/config/$d/ffmpeg.json"
	done
	Xvfb :78 -screen 0 "${W}x${H}x24" -nolisten tcp >/dev/null 2>&1 & PIDS+=($!)
	sleep 1
	DISPLAY=:78 WAYLAND_DISPLAY= SDL_VIDEODRIVER=x11 XDG_CONFIG_HOME="$TMP/config" WLOG_LEVEL=${WLOG_LEVEL:-WARN} \
		LD_LIBRARY_PATH="$PREFIX/lib64" "$PREFIX/bin/$bin" "/v:127.0.0.1:$PORT" /u:rdptest "/p:$PW" \
		/cert:ignore /gfx:AVC444 /gdi:hw /network:lan /f >"$TMP/client.log" 2>&1 &
	local pid=$!
	PIDS+=($pid)
	sleep 4
	local t0 c0 t1 c1
	t0=$(date +%s.%N); c0=$(cpu_ticks $pid)
	sleep "$SECS"
	t1=$(date +%s.%N); c1=$(cpu_ticks $pid)
	grep -m1 -iE "accelerated H264|Using (cuda|vaapi)" "$TMP/client.log" | sed 's/^.*\] - //' || true
	[ -n "${KEEP_LOG:-}" ] && cp "$TMP/client.log" "$KEEP_LOG"
	awk -v c0="$c0" -v c1="$c1" -v t0="$t0" -v t1="$t1" -v tck="$(getconf CLK_TCK)" \
		'BEGIN{printf "%s (%s): CPU %.1f %% eines Kerns über %.0f s\n", "'"$bin"'", "'"$dec"'", (c1-c0)/tck/(t1-t0)*100, t1-t0}'
	kill -TERM "$pid" 2>/dev/null || true
}

ARGS=(--host 127.0.0.1 --port "$PORT" --user rdptest --ignore-cert "--size=${W}x${H}" --duration "$SECS")
case "$MODE" in
headless) ARGS+=(--headless) ;;
window) ARGS+=(--test-window) ;;
sdl-sw) freerdp_client sdl-freerdp sw; exit 0 ;;
sdl-cuda) freerdp_client sdl-freerdp cuda; exit 0 ;;
xfreerdp-sw) freerdp_client xfreerdp sw; exit 0 ;;
xfreerdp-cuda) freerdp_client xfreerdp cuda; exit 0 ;;
*) echo "mode: headless|window|sdl-sw|sdl-cuda|xfreerdp-sw|xfreerdp-cuda" >&2; exit 2 ;;
esac
RDP_PASSWORD="$PW" target/release/rmc "${ARGS[@]}" "$@"
