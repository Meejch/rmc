#!/usr/bin/env bash
# Builds the RMC AppImage in a container (rootless podman, nothing is installed on the host).
#
#   tools/appimage/build.sh
#
# Result in target/appimage/: RMC-<version>-x86_64.AppImage and
# RMC-<version>-third-party-sources.tar.xz (sources of the bundled libraries: FFmpeg, x264,
# FreeRDP with RMC's patches). Downloads are cached in target/appimage/cache.
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
OUT=${OUT:-$REPO/target/appimage}
IMAGE=rmc-appimage-build:ubuntu22.04
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)
mkdir -p "$OUT/cache"
podman build -t "$IMAGE" -f "$REPO/tools/appimage/Containerfile" "$REPO/tools/appimage"
podman run --rm --security-opt label=disable \
	-v "$REPO":/src:ro -v "$OUT":/out \
	-e VERSION="$VERSION" \
	"$IMAGE" bash /src/tools/appimage/inner.sh
ls -la "$OUT"/RMC-"$VERSION"-*
