# Benutzung

## Launcher

RMC startet ohne Argumente den Launcher, über das Startmenü („RMC“) oder mit `rmc`. Ein zweiter Start holt das vorhandene Fenster nach vorn.

Die Kopfzeile führt zu den vier Bereichen **Verbinden**, **Verlauf**, **Log** und **Einstellungen**.

### Verbinden

**Neue Verbindung**
- Ziel/Host (optional mit Port, `rechner:3390`), Benutzer, Domäne und Passwort.
- „Entra-ID verwenden“ schaltet zwischen der Anmeldung mit Firmenkonto (Entra ID, mit 2FA im Browserfenster) und Benutzer und Passwort um.
- Der Stern speichert die Eingaben als Favorit. „⋮“ öffnet alle Optionen der Verbindung oder leert die Felder.
- Nach einer erfolgreichen Verbindung bietet der Launcher an, das Ziel als Favorit zu speichern.

**Aktuelle Verbindung**
- Name, Zustand, Host, Benutzer, Domäne und die tatsächlich genutzte Grafikdekodierung der laufenden Sitzung, dazu Bearbeiten, Trennen und „Log anzeigen“.
- Mehrere Verbindungen gleichzeitig sind möglich. Sie erscheinen dann als Liste.
- Beendete Sitzungen bleiben mit ihrem Ergebnis stehen, bis „Entfernen“ oder eine neue Verbindung sie ablöst.

**Letzte Verbindungen**
- Die zuletzt verwendeten Ziele. Der Stecker verbindet erneut.

**Favoriten**
- Suche, „Neuer Favorit“, Stift zum Bearbeiten, Stecker zum Verbinden (bei laufender Sitzung: Trennen).
- Doppelklick verbindet. Rechtsklick bietet zusätzlich Duplizieren, „Gespeicherte Anmeldung verwerfen“ und Löschen.
- Die **Statusleuchte** zeigt die Erreichbarkeit:

  | Leuchte | Bedeutung |
  |---|---|
  | grün | Der Rechner antwortet per RDP. |
  | gelb | Der Rechner antwortet, aber nicht per RDP (z. B. noch beim Hochfahren). |
  | grau | nicht erreichbar |
  | Ring | noch nicht geprüft oder Prüfung abgeschaltet |

- Geprüft wird nach dem Öffnen des Launchers zunächst alle 5 Sekunden, später seltener (einstellbar). „⋮“ → „Erreichbarkeit jetzt prüfen“ beginnt wieder mit dem schnellen Takt. Wie geprüft wird, steht in [sicherheit.md](sicherheit.md).

### Verlauf

Jede Verbindung mit Beginn, Dauer und Ergebnis, dazu „Log anzeigen“ und „Erneut verbinden“.

### Log

- Das Log einer Sitzung, aufbereitet nach Zeit, Stufe, Quelle und Text. Fehler sind rot, Warnungen gelb.
- Auswahl der letzten Logs, Filterfeld, „Details“ für FreeRDP-Einzelheiten.
- „Mitlaufen“ folgt einer laufenden Sitzung.

### Einstellungen

- Die Standardwerte für alle Verbindungen. Änderungen gelten sofort.
- In einem Favoriten gilt für jede Option der Standard, bis „Standard verwenden“ abgewählt wird. Dann hat der Favorit einen eigenen Wert.

| Kategorie | Inhalt |
|---|---|
| Launcher | Fenster schließen ins Tray, Erreichbarkeitsprüfung, Log-Ordner |
| Anzeige | Monitore (Klick in der Vorschau), Primärmonitor, Start im Fenstermodus, Verbindungsleiste angepinnt |
| Audio & Geräte | Ton, Mikrofon, Kamera |
| Freigaben | Zwischenablage, Bandbreite für Dateien, Laufwerke (Ordner), Drucker |
| Leistung | Grafikbeschleunigung (Automatisch, NVIDIA, AMD/Intel, CPU) mit Anzeige der erkannten Hardware, AVC420 statt AVC444, Chroma-Rückfilter |
| Erweitert | automatischer Reconnect, Aktiv halten, Tastatur-Grab, Login-Variante, Log-Stufe |

### Tray

- Das X schließt nur das Fenster, der Launcher bleibt im Tray (abschaltbar). Klick auf das Symbol oder ein erneuter Start öffnet ihn wieder.
- Das Tray-Menü bietet Trennen je Verbindung, Favorit verbinden, Launcher öffnen und Beenden.
- Wirklich beendet wird über „Beenden“. Laufende Sitzungen laufen dabei weiter.

## Anmeldung

**Entra ID**
- Die erste Verbindung öffnet ein Login-Fenster für Passwort und 2FA.
- Danach meldet sich RMC still an, bis Entra ID wieder eine Anmeldung verlangt.
- Auch der automatische Reconnect nach einem Netzaussetzer braucht kein Fenster.

