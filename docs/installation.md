# Installation

Am einfachsten ist das **AppImage** aus den [Releases](https://github.com/Meejch/rmc/releases). Alternativ lässt sich RMC aus dem Quellcode bauen.

## AppImage

```sh
chmod +x RMC-1.0.0-x86_64.AppImage
./RMC-1.0.0-x86_64.AppImage
```

- **Startmenü:** Beim ersten Start legt RMC den Eintrag „RMC“ mit Logo an. Wird das AppImage verschoben, korrigiert der nächste Start den Eintrag. Ein Eintrag einer aus dem Quellcode installierten Version bleibt unangetastet.
- **Mitgeliefert:** FreeRDP (mit den Patches von RMC), FFmpeg (H.264, Audio-Codecs, x264 für Webcams ohne eigenes H.264) und weitere Bibliotheken. Gebaut auf Ubuntu 22.04, läuft also auf Ubuntu 22.04 und neuer, Debian 12+, Fedora 36+ und anderen aktuellen Distributionen.
- **Vom System kommen:** Grafiktreiber (Vulkan, VA-API, NVIDIA), Wayland, PulseAudio/PipeWire, CUPS, Kerberos, OpenSSL 3 (sonst nimmt RMC eine mitgelieferte Version) und für das Entra-Login-Fenster WebKitGTK 4.1. Für den Anmelde-Rückfall über den Browser wird Python 3 gebraucht. Fehlt WebKitGTK, meldet RMC sich über den Systembrowser an, dann allerdings ohne gespeicherte Anmeldung.
- **Einstellungen:** Das AppImage nutzt dieselben Verzeichnisse wie eine gebaute Installation (siehe [konfiguration.md](konfiguration.md)).
- **FUSE:** AppImages brauchen `libfuse2` bzw. FUSE. Ohne FUSE: `./RMC-…AppImage --appimage-extract-and-run`.

Die Quellen der mitgelieferten Bibliotheken liegen jedem Release als `RMC-<Version>-third-party-sources.tar.xz` bei. Gebaut wird das AppImage mit `tools/appimage/build.sh` (siehe [entwicklung.md](entwicklung.md)).

## Aus dem Quellcode

### Voraussetzungen

| Bereich | Anforderung |
|---|---|
| System | Linux mit Wayland-Compositor. Entwickelt und getestet unter Fedora 44 mit KDE Plasma 6 (KWin). |
| Grafik | Ein Vulkan-1.2-Treiber. RMC dekodiert automatisch auf NVIDIA (NVDEC, proprietärer Treiber mit CUDA), auf AMD/Intel (VA-API, Mesa) oder auf der CPU. Siehe [Grafikbeschleunigung](#grafikbeschleunigung) unten. |
| Compositor-Protokolle | `ext-data-control-v1` (Zwischenablage), `keyboard-shortcuts-inhibit` (Tastatur-Grab). KWin bietet beide an. |
| Gegenstelle | Windows 10/11 oder Windows Server mit RDP. Für die Entra-ID-Anmeldung muss der Rechner Entra-ID-gejoint sein. |

Optionale Programme zur Laufzeit:

- `libsecret` mit einem Secret-Service-Dienst (KWallet, GNOME Keyring, KeePassXC) zum Speichern von Anmeldungen. Ohne ihn verschlüsselt `systemd-creds`.
- `pactl` (PulseAudio oder PipeWire) für Ton- und Mikrofon-Stummschaltung im Zahnrad-Menü.
- `lpstat` (CUPS) für die Druckerumleitung.
- `kdialog` oder `zenity` für die Ordnerauswahl im Zahnrad-Menü.

### Grafikbeschleunigung

| Hardware | Dekodierung | Voraussetzung |
|---|---|---|
| NVIDIA | NVDEC, Bild bleibt auf der GPU | proprietärer Treiber (CUDA, Vulkan), FFmpeg mit CUDA-Unterstützung |
| AMD, Intel | VA-API | Mesa-VA-API-Treiber mit H.264 (unter Fedora `mesa-va-drivers-freeworld` bzw. `intel-media-driver` aus RPM Fusion), Vulkan (RADV, ANV) |
| ohne passende Hardware | CPU (libavcodec) | beliebiger Vulkan-Treiber, notfalls Software-Vulkan (llvmpipe) |

- **Auswahl:** „Automatisch“ probiert in dieser Reihenfolge NVDEC, VA-API und die CPU. Ein Hardware-Dekoder, der beim ersten Bild scheitert, schaltet für diese Fläche auf die CPU um.
- **Anzeige:** Was erkannt wurde, zeigt der Launcher unter Einstellungen → Leistung. Was eine Sitzung tatsächlich nutzt, steht bei der aktuellen Verbindung.
- **Prüfen in der Konsole:** `rmc --probe-gpu`.

### Abhängigkeiten für den Build (Fedora)

```sh
sudo dnf install cmake ninja-build gcc gcc-c++ git pkgconf \
  openssl-devel krb5-devel cjson-devel json-c-devel uriparser-devel \
  pulseaudio-libs-devel alsa-lib-devel opus-devel cups-devel fuse3-devel libusb1-devel \
  libX11-devel libXext-devel libXfixes-devel libXrandr-devel libXi-devel \
  libxkbcommon-devel wayland-devel libv4l-devel SDL3-devel \
  webkit2gtk4.1-devel gtk3-devel glslc
```

- **FFmpeg:** FFmpeg mit H.264-Decoder. Für NVDEC und VA-API mit Hardware-Unterstützung, unter Fedora `ffmpeg-devel` aus RPM Fusion.
- **Rust:** ab Version 1.85 (Edition 2024).
- **Tests:** Für die lokalen Tests zusätzlich `xorg-x11-server-Xvfb` und `libX11-devel`.

Auf anderen Distributionen heißen die Pakete ähnlich. Maßgeblich ist, was FreeRDP 3 für seinen Build verlangt.

### 1. FreeRDP bauen

RMC nutzt libfreerdp 3.33.1 mit einem Patch (siehe [freerdp.md](freerdp.md)). Das Skript klont die Quellen nach `target/freerdp`, wendet die Patches an, baut und installiert ohne root-Rechte nach `~/.local/opt/freerdp-rmc`:

```sh
tools/freerdp/build.sh --install
```

Andere Orte lassen sich über `FREERDP_SRC`, `FREERDP_BUILD` und `FREERDP_PREFIX` festlegen.

### 2. RMC bauen

```sh
cargo build --release
```

Gebaut wird gegen `FREERDP_PREFIX` (Standard `~/.local/opt/freerdp-rmc`). Die Programme finden die FreeRDP-Bibliotheken über den eingebetteten Bibliothekspfad (rpath). Liegt FreeRDP woanders:

```sh
FREERDP_PREFIX=/pfad/zu/freerdp cargo build --release
```

Das Login-Fenster für Entra ID (`rmc-login`) wird nur gebaut, wenn WebKitGTK 4.1 vorhanden ist. Fehlt es, gibt Cargo eine Warnung aus. Die Anmeldung läuft dann über FreeRDPs eigene Helfer.

### 3. Installieren

```sh
tools/install-local.sh
```

- Kopiert `rmc` und `rmc-login` nach `~/.local/opt/rmc`. Ein späterer Neubau stört so keine laufende Sitzung, erst ein erneuter Aufruf aktualisiert die Kopie.
- Legt mit `rmc install-desktop` den Startmenü-Eintrag „RMC“ samt Icons an.
- `tools/install-local.sh --build` baut vorher, `tools/install-local.sh --remove` entfernt Programm und Menüeintrag. Einstellungen und gespeicherte Anmeldungen bleiben dabei erhalten. `rmc --logout` löscht die Anmeldungen.

### Aktualisieren

```sh
git pull
cargo build --release
tools/install-local.sh
```

Danach den Launcher über „Beenden“ schließen und neu starten.
