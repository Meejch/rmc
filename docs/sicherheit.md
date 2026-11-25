# Sicherheit und Datenschutz

## Anmeldedaten

**Speicherort**
- Passwörter und Entra-Refresh-Token liegen im **Secret Service** des Desktops: KWallet, GNOME Keyring oder KeePassXC.
- RMC lädt dazu `libsecret` zur Laufzeit, es gibt keine feste Abhängigkeit.
- Gibt es keinen Secret Service, verschlüsselt RMC mit **`systemd-creds`** im Benutzerbereich (an Rechner und Benutzer gebunden, mit TPM2, wenn vorhanden). Die Datei liegt in `~/.local/share/rmc/secrets/` (Rechte 0700).
- Klartext wird nie geschrieben.

**Übergabe an die Sitzung**
- Das Passwort geht über eine Pipe (stdin) an den Sitzungsprozess, nie über die Kommandozeile oder Umgebungsvariablen.
- Gespeichert wird es erst nach einer erfolgreichen Anmeldung.

**Entra ID**
- Das Login-Fenster (`rmc-login`, WebKitGTK) behält die Cookies der Anmeldung in einem eigenen Profil (`~/.local/share/rmc/login`, Rechte 0700).
- Das Refresh-Token tauscht RMC still gegen neue Zugriffstoken.
- Die Zugriffstoken sind an den Schlüssel der Sitzung gebunden (PoP, über FreeRDP) und werden nicht gespeichert.

**Löschen**
- `rmc --logout` löscht alle gespeicherten Anmeldungen und das Login-Profil.
- „Gespeicherte Anmeldung verwerfen“ löscht die eines Favoriten.

## Server-Zertifikate

- **Unbekanntes Zertifikat:** Der Launcher zeigt Inhaber, Aussteller und Fingerabdruck und fragt nach. Ein vertrautes Zertifikat liegt danach in `~/.config/rmc/freerdp/server/`.
- **Geänderter Fingerabdruck:** Es erscheint eine Warnung mit altem und neuem Wert. Ohne Bestätigung wird die Verbindung abgelehnt.
- **Ausnahme für Entra-ID-Rechner:** Ihr kurzlebiges Gerätezertifikat (Aussteller `MS-Organization-P2P-Access`) wird regelmäßig erneuert. Eine Erneuerung wird automatisch angenommen, wenn Aussteller und Inhaber (Geräte- und Tenant-ID) gleich bleiben.
- `--ignore-cert` schaltet die Prüfung ab und ist nur für lokale Testserver gedacht.

## Erreichbarkeitsprüfung

- Der Launcher prüft die Erreichbarkeit der Favoriten mit einem kurzen TCP-Verbindungsaufbau zum RDP-Port.
- Dabei schickt er den ersten Schritt der RDP-Aushandlung (X.224 Connection Request) und wertet die Antwort aus. Es findet keine Anmeldung statt.
- Auf dem Zielrechner kann das als kurze, abgebrochene Verbindung im Ereignisprotokoll erscheinen.
- Abschaltbar ist die Prüfung zentral und je Favorit (`reach_check`).

## Logs und Verlauf

- Sitzungs-Logs und Verlauf enthalten Hostnamen und Benutzernamen, aber keine Passwörter oder Token.
- Auf der Stufe DEBUG kann FreeRDP sehr ausführlich protokollieren, solche Logs nicht weitergeben.
- Alles bleibt lokal. RMC sendet keine Telemetrie.

## Umleitungen

- Laufwerke und Drucker werden nur umgeleitet, wenn sie ausdrücklich eingestellt oder in der Sitzung gewählt sind. Die automatische Umleitung aller eingehängten Datenträger ist abgeschaltet.
- Die Zwischenablage lässt sich je Verbindung abschalten (Einstellungen oder Zahnrad-Menü).

## Sicherheitslücken melden

Siehe [SECURITY.md](../SECURITY.md).
