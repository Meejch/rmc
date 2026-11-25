//! Cross-check of the CPU reference combination against FreeRDP's own
//! `YUV420CombineToYUV444` (helper program from tools/xcheck).
//!
//! FreeRDP 3.x applies no reverse chroma filter, so the comparison runs on the combined
//! planes before any filter: they must be identical.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};

use rdpgfx_model::frame::pad;
use rdpgfx_model::{Avc444Layout, Rect, Yuv420Frame, Yuv444Frame, avc444};

use rdpgfx_fixtures::meta::{Codec, Scenario};

pub struct PlaneDiff {
    pub mismatches: usize,
    pub max_diff: u8,
}

pub fn xcheck(dir: &Path, tool: &Path, frames: u32) -> Result<bool> {
    let sc: Scenario = serde_json::from_reader(std::fs::File::open(dir.join("scenario.json"))?)?;
    ensure!(sc.codec == Codec::Avc444, "xcheck needs an AVC444 scenario");
    let mut all_ok = true;
    for surf in &sc.surfaces {
        let (pw, ph) = (pad(surf.width), pad(surf.height));
        let frames = frames.min(sc.frames);
        let pics = decode(dir, &surf.stream, sc.fps * 2, frames * 2, pw, ph)?;

        // FreeRDP side.
        let layout = match sc.avc444_layout {
            Avc444Layout::V1 => "v1",
            Avc444Layout::V2 => "v2",
        };
        let mut child = Command::new(tool)
            .args([pw.to_string(), ph.to_string(), layout.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("start {}", tool.display()))?;
        let mut stdin = child.stdin.take().expect("stdin");
        let feed: Vec<u8> = pics.iter().flat_map(|p| p.to_i420()).collect();
        let writer = std::thread::spawn(move || stdin.write_all(&feed));
        let mut theirs = Vec::new();
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_end(&mut theirs)?;
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("writer panicked"))??;
        ensure!(child.wait()?.success(), "freerdp-combine failed");
        let n = (pw * ph) as usize;
        ensure!(
            theirs.len() == frames as usize * n * 3,
            "unexpected output size from freerdp-combine"
        );

        // Our side, frame by frame, compared on the visible area.
        let mut state = Yuv444Frame::new(surf.width, surf.height);
        let full = [Rect::new(0, 0, pw, ph)];
        let mut diffs = [
            PlaneDiff {
                mismatches: 0,
                max_diff: 0,
            },
            PlaneDiff {
                mismatches: 0,
                max_diff: 0,
            },
            PlaneDiff {
                mismatches: 0,
                max_diff: 0,
            },
        ];
        for f in 0..frames as usize {
            avc444::apply_main(&mut state, &pics[2 * f], &full);
            avc444::apply_aux(&mut state, sc.avc444_layout, &pics[2 * f + 1], &full);
            let base = f * n * 3;
            for (k, ours) in [&state.y, &state.u, &state.v].into_iter().enumerate() {
                let t = &theirs[base + k * n..base + (k + 1) * n];
                for row in 0..surf.height as usize {
                    let r = row * pw as usize..row * pw as usize + surf.width as usize;
                    for (a, b) in ours[r.clone()].iter().zip(&t[r]) {
                        if a != b {
                            diffs[k].mismatches += 1;
                            diffs[k].max_diff = diffs[k].max_diff.max(a.abs_diff(*b));
                        }
                    }
                }
            }
        }
        let ok = diffs.iter().all(|d| d.mismatches == 0);
        all_ok &= ok;
        println!(
            "surface {} {}x{} ({frames} frames, {layout}): Y {} / U {} / V {} abweichende Samples (max {}/{}/{}) -> {}",
            surf.id,
            surf.width,
            surf.height,
            diffs[0].mismatches,
            diffs[1].mismatches,
            diffs[2].mismatches,
            diffs[0].max_diff,
            diffs[1].max_diff,
            diffs[2].max_diff,
            if ok { "identisch" } else { "ABWEICHUNG" }
        );
    }
    Ok(all_ok)
}

