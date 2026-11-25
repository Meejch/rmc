# Fixtures: synthetische AVC420/AVC444-Streams

Testdaten für den GPU-Grafikkern (siehe [../docs/architektur.md](../docs/architektur.md)). Die Streams werden reproduzierbar erzeugt und nicht versioniert (`fixtures/out/`).

```
fixtures/generate-all.sh          # Standardsatz erzeugen und prüfen (120 Frames ≈ 2 s)
fixtures/generate-all.sh 600      # länger, z. B. für Dauerläufe
```

## Standardsatz

| Name | Surfaces | Codec | Besonderheit |
|---|---|---|---|
| `multi-v1-box` | 3 (1440×2560, 3440×1440, 2560×1440) | AVC444 v1 | Chroma 2×2 gemittelt (Spec-Modell, Windows), Rückfilter nötig |
| `multi-v2-box` | 3 | AVC444v2 | wie oben, v2-Layout |
| `multi-v1-sample` | 3 | AVC444 v1 | Chroma als Einzelwert (FreeRDP-Encoder), ohne Filter exakt |
| `multi-v1-video` | 3 | AVC444 v1 | zusätzlich großer Rausch-Bereich (Encoder-Worst-Case) |
| `multi-avc420` | 3 | AVC420 | Vergleich: farbiger Text verliert schon ohne Codec |
| `single-v1-box` | 1 (7440×2560) | AVC444 v1 | eine Surface über die ganze Bounding-Box |
| `single-avc420` | 1 (7440×2560) | AVC420 | |

Die Monitore liegen wie bei einem typischen Drei-Monitor-Arbeitsplatz: Hochformat links (0,0), Mitte primär (1440,546), rechts (4880,546).

## Aufbau eines Fixtures

- `scenario.json`: Codec, Layout, Chroma-Modus, Bildrate, Szene (Seed), Ausgänge, Surfaces mit Lage (`origin`), Encoder-Parameter.
- `surface-N.h264`: **ein** H.264-Strom pro Surface (Annex B, mit AUD). Bei AVC444 wechseln Haupt- und Hilfsansicht Bild für Bild, so wie ein Decoder pro Surface sie bekommt.
- `surface-N.jsonl`: ein Update pro Zeile mit Ziel-Rechteck, LC, und je Ansicht Byte-Offset, Länge und Regionsrechtecke im Strom.
- `verify-sw.json` / `verify-cuda.json`: Ergebnis der letzten Prüfung.

Inhalt: synthetischer Desktop mit Fenstern, farbigem Pseudo-Text, einem wandernden Fenster und einem scrollenden Bereich. Alles leitet sich aus (Seed, Frame, x, y) ab. Der Prüfer rendert die Quelle deshalb neu, statt sie zu speichern.

## Prüfungen

| Befehl | Prüft |
|---|---|
| `gen-fixtures verify <dir>` | Software-Decode → Kombination (CPU-Referenz) → Vergleich mit der codec-freien Referenz (schwächste YUV-Ebene, Schwelle 30 dB, Video-Fixture 24 dB) |
| `gen-fixtures verify <dir> --decoder cuda` | dasselbe mit NVDEC |
| `gen-fixtures xcheck <dir>` | Kombination gegen FreeRDPs `YUV420CombineToYUV444` (Hilfsprogramm: `tools/xcheck/build.sh`), muss identisch sein |

## Ergebnisse 22.11.2026 (CRF 16, 120 Frames)

- Alle sieben Fixtures bestehen. Codec-Verlust gegenüber der Referenz: 31–45 dB, Video-Fixture 27 dB.
- **NVDEC ist bitgleich** zum Software-Decoder (md5 der Rohausgabe, alle drei Surfaces).
- **FreeRDP-Abgleich identisch** für v1 und v2 (0 abweichende Samples in Y/U/V).
- RGB-Obergrenze ohne Codec: rund 42 dB im Sample-Modus (Festkomma-Farbformel der Spezifikation) und 37–41 dB im Box-Modus (Rückfilter mit Schwelle 30). AVC420 erreicht bei farbigem Text nur 25–30 dB, genau dafür gibt es AVC444.
