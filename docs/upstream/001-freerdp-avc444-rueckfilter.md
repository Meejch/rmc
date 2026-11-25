# Upstream-Notiz 001: FreeRDP wendet beim AVC444-Decode keinen Chroma-Rückfilter an

Stand 22.11.2026, geprüft mit FreeRDP 3.33.1 und master `5379388d4`. Status: **festgehalten, noch nicht gemeldet.** Diese Notiz ist die Vorlage für einen späteren Issue oder PR bei FreeRDP.

## Kurzfassung

Bei AVC444 (v1 und v2) trägt die Hauptansicht die Chroma in halber Auflösung. Der Encoder mittelt dafür jeden 2×2-Block (MS-RDPEGFX 3.3.8.3.2). Die Hilfsansicht liefert die drei übrigen Chroma-Werte jedes Blocks. Der Decoder muss den gemittelten Wert an der geraden/geraden Position mit einem Rückfilter wieder in den Originalwert zurückrechnen:

```
U'(2x,2y) = 4·U(2x,2y) − U(2x+1,2y) − U(2x,2y+1) − U(2x+1,2y+1)   (geklemmt auf 0..255)
```

Laut Spezifikation wird der Wert nur übernommen, wenn er um mindestens 30 vom gemittelten Wert abweicht. FreeRDP 3.x lässt den Rückfilter seit 3.11.0 ganz weg. An jeder vierten Chroma-Position bleibt so der Mittelwert stehen. Das kostet messbar Farbschärfe an Kanten, gerade mit FreeRDPs eigenem Encoder (Shadow-Server), der 2×2 mittelt.

## Messwerte

Die Messung läuft ohne H.264, nur die Zerlegung und Kombination. Sie misst also den reinen Effekt des Filters.

- **Werkzeug:** `gen-fixtures xsplit` (Repo-Werkzeug, `crates/rdpgfx-fixtures/src/xcheck.rs`), Hilfsprogramm `tools/xcheck/freerdp-split.c`.
- **Ablauf:** FreeRDP zerlegt ein Standbild des synthetischen Desktops (3440×1440, `gen-fixtures still`) mit `RGBToAVC444YUV` bzw. `RGBToAVC444YUVv2` in Haupt- und Hilfsansicht. Unsere Referenz kombiniert die Ansichten nach Spezifikation, einmal ohne und einmal mit Rückfilter, und vergleicht das Ergebnis mit der Quelle in RGB.

| FreeRDP-Encoder | ohne Rückfilter (Verhalten von FreeRDP 3.x) | mit Rückfilter (Spezifikation) |
|---|---|---|
| `RGBToAVC444YUV`, generisch | 31,06 dB | **36,85 dB** |
| `RGBToAVC444YUV`, SIMD | 30,90 dB | **36,35 dB** |
| `RGBToAVC444YUVv2`, generisch | 31,06 dB | **36,85 dB** |
| `RGBToAVC444YUVv2`, SIMD | 30,90 dB | **36,35 dB** |

Zur Einordnung:

- Die Obergrenze in RGB liegt wegen der Festkomma-Farbformel bei rund 42 dB.
- Der Filter bringt etwa 5,5 bis 6 dB.
- Die Hauptansicht von FreeRDPs Encoder entspricht der 2×2-gemittelten Spezifikations-Zerlegung (Chroma 57–58 dB generisch, 48–49 dB SIMD) und nicht der Zerlegung mit Punktabtastung (27 dB). Der Encoder mittelt also tatsächlich.

Im Rauchtest gegen den Shadow-Server verdeckt die niedrige Bitrate des Servers diesen Effekt. Bei normaler Bitrate wird er sichtbar.

## Gegenprobe mit H.264 bei niedriger Bitrate (22.11.2026)

Mit dem eigenen Client gegen den FreeRDP-Shadow-Server gemessen. Der Server encodiert mit libx264 bei fest 10 Mbit/s, ein Standbild kommt also mit rund 330 kbit. Gemessen wurde mit `tools/m1/shadow-e2e.sh` bzw. abwechselnden Läufen nach einer Aufwärmverbindung, in Klammern der Blau-Kanal:

| Decoder | PSNR gegen den Server-Bildschirm |
|---|---|
| eigener Client mit Rückfilter | 22,52 dB (B 20,89) |
| eigener Client ohne Rückfilter | **23,40 dB** (B 21,84) |
| FreeRDP `sdl-freerdp` 3.33.1 (ohne Rückfilter) | 20,56 dB (B 20,90) |

Folgerungen:

- **Bei starker Quantisierung schadet der Rückfilter.** Die Formel `4·avg − rechts − unten − diagonal` verstärkt den Codierfehler, und die Schwelle 30 fängt das nicht ab. Ohne Codec bringt der Filter +5,9 dB, bei sehr niedriger Bitrate kostet er 0,9 dB.
- **Der Filter muss deshalb schaltbar bleiben.** Für einen Upstream-Vorschlag gehört dazu eine Regel, wann er greift (z. B. abhängig vom QP der Region aus den Metadaten `quantQualityVals`). Erst messen, dann vorschlagen.
- **Ob der Filter beim Windows-Encoder mit normaler Bitrate hilft**, entscheidet der Live-Test. Unser Client hat dafür den Schalter `--no-reverse-filter`.
- **Unabhängig vom Filter liegt FreeRDP 2–3 dB unter unserem Client**, vor allem im Rot-Kanal (V-Ebene: 18,3 dB gegen 21,9/22,9 dB). Die Ursache ist nicht untersucht. Kandidat für eine eigene Notiz.

