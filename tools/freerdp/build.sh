#!/usr/bin/env bash
# Builds the FreeRDP version RMC is made for, with RMC's patches (tools/freerdp/patches).
#
#   tools/freerdp/build.sh [--install]
#
# Environment (all optional):
#   FREERDP_VERSION  git tag, default 3.33.1 (patches are looked up as <version>-*.patch)
#   FREERDP_SRC      source tree, default target/freerdp/src (cloned if missing)
#   FREERDP_BUILD    build directory, default target/freerdp/build
#   FREERDP_PREFIX   install prefix, default ~/.local/opt/freerdp-rmc (no root needed)
#
# RMC is then built against the same prefix: FREERDP_PREFIX=... cargo build --release
# (see docs/installation.md).
set -euo pipefail

INSTALL="${1:-}"
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
VERSION="${FREERDP_VERSION:-3.33.1}"
SRC="${FREERDP_SRC:-$REPO/target/freerdp/src}"
BUILD="${FREERDP_BUILD:-$REPO/target/freerdp/build}"
PREFIX="${FREERDP_PREFIX:-$HOME/.local/opt/freerdp-rmc}"

if [[ ! -d "$SRC" ]]; then
	mkdir -p "$(dirname "$SRC")"
	git clone --depth 1 --branch "$VERSION" https://github.com/FreeRDP/FreeRDP.git "$SRC"
fi

# Apply the patches once (idempotent).
for PATCH in "$HERE/patches/$VERSION"-*.patch; do
	[[ -f "$PATCH" ]] || continue
	if git -C "$SRC" apply --reverse --check "$PATCH" 2>/dev/null; then
		echo "$(basename "$PATCH"): already applied"
	else
		git -C "$SRC" apply "$PATCH"
		echo "$(basename "$PATCH"): applied"
	fi
done

# FFmpeg for audio and the camera encoder (no OpenH264), Entra ID login (AAD) with the
# out-of-process helpers, PulseAudio, CUPS printers, camera redirection (RDPECAM).
# The SDL client is only a reference for comparisons; the shadow server (X11 capture,
# AVC420/AVC444 via libx264) is the local test target of tools/test.
cmake -S "$SRC" -B "$BUILD" -G Ninja \
	-DCMAKE_BUILD_TYPE=Release \
	-DCMAKE_INSTALL_PREFIX="$PREFIX" \
	-DCMAKE_INSTALL_RPATH="$PREFIX/lib64" \
	-DWITH_FFMPEG=ON -DWITH_VIDEO_FFMPEG=ON -DWITH_DSP_FFMPEG=ON -DWITH_SWSCALE=ON \
	-DWITH_FFMPEG_HWACCEL=ON \
	-DWITH_OPENH264=OFF \
	-DWITH_AAD=ON -DWITH_SSO_MIB=ON \
	-DWITH_WEBVIEW_AAD_AUTH_HELPER=ON -DWITH_XDG_AAD_AUTH_HELPER=ON \
	-DWITH_CLIENT_SDL=ON -DWITH_CLIENT_SDL3=ON -DWITH_CLIENT_SDL2=OFF \
	-DWITH_PULSE=ON -DWITH_ALSA=ON -DWITH_CUPS=ON -DWITH_FUSE=ON \
	-DCHANNEL_RDPECAM=ON -DCHANNEL_RDPECAM_CLIENT=ON \
	-DWITH_SERVER=ON -DWITH_SHADOW=ON -DWITH_SHADOW_SUBSYSTEM=ON -DWITH_SERVER_SHADOW_CLI=ON \
	-DWITH_PROXY=OFF -DWITH_SAMPLE=OFF \
	-DWITH_MANPAGES=OFF -DBUILD_TESTING=OFF

cmake --build "$BUILD" -j"$(nproc)"

if [[ "$INSTALL" == "--install" ]]; then
	mkdir -p "$PREFIX" 2>/dev/null || true
	if [[ ! -w "$PREFIX" ]]; then
		echo "error: $PREFIX is not writable (set FREERDP_PREFIX to a writable directory)" >&2
		exit 1
	fi
	cmake --install "$BUILD"
	echo "installed: $PREFIX"
fi
