# Architektur

RMC ist eine Rust-Anwendung, die das RDP-Protokoll über **libfreerdp** abwickelt und die Grafik mit einem **eigenen GPU-Kern** darstellt. Das Ziel ist ein Client, der auch mit mehreren großen Monitoren flüssig und ressourcenschonend läuft:
- H.264 wird in Hardware dekodiert, wo möglich.
- Die Bilder werden auf der GPU zusammengesetzt.
- Nur geänderte Ansichten werden neu gezeichnet.

## Überblick

```
┌──────────────────────── Launcher (rmc) ────────────────────────┐
│ egui/eframe: Verbinden, Verlauf, Log, Einstellungen · Tray     │
│ Favoriten, Erreichbarkeit, Passwort-/Zertifikatsdialoge        │
└───────┬─────────────────────────────────────────────▲──────────┘
        │ startet je Verbindung einen Prozess         │ Statuszeilen (Log)
        │ stdin: Passwort, Antworten auf Rückfragen   │
┌───────▼──────────────── Sitzung (rmc --host …) ─────┴──────────┐
│ rdp-session ── C-Zwischenschicht über libfreerdp 3             │
│   Verbindung, Anmeldung (NLA, Entra ID), Kanäle, Ereignisse    │
│        │ RDPEGFX-Befehle, H.264         ▲ Eingabe, Quittungen  │
│ rdpgfx-gpu ── GPU-Kern                  │                      │
│   NVDEC/VA-API/CPU → Vulkan: AVC420/444, YUV→RGB, Surfaces     │
│        │ Surface-Bilder                 │                      │
│ rdp-viewer ── Wayland-Fenster je Monitor, Leiste, Eingabe      │
│ rdp-clipboard ── Wayland-Zwischenablage ↔ RDP-Formate          │
└────────────────────────────────────────────────────────────────┘
```

## Crates

| Crate | Aufgabe |
|---|---|
| `rmc` | Programm `rmc`: Launcher und Sitzungsprozess, Favoriten, Zwischenablage-Brücke, Menü der Leiste. Dazu das Login-Fenster `rmc-login` (C, WebKitGTK) |
| `rdp-session` | Anbindung an libfreerdp über eine dünne C-Schicht (`csrc/`): Verbindung, Ereignisse, Eingabe, Frame-Quittungen, Kanäle, Entra-Token-Cache (`aadtoken.c`), Secret-Speicher (`secret.c`) |
| `rdpgfx-gpu` | GPU-Kern: H.264-Dekodierung (FFmpeg mit NVDEC, VA-API oder CPU), AVC444-Zusammenbau und Farbumrechnung in Vulkan-Compute-Shadern, Surface-Modell, Darstellung und Overlay |
| `rdpgfx-model` | protokollneutrale RDPGFX-Typen und die CPU-Referenz für AVC444 und die Farbumrechnung |
| `rdp-viewer` | Wayland-Fenster je Monitor, Fenstermodus, Verbindungsleiste, Tastatur-Grab, Mauszeiger, Zuordnung der Monitore |
| `rdp-clipboard` | Wayland-Zwischenablage über `ext-data-control` und Umwandlung in RDP-Formate |
| `rdpgfx-fixtures` | erzeugt und prüft synthetische AVC420/AVC444-Teststreams |
| `rdpgfx-replay` | spielt Teststreams durch den GPU-Kern in Wayland-Fenster (Entwicklungswerkzeug) |

## Prozesse

- **Launcher:** Er startet für jede Verbindung einen eigenen Sitzungsprozess, dasselbe Programm `rmc` mit den Optionen des Favoriten. Dadurch ist jede Sitzung von den anderen und vom Launcher getrennt. Ein beendeter Launcher beendet keine Sitzung.
- **Zeilenprotokoll:** Die Kommunikation läuft über ein einfaches Zeilenprotokoll.
  - Die Sitzung schreibt Statuszeilen in ihr Log („Verbunden:“, „Getrennt:“, Leistenaktionen), der Launcher liest mit.
  - Rückfragen kommen als Zeile `RDPC-CERT …`. Antwort und Passwort gehen über stdin an die Sitzung.
  - Trennen ist ein SIGTERM an den Sitzungsprozess, der die Verbindung dann sauber trennt. Die Windows-Sitzung bleibt bestehen.

## Grafikpfad

