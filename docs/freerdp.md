# FreeRDP

RMC verwendet [FreeRDP](https://github.com/FreeRDP/FreeRDP) als Bibliothek für das RDP-Protokoll. FreeRDP übernimmt:
- TLS, NLA und die Entra-ID-Anmeldung (RDS AAD)
- die virtuellen Kanäle für Ton, Mikrofon, Kamera, Laufwerke, Drucker und Zwischenablage
- den automatischen Reconnect

Die Grafik (RDPEGFX) wertet RMC selbst aus: Die H.264-Daten gehen am FreeRDP-Decoder vorbei direkt an den GPU-Kern (siehe [architektur.md](architektur.md)).

## Version

RMC ist auf **FreeRDP 3.33.1** abgestimmt. Die C-Zwischenschicht nutzt die öffentliche API von FreeRDP 3 (`freerdp_settings_*`, Kanal-Kontexte, Callbacks). Andere 3.x-Versionen funktionieren voraussichtlich, sind aber nicht geprüft.

## Build

`tools/freerdp/build.sh` baut FreeRDP mit den nötigen Optionen:
- FFmpeg für Audio und Kamera
- AAD mit den Login-Helfern
- PulseAudio, CUPS und die Kamera-Umleitung (RDPECAM)
- den Shadow-Server als lokalen Testserver

Siehe [installation.md](installation.md).

## Patches

Die Patches liegen in `tools/freerdp/patches/` und werden vom Build-Skript angewendet.

| Patch | Zweck |
|---|---|
| `3.33.1-ecam-idle-release.patch` | Gibt die Kamera frei, wenn der Server nach einem Videoanruf keine Bilder mehr anfordert. Ohne den Patch bleibt die Kamera bei durchgereichtem H.264 an, bis die Verbindung endet. Details in [upstream/002](upstream/002-freerdp-ecam-kamera-bleibt-an.md). |

## Hinweise für FreeRDP

Unter [upstream/](upstream/) stehen Befunde, die als Issue oder Pull Request an FreeRDP gehen sollen:

- [001: AVC444 ohne Chroma-Rückfilter](upstream/001-freerdp-avc444-rueckfilter.md)
- [002: Kamera bleibt nach dem Videoanruf an](upstream/002-freerdp-ecam-kamera-bleibt-an.md)
