//! Fixture verification: decode with ffmpeg (software or CUDA/NVDEC), combine the views
//! with the CPU reference and compare against the re-rendered source pictures.

use std::io::{BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use rdpgfx_model::frame::pad;
use rdpgfx_model::metrics::psnr;
use rdpgfx_model::{ChromaMode, Rect, Yuv420Frame, Yuv444Frame, avc444};

use rdpgfx_fixtures::meta::{Codec, Scenario, Surface};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decoder {
    /// libavcodec software decoder.
    Sw,
    /// NVDEC via `-hwaccel cuda`, frames downloaded as NV12 and converted losslessly to I420.
    Cuda,
}

#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub min: f64,
    pub avg: f64,
}

impl Stats {
    fn from(v: &[f64]) -> Self {
        let finite: Vec<f64> = v
            .iter()
            .map(|x| if x.is_finite() { *x } else { 99.0 })
            .collect();
        Self {
            min: finite.iter().copied().fold(f64::INFINITY, f64::min),
            avg: finite.iter().sum::<f64>() / finite.len().max(1) as f64,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SurfaceReport {
    pub id: u16,
    pub size: (u32, u32),
    pub pictures: usize,
    pub psnr_y: Stats,
    pub psnr_u: Stats,
    pub psnr_v: Stats,
    pub psnr_rgb: Stats,
    /// Loss of view split + combination alone (no codec): shows what the chroma mode and the
    /// fixed-point colour conversion cost (RGB ceiling).
    pub psnr_rgb_without_codec: Stats,
    /// Worst plane of decoded vs. codec-free combination (YUV 4:4:4): codec loss only.
    /// A layout or combination bug drops this to 10-15 dB. Pass/fail criterion.
    pub psnr_vs_reference: Stats,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub scenario: String,
    pub decoder: Decoder,
    /// Minimum of `psnr_vs_reference` over all frames and surfaces.
    pub threshold_db: f64,
    pub passed: bool,
    pub surfaces: Vec<SurfaceReport>,
}

pub fn verify(
    dir: &Path,
    decoder: Decoder,
    threshold_db: f64,
    max_frames: Option<u32>,
) -> Result<Report> {
    let scenario: Scenario = serde_json::from_reader(
        std::fs::File::open(dir.join("scenario.json")).context("scenario.json")?,
    )?;
    let frames = max_frames.map_or(scenario.frames, |m| m.min(scenario.frames));
    let mut surfaces = Vec::new();
    for surf in &scenario.surfaces {
        surfaces.push(verify_surface(dir, &scenario, surf, decoder, frames)?);
    }
    let passed = surfaces
        .iter()
        .all(|s| s.psnr_vs_reference.min >= threshold_db);
    let report = Report {
        scenario: scenario.name.clone(),
        decoder,
        threshold_db,
        passed,
        surfaces,
    };
    let name = match decoder {
        Decoder::Sw => "verify-sw.json",
        Decoder::Cuda => "verify-cuda.json",
    };
    serde_json::to_writer_pretty(std::fs::File::create(dir.join(name))?, &report)?;
    Ok(report)
}

fn verify_surface(
    dir: &Path,
    sc: &Scenario,
    surf: &Surface,
    decoder: Decoder,
    frames: u32,
) -> Result<SurfaceReport> {
    let (pw, ph) = (pad(surf.width), pad(surf.height));
    let pics_per_frame = if sc.codec == Codec::Avc444 { 2 } else { 1 };
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostats", "-loglevel", "error"]);
    match decoder {
        Decoder::Sw => cmd.args(["-threads", "0"]),
        Decoder::Cuda => cmd.args(["-hwaccel", "cuda"]),
    };
    // Raw H.264 has no timestamps: give the real picture rate and pass every picture
    // through, otherwise ffmpeg resamples to 25 fps and drops pictures.
    cmd.args(["-framerate", &(sc.fps * pics_per_frame).to_string(), "-i"])
        .arg(dir.join(&surf.stream))
        .args(["-fps_mode", "passthrough"])
        .args(["-frames:v", &(frames * pics_per_frame).to_string()])
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().context("start ffmpeg")?;
    let mut out = BufReader::with_capacity(1 << 24, child.stdout.take().expect("stdout"));
    let pic_size = Yuv420Frame::i420_size(pw, ph);
    let mut buf = vec![0u8; pic_size];
    let mut read_pic = |out: &mut BufReader<_>| -> Result<Yuv420Frame> {
        out.read_exact(&mut buf).context("short read from ffmpeg")?;
        Ok(Yuv420Frame::from_i420(&buf, pw, ph))
    };

    let rect = Rect::from_xywh(surf.origin.0, surf.origin.1, surf.width, surf.height);
    let all = [Rect::new(0, 0, pw, ph)];
    let reverse = sc.codec == Codec::Avc444 && sc.chroma == ChromaMode::Box;
    let (mut py, mut pu, mut pv, mut prgb, mut pideal, mut pref) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    let mut state = Yuv444Frame::new(surf.width, surf.height);
    for frame in 0..frames {
        let rgb = sc.scene.render(frame, rect);
        let src = Yuv444Frame::from_rgb(&rgb, surf.width, surf.height);

        let main = read_pic(&mut out)?;
        avc444::apply_main(&mut state, &main, &all);
        if sc.codec == Codec::Avc444 {
            let aux = read_pic(&mut out)?;
            avc444::apply_aux(&mut state, sc.avc444_layout, &aux, &all);
        }
        let dec = if reverse {
            avc444::resolve_reverse_filter(&state)
        } else {
            state.clone()
        };

        // Codec-free reference: split + combine of the source itself.
        let ideal = {
            let chroma = if sc.codec == Codec::Avc444 {
                sc.chroma
            } else {
                ChromaMode::Box
            };
            let (m, a) = avc444::split(&src, sc.avc444_layout, chroma);
            let mut f = Yuv444Frame::new(surf.width, surf.height);
            avc444::apply_main(&mut f, &m, &all);
            if sc.codec == Codec::Avc444 {
                avc444::apply_aux(&mut f, sc.avc444_layout, &a, &all);
            }
            if reverse {
                avc444::resolve_reverse_filter(&f)
            } else {
                f
            }
        };

        let n = (pw * surf.height) as usize; // compare visible rows only
        py.push(psnr(&dec.y[..n], &src.y[..n]));
        pu.push(psnr(&dec.u[..n], &src.u[..n]));
        pv.push(psnr(&dec.v[..n], &src.v[..n]));
        prgb.push(psnr(&dec.to_rgb(), &rgb));
        pideal.push(psnr(&ideal.to_rgb(), &rgb));
        pref.push(
            psnr(&dec.y[..n], &ideal.y[..n])
                .min(psnr(&dec.u[..n], &ideal.u[..n]))
                .min(psnr(&dec.v[..n], &ideal.v[..n])),
        );
    }
    drop(out);
    let _ = child.kill();
    let _ = child.wait();
    if prgb.is_empty() {
        bail!("no frames verified");
    }
    Ok(SurfaceReport {
        id: surf.id,
        size: (surf.width, surf.height),
        pictures: (frames * pics_per_frame) as usize,
        psnr_y: Stats::from(&py),
        psnr_u: Stats::from(&pu),
        psnr_v: Stats::from(&pv),
        psnr_rgb: Stats::from(&prgb),
        psnr_rgb_without_codec: Stats::from(&pideal),
        psnr_vs_reference: Stats::from(&pref),
    })
}