**Benutzer und Passwort** (Rechner in einer Domäne oder mit lokalem Konto)
- Beim Verbinden fragt der Launcher nach dem Passwort.
- „Passwort speichern“ legt es nach erfolgreicher Anmeldung sicher ab.
- Lehnt Windows die Anmeldung ab, kommt die Abfrage erneut.

**Zertifikate**
- Ein unbekanntes Zertifikat (bei Rechnern ohne Entra ID üblich: Windows stellt es selbst aus) fragt der Launcher einmal nach.
- Ändert sich ein bekanntes Zertifikat, erscheint eine Warnung mit altem und neuem Fingerabdruck.
- Die regelmäßige Erneuerung des Gerätezertifikats von Entra-ID-Rechnern wird automatisch angenommen.

**Gespeicherte Anmeldungen**
- „Gespeicherte Anmeldung verwerfen“ (Rechtsklick auf einen Favoriten) löscht Token und Passwort dieses Favoriten.
- `rmc --logout` löscht alle.

## In der Sitzung

- **Vollbild** über alle gewählten Monitore. Die Verbindungsleiste sitzt oben am Primärmonitor und ist standardmäßig angepinnt. Nicht angepinnt erscheint sie, wenn die Maus den oberen Rand berührt.
- **Leiste**, von links nach rechts:
  - Pin, Name der Verbindung
  - Stern (nur bei Verbindungen ohne Favorit: als Favorit speichern)
  - Zahnrad, Aktiv halten (Tasse), Tastatur-Grab
  - Minimieren, Fenstermodus/Vollbild, Trennen
- **Zahnrad-Menü** (Escape oder ein Klick daneben schließt es):
  - Ton, Mikrofon und Zwischenablage für diese Verbindung schalten. Ton und Mikrofon werden lokal stummgeschaltet.
  - Ordner als Laufwerk verbinden: Persönlicher Ordner, Downloads, Dokumente, Desktop oder „Anderer Ordner …“. Danach fragt der Launcher, ob das Laufwerk dauerhaft in den Favoriten soll.
  - Drucker dieses Rechners (CUPS) verbinden.
  - „Im Launcher bearbeiten …“ öffnet die Einstellungen des Favoriten.
- **Aktiv halten:** Etwa jede Minute geht eine winzige Mausbewegung an Windows. So springt der Anwesenheitsstatus (etwa in Teams) nicht auf „Abwesend“, auch wenn das Fenster im Hintergrund liegt. Das verhindert auch die automatische Bildschirmsperre.
- **Zwischenablage:**
  - Text, Bilder und Dateien (auch Ordner) in beide Richtungen.
  - Dateien aus Windows landen beim Einfügen zuerst unter `~/.cache/rmc/clipboard/`.
  - Während einer Dateiübertragung zeigt ein Streifen unter der Leiste Prozent und Restzeit, das X bricht ab. Ist die Sitzung minimiert, zeigt KDE den Fortschritt im Benachrichtigungsbereich.
- **Ton, Mikrofon, Kamera:** werden nach Windows umgeleitet. Die Kamera geht nach einem Videoanruf wieder aus.

### Tastenkürzel

Alle mit Strg+Alt:

| Kürzel | Wirkung |
|---|---|
| Enter | Vollbild ↔ Fenstermodus |
| M | Minimieren |
| K | Tastatur-Grab an/aus |
| L | Leiste ein-/ausblenden |
| P | Leiste anpinnen |
| A | Aktiv halten an/aus |
| S | im Fenstermodus: 1:1-Ausschnitt ↔ skalierte Gesamtansicht |
| Q | Trennen |
| Pfeiltasten, Mausrad, Ziehen | im Fenstermodus den Ausschnitt verschieben |

Bei aktivem Tastatur-Grab gehen Kombinationen wie Alt+Tab oder die Windows-Taste an Windows statt an den Linux-Desktop.

## Ohne Launcher

```sh
rmc connect <Favorit>               # Sitzung eines Favoriten
rmc --host rechner --user name      # Sitzung ohne Favorit (Passwort aus RDP_PASSWORD)
rmc --help                          # alle Optionen
```

## Fehlersuche

- **Logs:** Jede Sitzung schreibt ein Log nach `~/.local/state/rmc/logs/`, die letzten 20 bleiben erhalten. Im Launcher unter „Log“.
- **Mehr Details:** Log-Stufe im Favoriten (Erweitert) auf DEBUG stellen. Die Logs werden dann groß und können Kennungen enthalten.
- **Verbindungsabbruch:** Der Grund steht in der letzten Zeile „Getrennt:“ des Logs.
