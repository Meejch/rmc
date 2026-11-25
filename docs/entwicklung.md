# Entwicklung

## Aufbau des Repositorys

| Pfad | Inhalt |
|---|---|
| `crates/` | Rust-Workspace (Übersicht in [architektur.md](architektur.md)) |
| `crates/rdp-session/csrc/` | C-Zwischenschicht über libfreerdp |
| `crates/rmc/login/` | Login-Fenster `rmc-login` (C, WebKitGTK) |
| `crates/rdpgfx-gpu/shaders/` | Vulkan-Compute-Shader (GLSL, beim Build nach SPIR-V übersetzt) |
| `assets/` | Logo und Icons |
| `fixtures/` | Erzeugung und Prüfung der Teststreams ([fixtures/README.md](../fixtures/README.md)) |
| `tools/freerdp/` | FreeRDP-Build und Patches |
| `tools/appimage/` | AppImage-Build im Container |
| `tools/test/` | End-to-End-Tests gegen den FreeRDP-Shadow-Server |
| `tools/ui/` | Screenshots in einem unsichtbaren, verschachtelten KWin |
| `tools/xcheck/`, `tools/rec/`, `tools/xprobe/` | Abgleich mit FreeRDP, Aufnahme-Wiedergabe, X11-Eingabesonde |
| `docs/` | Dokumentation |

## Bauen

Siehe [installation.md](installation.md). Kurz:

```sh
tools/freerdp/build.sh --install     # einmal: FreeRDP nach ~/.local/opt/freerdp-rmc
cargo build --release
```

Die Shader werden beim Build mit `glslc` übersetzt (Paket `glslc` bzw. `shaderc`).

## Prüfen

Vor jedem Commit:

```sh
cargo fmt
cargo clippy --release --all-targets      # ohne Warnungen
cargo test --release
```

Bei Änderungen am Grafikkern zusätzlich:

```sh
fixtures/generate-all.sh                   # Teststreams erzeugen, prüfen; GPU muss bitgleich zur CPU-Referenz sein
RDPGFX_VALIDATION=1 target/release/gpu-check fixtures/out/multi-v1-box --frames 4   # Vulkan-Validierung
for b in nvidia vaapi software; do target/release/gpu-check fixtures/out/multi-v2-box --backend $b; done
```

`gpu-check` und `surface-check` nehmen `--backend auto|nvidia|vaapi|software`. Mit `RMC_VULKAN_DEVICE=radv` bzw. `llvmpipe` lässt sich ein reiner AMD- oder Software-Rechner nachstellen. Jeder Weg muss bitgleich zur CPU-Referenz sein.

Bei Änderungen an Sitzung, Eingabe oder Kanälen:

```sh
tools/test/shadow-e2e.sh
```

Der End-to-End-Test startet einen FreeRDP-Shadow-Server auf einem unsichtbaren Xvfb (Testkonto mit Zufallspasswort) und prüft:
- Ablehnung eines falschen Passworts
- AVC444- und AVC420-Sitzungen mit Bildvergleich gegen den Server
- Maus und Tastatur, die über eine X11-Sonde ankommen
- Zeigerformen und Audio-Kanäle

Alle Prüfungen müssen bestehen. Die Skripte nutzen FreeRDP aus `FREERDP_PREFIX`. `RMC_E2E_DECODER=vaapi` bzw. `software` prüft eine bestimmte Dekodierung.

## Oberfläche ohne sichtbare Fenster prüfen

```sh
tools/ui/nested-shot.sh bild.png 5 1300x900 -- target/release/rmc            # Launcher
tools/ui/shadow-shot.sh bild.png 8 --name Test                                # Sitzung gegen den Shadow-Server
```

Beide starten einen verschachtelten KWin (virtuelles Backend, eigener D-Bus) und machen nach der angegebenen Zeit einen Screenshot. Auf dem echten Desktop erscheint nichts.

Testschalter (Umgebungsvariablen):

| Variable | Wirkung |
|---|---|
| `RDPC_INSTANCE=<name>` | eigene Launcher-Instanz, ein laufender Launcher bleibt unberührt |
| `RDPC_NO_TRAY=1` | Launcher ohne Tray-Symbol |
| `RDPC_TEST_PAGE=history\|log\|settings` | Startseite des Launchers |
| `RDPC_TEST_EDITOR=defaults\|<index>[:<kategorie>]` | öffnet Einstellungen bzw. einen Favoriten |
| `RDPC_TEST_CLOSE=<s>` | schließt das Fenster nach s Sekunden (Tray-Test) |
| `RDPC_TEST_HEADLESS=1` | Sitzungen des Launchers ohne Fenster |
| `RDPC_TEST_MENU=1` | Zahnrad-Menü offen beim Start |
| `RDPC_TEST_MENU_ACTION=<id>` | löst 3 s nach dem Verbinden einen Menüeintrag aus |
| `RDPC_SECRET_BACKEND=creds` | Secret Service überspringen, nur `systemd-creds` |
| `RDP_VIEWER_DEBUG=1` | Diagnose der Ansichten und des Zeigers im Log |

