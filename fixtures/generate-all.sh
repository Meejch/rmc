#!/usr/bin/env bash
# Generates and verifies the standard fixture set into fixtures/out/.
# Usage: fixtures/generate-all.sh [frames]   (default 120 = 2 s at 60 fps)
set -euo pipefail
cd "$(dirname "$0")/.."
FRAMES="${1:-120}"
cargo build -q --release -p rdpgfx-fixtures
B=target/release/gen-fixtures
O=fixtures/out

gen() { local name=$1; shift; echo "== $name"; $B generate --frames "$FRAMES" "$@" "$O/$name"; }

gen multi-v1-box    --layout multi  --codec avc444 --avc444 v1 --chroma box
gen multi-v2-box    --layout multi  --codec avc444 --avc444 v2 --chroma box
gen multi-v1-sample --layout multi  --codec avc444 --avc444 v1 --chroma sample
gen multi-v1-video  --layout multi  --codec avc444 --avc444 v1 --chroma box --content video
gen multi-avc420    --layout multi  --codec avc420
gen single-v1-box   --layout single --codec avc444 --avc444 v1 --chroma box
gen single-avc420   --layout single --codec avc420

echo "== verify (software decode)"
# Thresholds: worst YUV plane vs. codec-free reference. The video fixture is full-motion
# noise on purpose (encoder worst case), so its codec loss is larger.
for d in "$O"/*/; do
	case "$(basename "$d")" in
	*-video) t=24 ;;
	*) t=30 ;;
	esac
	$B verify "$d" --threshold "$t"
done
echo "== verify (NVDEC) multi-v1-box, single-v1-box"
$B verify "$O/multi-v1-box" --decoder cuda
$B verify "$O/single-v1-box" --decoder cuda
if [[ -x target/xcheck/freerdp-combine-3.33.1 ]]; then
	echo "== FreeRDP cross-check"
	$B xcheck "$O/multi-v1-sample"
fi

if command -v nvidia-smi >/dev/null; then
	echo "== GPU core (NVDEC + Vulkan compute) vs. CPU reference"
	cargo build -q --release -p rdpgfx-gpu
	for d in "$O"/*/; do target/release/gpu-check "$d" | tail -1; done
	target/release/gpu-check "$O/multi-v1-box" --regions --frames 60 | tail -1
	echo "== Surface model on the GPU (random command sequences) vs. CPU reference"
	target/release/surface-check "$O/multi-v2-box" --steps 1000 --seed 1 | tail -1
	target/release/surface-check "$O/single-avc420" --steps 400 --seed 2 | tail -1
fi
