//! `gpu-check`: runs fixture streams through the GPU core (NVDEC -> Vulkan compute ->
//! surface image) and compares every frame with the CPU reference (`SurfaceModel`), byte by
//! byte.
//!
//!   gpu-check fixtures/out/multi-v1-box [--frames N] [--reverse on|off|auto] [--regions]
//!   gpu-check fixtures/out/multi-v1-box --bench 15

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};

use rdpgfx_fixtures::annexb::{AccessUnit, split_access_units};
use rdpgfx_fixtures::meta::{Codec, Scenario};
use rdpgfx_gpu::core::{AvcUpdate, AvcView, CoreOptions, GpuCore};
use rdpgfx_model::frame::pad;
use rdpgfx_model::{ChromaMode, PixelFormat, Rect, SurfaceModel, SurfaceOp, Yuv420Frame};

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Reverse {
    /// On for AVC444 with averaged chroma (box), off otherwise.
    Auto,
    On,
    Off,
}

#[derive(Parser)]
#[command(about = "Compare the GPU core (NVDEC + Vulkan compute) with the CPU reference")]
struct Args {
    dir: PathBuf,
    #[arg(long)]
    frames: Option<u32>,
    #[arg(long, value_enum, default_value = "auto")]
    reverse: Reverse,
    /// After the first frame, apply updates only inside moving partial rectangles with odd
    /// edges (region semantics of MS-RDPEGFX); the whole surface is still compared.
    #[arg(long)]
    regions: bool,
    /// Resource measurement instead of comparison: all surfaces, paced at the scenario's
    /// frame rate, looping for this many seconds (no CPU reference, no readback).
    #[arg(long)]
    bench: Option<u64>,
    /// CUDA device for NVDEC.
    #[arg(long, default_value = "0")]
    device: String,
    /// Decoder backend: auto, nvidia, vaapi, software.
    #[arg(long, default_value = "auto")]
    backend: String,
    /// VA-API render node (default: the first that decodes H.264).
    #[arg(long)]
    vaapi_device: Option<String>,
    /// Decode the surfaces of a frame one after the other (default: in parallel).
    #[arg(long)]
    serial_decode: bool,
}

struct Stream {
    id: u16,
    width: u32,
    height: u32,
    data: Vec<u8>,
    aus: Vec<AccessUnit>,
}

impl Stream {
    fn au(&self, i: usize) -> &[u8] {
        let au = &self.aus[i];
        &self.data[au.offset..][..au.len]
    }
}

fn load(args: &Args, sc: &Scenario, per_frame: usize, frames: u32) -> Result<Vec<Stream>> {
    sc.surfaces
        .iter()
        .map(|s| {
            let data = std::fs::read(args.dir.join(&s.stream))?;
            let aus = split_access_units(&data);
            ensure!(aus.len() >= frames as usize * per_frame, "stream too short");
            Ok(Stream {
                id: s.id,
                width: s.width,
                height: s.height,
                data,
                aus,
            })
        })
        .collect()
}

fn main() -> Result<()> {
    let args = Args::parse();
    let sc: Scenario = serde_json::from_reader(
        std::fs::File::open(args.dir.join("scenario.json")).context("scenario.json")?,
    )?;
    let frames = args.frames.map_or(sc.frames, |f| f.min(sc.frames));
    let is444 = sc.codec == Codec::Avc444;
    let per_frame: usize = if is444 { 2 } else { 1 };
    let reverse = match args.reverse {
        Reverse::On => true,
        Reverse::Off => false,
        Reverse::Auto => is444 && sc.chroma == ChromaMode::Box,
    };
    let mut core = GpuCore::new(&CoreOptions {
        device: args.device.clone(),
        backend: rdpgfx_gpu::core::BackendChoice::parse(&args.backend)
            .ok_or_else(|| anyhow::anyhow!("unknown backend {}", args.backend))?,
        vaapi_device: args.vaapi_device.clone(),
        reverse_filter: reverse,
        parallel_decode: !args.serial_decode,
        ..Default::default()
    })?;
    println!("Grafik: {}", core.backend_info().summary());
    if let Some(secs) = args.bench {
        let streams = load(&args, &sc, per_frame, sc.frames)?;
        return bench(&mut core, &sc, &streams, per_frame, secs);
    }
    let streams = load(&args, &sc, per_frame, frames)?;
    core.set_capture(true);
    let mut model = SurfaceModel::default();
    for s in &streams {
        let op = SurfaceOp::Create {
            id: s.id,
            width: s.width,
            height: s.height,
            format: PixelFormat::Xrgb8888,
        };
        core.apply(&op)?;
        model.apply(&op)?;
    }

    let mut mismatches = vec![(0usize, 0u8); streams.len()];
    for f in 0..frames as usize {
        // All surfaces of a frame go through `avc_frame` (parallel decode by default).
        let rects: Vec<Vec<Rect>> = streams
            .iter()
            .map(|s| {
                if args.regions && f > 0 {
                    region_rects(f as u32, s.width, s.height)
                } else {
                    vec![Rect::new(0, 0, s.width, s.height)]
                }
            })
            .collect();
        let updates: Vec<AvcUpdate<'_>> = streams
            .iter()
            .zip(&rects)
            .map(|(s, r)| AvcUpdate {
                id: s.id,
                layout: sc.avc444_layout,
                main: Some(AvcView {
                    au: s.au(f * per_frame),
                    rects: r,
                }),
                aux: is444.then(|| AvcView {
                    au: s.au(f * per_frame + 1),
                    rects: r,
                }),
            })
            .collect();
        core.avc_frame(&updates)?;
        for (s, rects) in streams.iter().zip(&rects) {
            let cap = core.take_captured(s.id);
            let (pw, ph) = (pad(s.width), pad(s.height));
            let mv = Yuv420Frame::from_i420(&cap.main.context("main view not captured")?, pw, ph);
            let av = cap.aux.map(|a| Yuv420Frame::from_i420(&a, pw, ph));
            model.avc(
                s.id,
                sc.avc444_layout,
                Some((&mv, rects)),
                av.as_ref().map(|a| (a, rects.as_slice())),
                reverse,
            )?;
        }
        core.end_frame()?;
        for (k, s) in streams.iter().enumerate() {
            let gpu = core.read_surface(s.id)?;
            let cpu = &model.surface(s.id)?.pixels;
            for (px, (g, c)) in gpu
                .as_chunks::<4>()
                .0
                .iter()
                .zip(cpu.as_chunks::<4>().0)
                .enumerate()
            {
                if g != c {
                    let d = (0..4).map(|i| g[i].abs_diff(c[i])).max().unwrap_or(0);
                    if mismatches[k].0 < 3 {
                        let (x, y) = (px as u32 % s.width, px as u32 / s.width);
                        eprintln!(
                            "  Abweichung Surface {} Frame {f} bei ({x},{y}): GPU {g:?} CPU {c:?} (BGRA)",
                            s.id
                        );
                    }
                    mismatches[k].0 += 1;
                    mismatches[k].1 = mismatches[k].1.max(d);
                }
            }
        }
    }
    let mut all_ok = true;
    for (s, (n, max)) in streams.iter().zip(&mismatches) {
        let ok = *n == 0;
        all_ok &= ok;
        println!(
            "surface {} {}x{}: {} Frames{}, {} abweichende Pixel (max {}), Rückfilter {} -> {}",
            s.id,
            s.width,
            s.height,
            frames,
            if args.regions {
                " mit Teilregionen"
            } else {
                ""
            },
            n,
            max,
            if reverse { "an" } else { "aus" },
            if ok { "bitgleich" } else { "ABWEICHUNG" }
        );
    }
    println!("{}: {}", sc.name, if all_ok { "OK" } else { "FEHLER" });
    if !all_ok {
        std::process::exit(1);
    }
    Ok(())
}