Mit eigenen `XDG_*_HOME`-Verzeichnissen lassen sich Tests von den echten Einstellungen trennen.

## AppImage bauen

```sh
tools/appimage/build.sh
```

Baut in einem Container (Podman, Ubuntu 22.04 als Basis für eine alte glibc):
- x264, FFmpeg (nur die benötigten Teile), FreeRDP mit den Patches und RMC
- danach das AppDir mit allen Bibliotheken, die nicht zur Grundausstattung eines Desktops gehören

Das Ergebnis liegt in `target/appimage/`: das AppImage und das Archiv mit den Quellen der gebündelten Bibliotheken. Beides wird an das GitHub-Release gehängt.

**Wo die Abhängigkeiten festgehalten sind:**
- **Systempakete der Build-Umgebung:** `tools/appimage/Containerfile`
- **Feste Versionen** von FFmpeg, x264, nv-codec-headers, FreeRDP, appimagetool und der AppImage-Laufzeit: Anfang von `tools/appimage/inner.sh`
- **Systembibliotheken:** Welche Bibliotheken vom System kommen und nicht gebündelt werden, steht in der Liste `EXCLUDE` in `inner.sh`.

Gebaute Bibliotheken werden in `target/appimage/cache` zwischengespeichert. Ein neuer Build baut sie nur nach Änderungen am Abhängigkeitsteil von `inner.sh`, am Containerfile oder an den FreeRDP-Patches neu.

**Bekannte Stolpersteine** (bereits gelöst, beim Ändern beachten):
- **Kamerakanal:** Er braucht die libusb-Header (H.264-Erweiterung von UVC-Webcams).
- **FreeRDP-Optionen:** FreeRDP fest auf cJSON stellen (`WITH_CJSON_REQUIRED`), sonst nimmt es json-c ohne passende Header. `WITH_UNICODE_BUILTIN` vermeidet ICU (30 MB).
- **Shader:** Ubuntu 22.04 hat kein `glslc`. `build.rs` übersetzt dann mit `glslangValidator` und setzt die Includes selbst ein.
- **FFmpeg:** ohne libavformat, libavdevice und libavfilter. `ffmpeg-sys-next` ist deshalb auf das Feature `avcodec` beschränkt.
- **Neuere Cargo-Versionen** legen Build-Skripte eine Ebene tiefer ab. `rmc-login` wird neben `rmc` im Profilverzeichnis abgelegt (Suche nach `build/`).
- **OpenSSL:**
  - Wird nicht fest gebündelt: System-Bibliotheken wie libcups linken OpenSSL und Kerberos des Systems, Mischen bricht.
  - OpenSSL 3 liegt als Rückfall in `usr/lib/fallback` mit dem legacy-Modul (MD4/RC4 für NTLM). `AppRun` nutzt es nur, wenn das System kein `libssl.so.3` hat, z. B. Arch mit OpenSSL 4.
- **CA-Zertifikate:** `AppRun` setzt `SSL_CERT_FILE` auf die CA-Datei der Distribution (wichtig für das mitgelieferte OpenSSL).

**Portabilität prüfen:** das AppImage in Containern verschiedener Distributionen starten (`--appimage-extract`, dann `AppRun --probe-gpu --decoder software` mit Software-Vulkan) und eine Verbindung gegen den Shadow-Server aufbauen (`--network host`). Geprüft für 1.0.0: Ubuntu 22.04 und 24.04, Debian 12, Fedora 41, Arch. `RMC_E2E_BIN=<AppImage> tools/test/shadow-e2e.sh` führt den End-to-End-Test mit dem AppImage aus.

## Releases

1. Version in `Cargo.toml` setzen, `CHANGELOG.md` ergänzen, Commit.
2. Tag `v<Version>` setzen und pushen.
3. `tools/appimage/build.sh`, dann AppImage und Quellarchiv mit `gh release create v<Version> …` veröffentlichen. Die Release-Notizen kommen aus dem CHANGELOG.

## Konventionen

- **Sprache:** Doku, Oberfläche, Commit-Nachrichten und Changelog auf Deutsch. Code, Kommentare und Bezeichner auf Englisch.
- **Commits:** kleine, in sich geschlossene Schritte. Die erste Zeile sagt knapp, was sich ändert. Der Text danach erklärt, warum.
- **Änderungen** mit Wirkung für Nutzer kommen in den [CHANGELOG](../CHANGELOG.md).
- **Keine Kennungen im Repository:** keine echten Hostnamen, Konten, Tenant-IDs oder Mitschnitte. Lokale Dateien gehören nach `local/`, das Verzeichnis ist von Git ausgenommen.
- **FreeRDP-Änderungen** möglichst als Patch in `tools/freerdp/patches/` mit einer Notiz unter `docs/upstream/`, damit sie zurück an FreeRDP gehen können.
