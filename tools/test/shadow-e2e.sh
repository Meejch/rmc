#!/usr/bin/env bash
# End-to-end test against the local FreeRDP shadow server (invisible, Xvfb only):
#   server: Xvfb :77 with the synthetic desktop shown by xprobe (logs all input it gets),
#           freerdp-shadow-cli with NLA and a SAM file (test account, random password).
#   client: rmc --headless (libfreerdp session -> GPU core), password auth.
# Checks: wrong password is rejected; AVC444 and AVC420 sessions decode, frames are
# acknowledged after the present; the client picture matches the server screen; mouse and
# keyboard input arrive on the server with the right coordinates and keys.
#
#   tools/test/shadow-e2e.sh [output dir]      (default: target/e2e-<date>)
set -euo pipefail

REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=${FREERDP_PREFIX:-$HOME/.local/opt/freerdp-rmc}
W=${W:-3440}
H=${H:-1440}
DISP=${DISP:-:77}
PORT=${PORT:-3391}
OUT=${1:-$REPO/target/e2e-$(date +%F)}
TMP=$(mktemp -d)
PIDS=()

cleanup() {
	for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
	wait 2>/dev/null || true
	rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

mkdir -p "$OUT"
cd "$REPO"
cargo build -q --release -p rmc -p rdpgfx-fixtures
mkdir -p target/xprobe
gcc -O2 -Wall -o target/xprobe/xprobe tools/xprobe/xprobe.c -lX11

Xvfb "$DISP" -screen 0 "${W}x${H}x24" -nolisten tcp >/dev/null 2>&1 &
PIDS+=($!)
sleep 1
target/release/gen-fixtures still "$TMP/source.ppm" --width "$W" --height "$H" >/dev/null
DISPLAY="$DISP" target/xprobe/xprobe "$TMP/source.ppm" >"$OUT/probe.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 50); do grep -q READY "$OUT/probe.log" && break; sleep 0.1; done
grep -q READY "$OUT/probe.log" || { echo "xprobe did not start" >&2; exit 1; }

# Test account for this run only; the password never touches the repository.
PW=$(head -c 24 /dev/urandom | base64 | tr -d '/+=' | head -c 20)
"$PREFIX/bin/winpr-hash" -u rdptest -p "$PW" -f sam >"$TMP/sam"
DISPLAY="$DISP" WAYLAND_DISPLAY= LD_LIBRARY_PATH="$PREFIX/lib64" \
	"$PREFIX/bin/freerdp-shadow-cli" /bind-address:127.0.0.1 "/port:$PORT" /sec:nla \
	"/sam-file:$TMP/sam" +server-side-cursor >"$OUT/shadow.log" 2>&1 &
PIDS+=($!)
sleep 2

# RMC_E2E_DECODER=nvidia|vaapi|software checks one decoder backend (default: auto)
# RMC_E2E_BIN=<path> tests another build, e.g. the AppImage
CLIENT=("${RMC_E2E_BIN:-target/release/rmc}" --host 127.0.0.1 --port "$PORT" --user rdptest --ignore-cert --headless "--size=${W}x${H}" --decoder "${RMC_E2E_DECODER:-auto}")

echo "== 1. falsches Passwort"
set +e
RDP_PASSWORD="falsch-$RANDOM" "${CLIENT[@]}" --duration 8 >"$OUT/client-wrong-password.txt" 2>&1
rc_wrong=$?
set -e
grep -E "Getrennt|Fehler|Error" "$OUT/client-wrong-password.txt" | head -3

echo "== 2. AVC444, Eingabetest, Surface-Ablage"
WLOG_FILTER="com.freerdp.channels.rdpsnd.client:DEBUG,com.freerdp.channels.audin.client:DEBUG,com.freerdp.channels.rdpecam-enum.client:INFO" \
	RDP_PASSWORD="$PW" "${CLIENT[@]}" --duration 20 --input-test --dump-dir "$OUT/avc444" \
	>"$OUT/client-avc444.txt" 2>&1 || true
sleep 1
ffmpeg -hide_banner -loglevel error -y -f x11grab -video_size "${W}x${H}" -i "$DISP" -frames:v 1 "$OUT/server.png"

grab() { ffmpeg -hide_banner -loglevel error -y -f x11grab -video_size "${W}x${H}" -i "$1" -frames:v 1 "$2"; }

echo "== 3. AVC420"
RDP_PASSWORD="$PW" "${CLIENT[@]}" --duration 10 --avc420-only --dump-dir "$OUT/avc420" \
	>"$OUT/client-avc420.txt" 2>&1 || true

# From here on the server screen no longer changes: own client and FreeRDP's reference
# client see the same picture. (The first connection after the server start gets more
# refinement frames and a better picture; all compared runs below are later connections.)
echo "== 4. AVC444 ohne Eingabe (Vergleich mit FreeRDP)"
RDP_PASSWORD="$PW" "${CLIENT[@]}" --duration 10 --dump-dir "$OUT/avc444-still" \
	>"$OUT/client-avc444-still.txt" 2>&1 || true
RDP_PASSWORD="$PW" "${CLIENT[@]}" --duration 10 --no-reverse-filter --dump-dir "$OUT/avc444-still-nofilter" \
	>"$OUT/client-avc444-still-nofilter.txt" 2>&1 || true
sleep 1
grab "$DISP" "$OUT/server-still.png"

echo "== 5. Referenz: FreeRDP sdl-freerdp, AVC444, Software-Decode"
RDISP=:78
Xvfb "$RDISP" -screen 0 "${W}x${H}x24" -nolisten tcp >/dev/null 2>&1 &
PIDS+=($!)
sleep 1
DISPLAY="$RDISP" WAYLAND_DISPLAY= SDL_VIDEODRIVER=x11 LD_LIBRARY_PATH="$PREFIX/lib64" \
	"$PREFIX/bin/sdl-freerdp" "/v:127.0.0.1:$PORT" /u:rdptest "/p:$PW" /cert:ignore /gfx:AVC444 /f \
	>"$OUT/freerdp-reference.txt" 2>&1 &
REF=$!
sleep 10
grab "$RDISP" "$OUT/freerdp-reference.png"
kill -TERM "$REF" 2>/dev/null || true
wait "$REF" 2>/dev/null || true

compare() { # client.ppm server.png
	ffmpeg -hide_banner -nostats -i "$1" -i "$2" -lavfi "[0][1]psnr;[0][1]ssim" -f null - 2>&1 |
		grep -oE "(PSNR|SSIM).*" | sed -E 's/ *\(.*//' | tr '\n' ' '
}

expect() { # pattern description
	if grep -qE "$1" "$OUT/probe.log"; then echo "  ok      $2"; else echo "  FEHLT   $2"; fail=1; fi
}

{
	echo "End-to-End gegen Shadow-Server, $(date '+%F %T')"
	echo "Server: Xvfb $DISP ${W}x${H}, freerdp-shadow-cli NLA + SAM (Testkonto rdptest), Port $PORT"
	echo
	echo "1. Falsches Passwort: Exit $rc_wrong, $(grep -m1 -E 'Getrennt|Verbindung beendet' "$OUT/client-wrong-password.txt" || echo 'keine Meldung')"
	for run in avc444 avc420 avc444-still; do
		f="$OUT/client-$run.txt"
		echo
		echo "== $run"
		grep -E "Verbunden|GFX-Caps|ResetGraphics|CreateSurface|MapSurface|Frames dekodiert|Nicht unterstützt|Laufzeit|Fehler" "$f" | head -12 || true
		grep -E "^  (AVC|CPU-Codec|SolidFill|SurfaceTo|CacheTo|EndFrame)" "$f" || true
		grep -E "quittiert \(" "$f" | tail -1 || true
	done
	echo
	surf() { ls "$1"/surface-*.ppm | head -1; }
	echo "Bildvergleich gegen den Server-Bildschirm:"
	echo "  eigener Client AVC444 (nach Eingabetest): $(compare "$(surf "$OUT/avc444")" "$OUT/server.png")"
	echo "  eigener Client AVC420:                    $(compare "$(surf "$OUT/avc420")" "$OUT/server-still.png")"
	echo "  eigener Client AVC444 (Standbild):        $(compare "$(surf "$OUT/avc444-still")" "$OUT/server-still.png")"
	echo "  eigener Client AVC444 ohne Rückfilter:    $(compare "$(surf "$OUT/avc444-still-nofilter")" "$OUT/server-still.png")"
	echo "  FreeRDP sdl-freerdp AVC444 (Standbild):   $(compare "$OUT/freerdp-reference.png" "$OUT/server-still.png")"
	echo
	echo "Eingabe (Protokoll der X11-Sonde auf dem Server):"
	fail=0
	expect "BUTTON press 1 at 100,100" "Linksklick bei 100,100"
	expect "BUTTON press 3 at $((W - 440)),$((H - 140))" "Rechtsklick bei $((W - 440)),$((H - 140))"
	expect "BUTTON press 2 at $((W / 2)),$((H / 2))" "Mittelklick bei $((W / 2)),$((H / 2))"
	expect "BUTTON press 4 " "Mausrad hoch (Taste 4)"
	expect "BUTTON press 5 " "Mausrad runter (Taste 5)"
	expect "KEY press .*keysym=r$" "Taste r"
	expect "KEY press .*keysym=d$" "Taste d"
	expect "KEY press .*keysym=p$" "Taste p"
	expect "KEY press .*keysym=A$" "Shift+a = A"
	expect "KEY press .*keysym=Return$" "Enter"
	expect "KEY press .*keysym=Right$" "Pfeil rechts (extended)"
	expect "KEY press .*keysym=KP_(Right|6)$" "Ziffernblock 6 (nicht extended)"
	if grep -q "Zeigerform" "$OUT/client-avc444.txt"; then
		echo "  ok      Zeigerform vom Server empfangen ($(grep -m1 -o 'Zeigerform [0-9]*: [0-9x]*, Hotspot [0-9,]*' "$OUT/client-avc444.txt"))"
		# The probe's cursors: a cross (row and column 7 bright) or a square (bright frame).
		f="$OUT/client-avc444.txt"
		if { grep -qE '^  \|#{16}\|$' "$f" && [ "$(grep -cE '^  \|\.{7}#\.{8}\|$' "$f")" -ge 15 ]; } ||
			{ [ "$(grep -cE '^  \|#{16}\|$' "$f")" -ge 2 ] && [ "$(grep -cE '^  \|#\.{14}#\|$' "$f")" -ge 14 ]; }; then
			echo "  ok      Zeigerinhalt (16x16, Form der Sonde) stimmt"
		else
			echo "  FEHLT   Zeigerinhalt"; fail=1
		fi
	else
		echo "  FEHLT   Zeigerform vom Server"; fail=1
	fi
	echo
	echo "Audio-Kanäle (Client-Log, Lauf 2):"
	audio() { # pattern description
		if grep -qE "$1" "$OUT/client-avc444.txt"; then echo "  ok      $2"; else echo "  FEHLT   $2"; fail=1; fi
	}
	audio "rdpsnd_recv_server_audio_formats_pdu" "Audio-Wiedergabe: Server-Formate empfangen"
	audio "rdpsnd_send_client_audio_formats" "Audio-Wiedergabe: Client-Formate gesendet (PulseAudio/PipeWire)"
	audio "audin_process_formats" "Mikrofon: Formate ausgehandelt"
	audio "stream state PA_STREAM_READY" "Mikrofon: Aufnahme-Stream bereit (PipeWire)"
	audio "Loaded v4l HAL for ecam" "Kamera: Kanal geladen (V4L2); Gegenstelle nur live"
	echo
	[ "$fail" = 0 ] && echo "Eingabe, Zeiger, Audio: alle Prüfungen bestanden" || echo "FEHLER"
} | tee "$OUT/summary.txt"
