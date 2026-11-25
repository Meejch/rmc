# Changelog

Alle nennenswerten Änderungen an RMC. Das Format folgt [Keep a Changelog](https://keepachangelog.com/de/1.1.0/), die Versionen folgen [Semantic Versioning](https://semver.org/lang/de/).

## [1.0.0] – 2026-11-25

### Hinzugefügt

- **Erste Version von RMC – Remote Machine Connect.**
- **AppImage** für x86_64 mit FreeRDP, FFmpeg und weiteren Bibliotheken. Läuft auf Ubuntu 22.04 und neuer und anderen aktuellen Distributionen. Startmenü-Eintrag beim ersten Start.
- **Anmeldung:**
  - Entra ID (RDS AAD) mit eigenem Login-Fenster und gespeicherter Anmeldung
  - Benutzer und Passwort (NLA) mit optional gespeichertem Passwort
  - Rückfrage bei unbekannten oder geänderten Zertifikaten
- **GPU-Grafikkern:**
  - H.264/AVC420/AVC444 mit Dekodierung über NVIDIA NVDEC, VA-API (AMD, Intel) oder die CPU, automatisch gewählt oder fest einstellbar
  - Zusammenbau und Farbumrechnung in Vulkan auf jeder Grafikkarte mit Vulkan 1.2
  - Frame-Quittung nach der Anzeige
  - Anzeige der erkannten Grafik im Launcher, `rmc --probe-gpu`
- **Anzeige:**
  - Vollbild über mehrere Monitore mit wählbarem Primärmonitor
  - Fenstermodus als 1:1-Ausschnitt oder skaliert
  - Tastatur-Grab
- **Verbindungsleiste:**
  - Name der Verbindung, Pin, Minimieren, Fenstermodus, Trennen
  - „Aktiv halten“
  - Speichern als Favorit
  - Zahnrad-Menü für Ton, Mikrofon, Zwischenablage, Laufwerke und Drucker während der Sitzung
- **Kanäle:**
  - Ton, Mikrofon, Webcam
  - Laufwerke, Drucker (CUPS)
  - Zwischenablage mit Text, Bildern und Dateien, Fortschrittsanzeige und Bandbreitengrenze
- **Launcher:**
  - Favoriten mit Suche und Erreichbarkeitsleuchte
  - mehrere gleichzeitige Verbindungen
  - Verlauf, Log-Ansicht
  - Einstellungen mit Standardwerten und Ausnahmen je Favorit
  - Tray, Startmenü-Eintrag (`rmc install-desktop`)
- **Speicher für Anmeldedaten:** Secret Service (KWallet, GNOME Keyring, KeePassXC), ohne ihn verschlüsselt mit `systemd-creds`.
- **FreeRDP-Patch:** Die Kamera wird nach einem Videoanruf freigegeben.

[1.0.0]: https://github.com/Meejch/rmc/releases/tag/v1.0.0