1. **Abfangen:** FreeRDP empfängt den Grafikkanal (MS-RDPEGFX). `rdp-session` übernimmt dessen Callbacks und reicht die Befehle unverändert weiter: Surfaces anlegen, H.264-Teilbilder, Füllen, Kopieren, Cache. Der Software-Decoder von FreeRDP bleibt unbenutzt.
2. **Dekodieren:** FFmpeg dekodiert auf einem von drei Wegen. Das Ergebnis ist in jedem Fall ein NV12-Bild im Ansichtspuffer der Fläche.
   - **NVDEC (NVIDIA):** Die Bilder liegen im CUDA-Speicher und werden GPU-intern in Puffer kopiert, die Vulkan über External Memory teilt. Kein Umweg über den Hauptspeicher.
   - **VA-API (AMD, Intel):** Dekodiert wird in Hardware. Das Bild kommt in den Hauptspeicher und wird in einen für die CPU sichtbaren Vulkan-Puffer geschrieben. Ein kopierfreier Weg über DMA-BUF ist geplant.
   - **CPU:** libavcodec mit Slice-Threads, Übergabe wie bei VA-API.
   - **Auswahl:** automatisch in dieser Reihenfolge oder fest per Einstellung. Der Vulkan-Teil läuft auf jeder Grafikkarte mit Vulkan 1.2. Mit NVDEC ist es dieselbe GPU, sonst die leistungsfähigste (diskret vor integriert vor Software).
3. **Zusammensetzen:**
   - Compute-Shader setzen bei AVC444 Haupt- und Hilfsansicht zusammen, wahlweise mit Chroma-Rückfilter (siehe [upstream/001](upstream/001-freerdp-avc444-rueckfilter.md)).
   - Weitere Shader rechnen YUV in RGB um und schreiben das Ergebnis in die Surface-Bilder.
   - Füllen, Kopieren und Cache sind ebenfalls GPU-Operationen.
4. **Darstellen:** Jede Ansicht (ein Fenster je Monitor, im Fenstermodus ein Ausschnitt) ist eine Vulkan-Swapchain. Ein Present-Shader überträgt den passenden Ausschnitt der Surfaces. Danach zeichnet der Overlay-Shader Verbindungsleiste, Fortschritt und Menü.
5. **Flusskontrolle:** Die Frame-Quittung an den Server geht erst nach dem Present hinaus. So drosselt der Encoder in Windows selbst, wenn der Client nicht hinterherkommt.

Die CPU-Referenz in `rdpgfx-model` ist die verbindliche Vorgabe: Die Tests prüfen, dass der GPU-Kern dieselben Bilder bitgleich erzeugt, mit jedem der drei Dekodierwege.

## Leiste und Overlay

Das Overlay wird nie in die Session-Flächen geschrieben, sondern nach dem Session-Bild in die Swapchain gezeichnet.
- **Formen:** Knöpfe und Symbole zeichnet ein Compute-Shader.
- **Text und Menü:** Text (Name der Verbindung) und das Zahnrad-Menü rendert `rdp-viewer` auf der CPU (`ab_glyph`, Systemschrift über fontconfig) als kleines Bild, das der Shader einblendet.
- **Kosten:** Eine reine Overlay-Änderung löst ein Present aus, ohne die Quittungen an den Server zu berühren.

## Anmeldung

- **Benutzer und Passwort:** NLA (CredSSP) über FreeRDP.
- **Entra ID (RDS AAD):** FreeRDP übernimmt Protokoll und PoP-Token. RMC liefert über den `GetAccessToken`-Callback die Token aus `aadtoken.c`:
  - Mit gespeichertem Refresh-Token holt RMC still neue Zugriffstoken.
  - Sonst öffnet `rmc-login` die Anmeldung (Passwort, 2FA). Das Refresh-Token daraus wird für den Session-Host eingetauscht und gespeichert.
- **Speicher:** Wo Token und Passwörter liegen, steht in [sicherheit.md](sicherheit.md).

## Kanäle

Ton (rdpsnd), Mikrofon (audin), Kamera (RDPECAM), Laufwerke und Drucker (rdpdr) laufen über die Plugins von FreeRDP.
- **Gerätekanal:** RMC lädt rdpdr immer. Laufwerke und Drucker lassen sich so auch während der Sitzung anmelden (`RdpdrRegisterDevice`).
- **Zwischenablage:** Die Brücke in `rmc` verbindet den cliprdr-Kanal mit `rdp-clipboard`. Dateien werden in Blöcken und mit begrenzter Bandbreite übertragen.

## Verzeichnisse und Einstellungen

Siehe [konfiguration.md](konfiguration.md).