fn decode(
    dir: &Path,
    stream: &str,
    rate: u32,
    pictures: u32,
    pw: u32,
    ph: u32,
) -> Result<Vec<Yuv420Frame>> {
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostats",
            "-loglevel",
            "error",
            "-framerate",
            &rate.to_string(),
            "-i",
        ])
        .arg(dir.join(stream))
        .args([
            "-fps_mode",
            "passthrough",
            "-frames:v",
            &pictures.to_string(),
        ])
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
        .output()
        .context("ffmpeg decode")?;
    let size = Yuv420Frame::i420_size(pw, ph);
    ensure!(
        out.stdout.len() == size * pictures as usize,
        "decoded {} bytes, expected {}",
        out.stdout.len(),
        size * pictures as usize
    );
    Ok(out
        .stdout
        .chunks_exact(size)
        .map(|c| Yuv420Frame::from_i420(c, pw, ph))
        .collect())
}

/// Compares FreeRDP's AVC444 encoder primitive (freerdp-split helper) with the spec split
/// of the CPU reference, and reconstructs FreeRDP's views with the reference combination.
pub fn xsplit(ppm: &Path, tool: &Path, layout: Avc444Layout, implementation: &str) -> Result<()> {
    use rdpgfx_model::metrics::psnr;
    use rdpgfx_model::{ChromaMode, avc444};

    let data = std::fs::read(ppm)?;
    let (w, h, rgb) = parse_ppm(&data)?;
    let (pw, ph) = (pad(w), pad(h));
    let lay = match layout {
        Avc444Layout::V1 => "v1",
        Avc444Layout::V2 => "v2",
    };
    let out = Command::new(tool)
        .arg(ppm)
        .args([lay, implementation])
        .output()
        .context("run freerdp-split")?;
    ensure!(
        out.status.success(),
        "freerdp-split failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let size = Yuv420Frame::i420_size(pw, ph);
    ensure!(
        out.stdout.len() == 2 * size,
        "unexpected freerdp-split output size"
    );
    let fr_main = Yuv420Frame::from_i420(&out.stdout[..size], pw, ph);
    let fr_aux = Yuv420Frame::from_i420(&out.stdout[size..], pw, ph);

    let src = Yuv444Frame::from_rgb(rgb, w, h);
    let all = [Rect::new(0, 0, pw, ph)];
    println!(
        "FreeRDP RGBToAVC444YUV{} ({implementation}) gegen Spec-Split, {w}x{h}:",
        if lay == "v2" { "v2" } else { "" }
    );
    for chroma in [ChromaMode::Sample, ChromaMode::Box] {
        let (m, a) = avc444::split(&src, layout, chroma);
        let p = |x: &[u8], y: &[u8]| psnr(x, y);
        println!(
            "  vs {chroma:?}: main Y {:.1} U {:.1} V {:.1} | aux Y {:.1} U {:.1} V {:.1} dB",
            p(&fr_main.y, &m.y),
            p(&fr_main.u, &m.u),
            p(&fr_main.v, &m.v),
            p(&fr_aux.y, &a.y),
            p(&fr_aux.u, &a.u),
            p(&fr_aux.v, &a.v)
        );
    }
    // Swapped-chroma hypothesis for the main view.
    let (m, _) = avc444::split(&src, layout, ChromaMode::Sample);
    println!(
        "  main U gegen Spec-V: {:.1} dB, main V gegen Spec-U: {:.1} dB",
        psnr(&fr_main.u, &m.v),
        psnr(&fr_main.v, &m.u)
    );

    let mut f = Yuv444Frame::new(w, h);
    avc444::apply_main(&mut f, &fr_main, &all);
    avc444::apply_aux(&mut f, layout, &fr_aux, &all);
    for reverse in [false, true] {
        println!(
            "  FreeRDP-Ansichten, Referenz-Kombination{}: RGB {:.2} dB gegen Quelle",
            if reverse { " + Rückfilter" } else { "" },
            psnr(&avc444::to_rgb(&f, reverse), rgb)
        );
    }
    Ok(())
}

fn parse_ppm(data: &[u8]) -> Result<(u32, u32, &[u8])> {
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while data[i].is_ascii_whitespace() {
            i += 1;
        }
        let s = i;
        while !data[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(std::str::from_utf8(&data[s..i])?.to_string());
    }
    ensure!(
        fields[0] == "P6" && fields[3] == "255",
        "only binary 8-bit PPM supported"
    );
    let (w, h): (u32, u32) = (fields[1].parse()?, fields[2].parse()?);
    let px = &data[i + 1..];
    ensure!(px.len() >= (w * h * 3) as usize, "PPM too short");
    Ok((w, h, &px[..(w * h * 3) as usize]))
}
