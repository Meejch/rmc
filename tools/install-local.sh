#!/usr/bin/env bash
# Installs the current release build for the current user (no root needed):
#   ~/.local/opt/rmc/{rmc,rmc-login}         copies, so rebuilding the repository does not
#                                             disturb a running session
#   ~/.local/share/applications/rmc.desktop   start menu entry "RMC" (rmc install-desktop)
#   ~/.local/share/icons/hicolor/*/apps/rmc.png
# The binaries link libfreerdp from the prefix they were built against (rpath, see
# docs/installation.md).
#
#   tools/install-local.sh [--build] [--remove]
set -euo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)
DEST="$HOME/.local/opt/rmc"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"

refresh_menu() { command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true; }

if [[ "${1:-}" == "--remove" ]]; then
	rm -rf "$DEST" "$DATA/applications/rmc.desktop"
	rm -f "$DATA"/icons/hicolor/*/apps/rmc.png
	refresh_menu
	echo "entfernt: $DEST und Menüeintrag (Einstellungen und Anmeldungen bleiben; rmc --logout löscht die Anmeldungen)"
	exit 0
fi

if [[ "${1:-}" == "--build" ]]; then
	(cd "$REPO" && cargo build --release -p rmc)
fi

for f in rmc rmc-login; do
	[[ -x "$REPO/target/release/$f" ]] || { echo "fehlt: target/release/$f (cargo build --release)" >&2; exit 1; }
done

mkdir -p "$DEST"
# Replace atomically (a running session keeps its old inode).
for f in rmc rmc-login; do
	install -m 755 "$REPO/target/release/$f" "$DEST/.$f.new"
	mv -f "$DEST/.$f.new" "$DEST/$f"
done

"$DEST/rmc" install-desktop
refresh_menu

echo "installiert: $DEST"
echo "Startmenü: „RMC“ (Internet, oder Suche nach „RMC“ bzw. „RDP“)"