/// Two moving rectangles with odd edges, partly overlapping, one touching the surface edge.
fn region_rects(f: u32, w: u32, h: u32) -> Vec<Rect> {
    let a = Rect::from_xywh(
        (13 + f * 37) % (w / 2),
        (9 + f * 23) % (h / 2),
        w / 3 + 1,
        h / 4 + 3,
    );
    let b = Rect::new(
        w - w / 5 - (f % 7),
        (f * 11) % (h / 3) + 1,
        w,
        h / 3 + 5 + (f % 5),
    );
    [a, b]
        .into_iter()
        .filter_map(|r| r.intersect(&Rect::new(0, 0, w, h)))
        .collect()
}

/// CPU time (user + system) of this process in seconds.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = stat.rsplit(')').next().unwrap_or("");
    let f: Vec<&str> = after.split_whitespace().collect();
    let ticks: f64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0)
        + f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    ticks / 100.0
}

fn bench(
    core: &mut GpuCore,
    sc: &Scenario,
    streams: &[Stream],
    per_frame: usize,
    secs: u64,
) -> Result<()> {
    let is444 = per_frame == 2;
    for s in streams {
        core.create_surface(s.id, s.width, s.height, PixelFormat::Xrgb8888)?;
    }
    let frames = sc.frames as usize;
    let interval = Duration::from_secs_f64(1.0 / f64::from(sc.fps));
    let start = Instant::now();
    let cpu0 = cpu_seconds();
    let (mut n, mut late, mut busy) = (0u64, 0u64, Duration::ZERO);
    while start.elapsed() < Duration::from_secs(secs) {
        let deadline = start + interval * (n as u32 + 1);
        let t = Instant::now();
        let f = (n as usize) % frames; // frame 0 is an IDR, so looping restarts cleanly
        let full: Vec<[Rect; 1]> = streams
            .iter()
            .map(|s| [Rect::new(0, 0, s.width, s.height)])
            .collect();
        let updates: Vec<AvcUpdate<'_>> = streams
            .iter()
            .zip(&full)
            .map(|(s, full)| AvcUpdate {
                id: s.id,
                layout: sc.avc444_layout,
                main: Some(AvcView {
                    au: s.au(f * per_frame),
                    rects: full,
                }),
                aux: is444.then(|| AvcView {
                    au: s.au(f * per_frame + 1),
                    rects: full,
                }),
            })
            .collect();
        core.avc_frame(&updates)?;
        core.end_frame()?;
        busy += t.elapsed();
        n += 1;
        let now = Instant::now();
        if now > deadline {
            late += 1;
        } else {
            std::thread::sleep(deadline - now);
        }
    }
    let wall = start.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu0;
    println!(
        "{}: {} Surfaces, {} Frames in {:.1} s ({:.1} fps), Arbeit {:.2} ms/Frame, {} Frames zu spät, CPU {:.1} % eines Kerns",
        sc.name,
        streams.len(),
        n,
        wall,
        n as f64 / wall,
        busy.as_secs_f64() * 1000.0 / n as f64,
        late,
        cpu / wall * 100.0
    );
    print!("{}", core.profile.take_report(n));
    Ok(())
}
