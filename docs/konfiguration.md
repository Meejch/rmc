# Konfiguration

Alle Einstellungen lassen sich im Launcher bearbeiten. Die Dateien sind TOML und dürfen auch von Hand geändert werden, solange der Launcher nicht läuft.

## Verzeichnisse

RMC hält sich an die XDG-Basisverzeichnisse:

| Ort | Inhalt |
|---|---|
| `~/.config/rmc/settings.toml` | Launcher-Einstellungen und Standardwerte |
| `~/.config/rmc/favorites.toml` | Favoriten |
| `~/.config/rmc/freerdp/` | Dateien von FreeRDP, u. a. die vertrauten Server-Zertifikate (`server/`) |
| `~/.local/state/rmc/session.toml` | gemerkte Leistenzustände (angepinnt, Aktiv halten) |
| `~/.local/state/rmc/history.toml` | Verlauf der Verbindungen (ohne Passwörter) |
| `~/.local/state/rmc/logs/` | Sitzungs-Logs, die letzten 20 |
| `~/.local/share/rmc/login/` | Profil des Login-Fensters (Cookies der Entra-Anmeldung) |
| `~/.local/share/rmc/secrets/` | verschlüsselte Anmeldungen, nur ohne Schlüsselbund (siehe [sicherheit.md](sicherheit.md)) |
| `~/.cache/rmc/clipboard/` | aus Windows eingefügte Dateien, älter als ein Tag wird beim Start entfernt |

Gesetzte Variablen `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME` und `XDG_CACHE_HOME` werden beachtet.

## settings.toml

```toml
close_to_tray = true      # X schließt nur das Fenster, der Launcher bleibt im Tray

[defaults]                # Standardwerte aller Verbindungen (Schlüssel siehe unten)
bar_pinned = true
clipboard_limit = 50
reach_intervals = [5, 15, 25, 40, 60]
```

## favorites.toml

```toml
[[favorite]]
name = "Büro-PC"
host = "pc01.example.local"       # optional mit Port: "pc01.example.local:3390"
user = "benutzer@example.com"     # bei Entra ID die E-Mail-Adresse (UPN)
entra = true                      # false = Benutzer und Passwort (NLA)
connect_on_start = false          # beim Start des Launchers verbinden

[favorite.overrides]              # eigene Werte dieses Favoriten, sonst gilt [defaults]
monitors = ["DP-1", "DP-2"]
camera = false

[[favorite]]
name = "Server"
host = "srv01.example.local"
user = "administrator"
domain = "EXAMPLE"                # leer = lokales Konto
entra = false
```

Passwörter und Anmeldetoken stehen nie in diesen Dateien.

## Optionen

Diese Schlüssel gibt es unter `[defaults]` und `[favorite.overrides]`:

| Schlüssel | Standard | Bedeutung |
|---|---|---|
| `monitors` | `[]` (alle) | Monitore der Sitzung, Connector-Namen wie `DP-1` |
| `primary` | `""` | Primärmonitor, leer = Reihenfolge des Desktops |
| `start_windowed` | `false` | im Fenstermodus starten |
| `bar_pinned` | `true` | Verbindungsleiste immer sichtbar |
| `sound` | `true` | Tonwiedergabe |
| `microphone` | `true` | Mikrofon |
| `camera` | `true` | Kamera |
| `clipboard` | `true` | Zwischenablage |
| `clipboard_limit` | `50` | Bandbreite für Dateien über die Zwischenablage in Mbit/s, 0 = unbegrenzt |
| `drives` | `[]` | Ordner als Laufwerke, z. B. `[{ name = "Home", path = "~" }]` |
| `printers` | `[]` | CUPS-Drucker (Namen wie in `lpstat -e`) |
| `decoder` | `"auto"` | Dekodierung: `auto` (NVIDIA, sonst VA-API, sonst CPU), `nvidia`, `vaapi`, `software` |
| `avc420_only` | `false` | nur AVC420 anbieten (Fehlersuche) |
| `reverse_filter` | `true` | Chroma-Rückfilter bei AVC444 |
| `auto_reconnect` | `true` | nach Netzaussetzern neu verbinden |
| `keyboard_grab` | `true` | Tastatur-Grab im Vollbild |
| `keep_active` | `false` | Aktiv halten von Beginn an |
| `login_helper` | `"login"` | Entra-Login: `login` (eigenes Fenster mit gespeicherter Anmeldung), `webview` (FreeRDP), `xdg` (Systembrowser) |
| `log_level` | `"INFO"` | Log-Stufe von FreeRDP: ERROR, WARN, INFO, DEBUG |
| `reach_check` | `true` | Erreichbarkeit im Launcher prüfen |
| `reach_intervals` | `[5, 15, 25, 40, 60]` | Prüfabstände in Sekunden, eine Stufe je `reach_stage` |
| `reach_stage` | `60` | Dauer einer Stufe in Sekunden, die letzte Stufe bleibt |

## Kommandozeile

`rmc --help` listet alle Optionen. Die wichtigsten:

| Aufruf | Wirkung |
|---|---|
| `rmc` | Launcher |
| `rmc connect <Favorit>` | Sitzung eines Favoriten ohne Launcher. Weitere Optionen dürfen folgen. |
| `rmc --host H --user U [--domain D]` | Sitzung ohne Favorit, Passwort aus der Umgebungsvariable `RDP_PASSWORD` |
| `rmc --host H --user U --aad` | Sitzung mit Entra-ID-Anmeldung |
| `rmc install-desktop` | Startmenü-Eintrag und Icons für dieses Programm anlegen |
| `rmc --probe-gpu [--decoder X]` | zeigt, welche Dekodierung und welches Vulkan-Gerät hier genutzt würden |
| `rmc … --decoder vaapi` | feste Dekodierung für diese Sitzung |

Umgebungsvariablen:

| Variable | Wirkung |
|---|---|
| `RMC_VULKAN_DEVICE=<Namensteil>` | Vulkan-Gerät wählen, wenn nicht über NVDEC festgelegt (z. B. `radv`, `llvmpipe`) |
| `rmc --logout` | alle gespeicherten Anmeldungen löschen |
