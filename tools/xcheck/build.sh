#!/usr/bin/env bash
# Builds the FreeRDP cross-check helpers (gen-fixtures xcheck/xsplit compare the CPU reference
# with FreeRDP's primitives) against the FreeRDP install in FREERDP_PREFIX.
set -euo pipefail
VERSION="${1:-3.33.1}"
P="${FREERDP_PREFIX:-$HOME/.local/opt/freerdp-rmc}"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/../../target/xcheck"
mkdir -p "$OUT"
for tool in freerdp-combine freerdp-split; do
	cc -O2 -Wall -o "$OUT/$tool-$VERSION" "$HERE/$tool.c" \
		-I"$P/include/freerdp3" -I"$P/include/winpr3" -L"$P/lib64" -Wl,-rpath,"$P/lib64" -lfreerdp3 -lwinpr3
	echo "$OUT/$tool-$VERSION"
done
