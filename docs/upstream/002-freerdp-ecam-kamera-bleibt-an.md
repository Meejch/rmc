# Upstream-Notiz 002: Kamera bleibt nach dem Videoanruf an (rdpecam, H.264)

Stand 25.11.2026, geprüft mit FreeRDP 3.33.1 und master `5379388d4`. Status: **festgehalten, noch nicht gemeldet.** Lokal behoben mit `tools/a1/patches/3.33.1-ecam-idle-release.patch`.

## Beobachtung

Nach einem Teams- oder Webex-Anruf mit Kamera schickt der Client keine Bilder mehr. Die Kamera-LED bleibt trotzdem an, bis die RDP-Verbindung getrennt wird. Die Kamera ist eine Logitech C920, ihr H.264 wird durchgereicht. Mit `mstsc` war das Verhalten gleich.

## Ursache

- Windows hört nach dem Anruf auf, Sample Requests zu schicken. Ein Stop Streams Request kommt nicht, der Stream bleibt aus Sicht des Servers offen.
- Für diesen Fall gibt FreeRDP das Gerät frei, wenn 2 s lang kein Sample Request kam (`ECAM_CAPTURE_IDLE_TIMEOUT_MS` in `ecam_dev_sample_captured_callback`). Diese Prüfung steht aber nur am Anfang des Callbacks.
- H.264-Samples dürfen nicht verworfen werden (`mediaSupportDrops`). Liegt noch ein Sample bereit, wartet der Capture-Thread in `while (stream->haveSample && stream->streaming)` auf den nächsten Sample Request. Kommt keiner mehr, wartet er für immer.
- Die Leerlaufprüfung wird dadurch nie wieder erreicht. Der v4l-Stream bleibt mit `VIDIOC_STREAMON` offen, die Kamera bleibt an.
- Nachweis am laufenden Prozess: Der Capture-Thread schläft im Takt von 33 ms (`SleepEx(waitDelay)` bei 30 fps) und macht etwa 30 Wechsel pro Sekunde. Die CPU-Zeit steigt nicht, und `/dev/video0` bleibt offen.

## Fix

Die Leerlaufprüfung läuft auch in der Warteschleife. Nach 2 s ohne Sample Request wird das wartende Sample verworfen und `ECAM_SAMPLE_CAPTURE_DRAINED` zurückgegeben. Der v4l-Teil beendet dann den Stream und schließt das Gerät. Der nächste Sample Request startet die Aufnahme über den vorhandenen Weg in `ecam_dev_process_sample_request` neu. Das alte Sample wird nicht nachgeschickt, weil es nach dem Neustart nicht mehr zum Strom passt.