## Betroffene Stellen im FreeRDP-Code

**Decoder (fehlender Filter), 3.33.1:**

- `libfreerdp/primitives/prim_YUV.c`:
  - `general_LumaToYUV444` kopiert die Chroma der Hauptansicht in alle vier Positionen jedes 2×2-Blocks.
  - `general_ChromaV1ToYUV444` (ab Zeile 97) und `general_ChromaV2ToYUV444` (ab Zeile 173) überschreiben danach nur die drei ungeraden Positionen. Die gerade/gerade Position behält den Mittelwert.
  - Aufruf über `general_YUV420CombineToYUV444` (Zeile 225).
- Dasselbe gilt für die SIMD-Varianten:
  - `libfreerdp/primitives/sse/prim_YUV_sse4.1.c`: `sse41_YUV420CombineToYUV444`, Zeile 1767.
  - `libfreerdp/primitives/neon/prim_YUV_neon.c`: `neon_YUV420CombineToYUV444`, Zeile 1252.
- Aufrufer: `libfreerdp/codec/yuv.c`, Zeile 514 (pro Region und Ansicht).

**Entfernung des alten Filters:**

- Commit `901cf28d1` „[primitives,yuv] remove broken chroma filter“ vom 05.01.2025, erstmals in 3.11.0.
- Entfernt wurden `general_ChromaFilter` in `prim_YUV.c` sowie die SSSE3- und NEON-Gegenstücke, zusammen 226 Zeilen.
- Der Filter in 2.11.8 war tatsächlich fehlerhaft:
  - **Falsche Position bei Regionen:** Die Schleifen laufen von `roi->top` bzw. `roi->left` bis `+ halfHeight` bzw. `+ halfWidth`, adressieren aber Zeile/Spalte `2·y` bzw. `2·x`. Bei einer Region mit `top > 0` wird also ein falscher Bereich gefiltert.
  - **Gemischte Grenzen:** Die Grenzprüfung `val2y1 > nHeight` vergleicht eine absolute Zeile mit der Regionshöhe.
  - **Kein Schwellwert:** Die Schwelle 30 aus der Spezifikation fehlt. Der Filter wird immer angewendet.

  Statt den Filter zu korrigieren, wurde er entfernt.

**Encoder (zur Einordnung):**

- `libfreerdp/primitives/prim_YUV.c`, `BGRX_fillYUV` (Zeile 672) und `fillYUV` (Zeile 785), jeweils Abschnitt „Apply chroma filter“.
- Der BGRX-Pfad nimmt den Mittelwert über `CONDITIONAL_CLIP` (`prim_internal.h:215`) nur, wenn er um mindestens 30 vom Originalwert abweicht. Der allgemeine Pfad mittelt immer.
- Das ist uneinheitlich, betrifft den Decode-Fehler aber nicht.

## Vorschlag für den Fix

1. Den Rückfilter nach Spezifikation wieder einführen, mit Schwelle 30 und Klemmung, und nur für gerade/gerade Positionen innerhalb der aktualisierten Region. Die Region-Koordinaten müssen absolut gerechnet werden.
2. **Nicht destruktiv:** Den Filter erst bei der Farbumrechnung anwenden, nicht in den gespeicherten YUV444-Zustand schreiben.
   - Grund: Kommt danach ein Frame nur mit Hauptansicht (LC = 1), würde ein im Zustand gespeicherter Filter sonst doppelt oder auf veraltete Hilfswerte wirken.
   - So arbeiten unsere CPU-Referenz (`rdpgfx-model`: `avc444::to_rgb(…, reverse)`, `reverse_filter`) und der Shader (`yuv444_to_rgba.comp`).
3. Die Anwendung schaltbar machen, z. B. per Einstellung oder Caps-abhängig. Ob der Windows-Encoder mittelt, ist ungeklärt. Bei Punktabtastung würde der Filter das Bild leicht verschlechtern.
4. Testfall: `TestPrimitivesYUV` um einen Rundlauf `RGBToAVC444YUV` → `YUV420CombineToYUV444` → RGB mit PSNR-Schwelle erweitern. Die Werte oben zeigen die erwartete Größenordnung.

## Offene Punkte vor dem Melden

- Live gegen einen Windows-Host prüfen, ob dessen Encoder mittelt und ob der Filter bei dessen Bitrate hilft. Das geht erst, wenn der Zugang zum Ziel-Laptop wieder möglich ist.
- Eine Regel ausarbeiten, wann der Filter greifen soll (siehe Gegenprobe oben).
- Im FreeRDP-Tracker nach bestehenden Issues zu „chroma filter“ und „AVC444 color“ suchen.
- Für die Meldung keine Zielkennungen und keine Personennamen verwenden.
