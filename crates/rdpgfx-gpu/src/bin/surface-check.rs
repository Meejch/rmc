//! `surface-check`: random RDPGFX command sequences against the GPU core and the CPU
//! reference (`SurfaceModel`) at the same time; surfaces are compared byte for byte and both
//! sides must reject the same invalid commands.
//!
//!   surface-check fixtures/out/multi-v2-box [--steps 600] [--seed 1] [--compare-every 10]
//!
//! The fixture provides the AVC surfaces (one H.264 stream each). Two small extra surfaces
//! (odd sizes, ARGB, one repeatedly deleted and recreated) take part in fills, copies, the
//! cache and uploads. AVC updates use different rectangle sets for the main and auxiliary
//! view; an empty set stands in for luma-only/chroma-only updates (LC = 1/2) without
//! breaking the reference chain of the stream.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;

use rdpgfx_fixtures::annexb::{AccessUnit, split_access_units};
use rdpgfx_fixtures::meta::{Codec, Scenario};
use rdpgfx_gpu::core::{AvcView, CoreOptions, GpuCore};
use rdpgfx_model::frame::pad;
use rdpgfx_model::surface::SurfaceError;
use rdpgfx_model::{ChromaMode, Color, PixelFormat, Rect, SurfaceModel, SurfaceOp, Yuv420Frame};

#[derive(Parser)]
#[command(about = "Random surface command sequences: GPU core against the CPU reference")]
struct Args {
    dir: PathBuf,
    #[arg(long, default_value_t = 600)]
    steps: u32,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Compare all surfaces after every N steps (and at the end).
    #[arg(long, default_value_t = 10)]
    compare_every: u32,
    /// Cache slots used by the test (small, so slots get replaced often).
    #[arg(long, default_value_t = 48)]
    slots: u16,
    #[arg(long, default_value = "0")]
    device: String,
    /// Decoder backend: auto, nvidia, vaapi, software.
    #[arg(long, default_value = "auto")]
    backend: String,
    /// VA-API render node (default: the first that decodes H.264).
    #[arg(long)]
    vaapi_device: Option<String>,
}

/// Small deterministic generator (PCG-style LCG), no external crate needed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    /// Rectangle inside `w`x`h` (sometimes empty, sometimes the whole surface).
    fn rect_in(&mut self, w: u32, h: u32) -> Rect {
        if self.chance(5) {
            return Rect::new(0, 0, w, h);
        }
        let x = self.below(u64::from(w)) as u32;
        let y = self.below(u64::from(h)) as u32;
        let rw = self.below(u64::from(w - x) + 1) as u32;
        let rh = self.below(u64::from(h - y) + 1) as u32;
        Rect::from_xywh(x, y, rw, rh)
    }
    /// Rectangle that may stick out of the surface (for clipping).
    fn rect_loose(&mut self, w: u32, h: u32) -> Rect {
        let x = self.below(u64::from(w) + 40) as u32;
        let y = self.below(u64::from(h) + 40) as u32;
        Rect::from_xywh(x, y, self.below(400) as u32, self.below(300) as u32)
    }
    fn point_for(&mut self, w: u32, h: u32, cw: u32, ch: u32) -> (u32, u32) {
        (
            self.below(u64::from(w.saturating_sub(cw)) + 1) as u32,
            self.below(u64::from(h.saturating_sub(ch)) + 1) as u32,
        )
    }
}

struct Stream {
    id: u16,
    width: u32,
    height: u32,
    data: Vec<u8>,
    aus: Vec<AccessUnit>,
    next_frame: usize,
}

const EXTRA_STATIC: u16 = 100;
const EXTRA_DYNAMIC: u16 = 101;

/// Outcome of one command on one side, reduced to what must agree.
fn outcome(r: &Result<()>) -> Result<(), String> {
    match r {
        Ok(()) => Ok(()),
        Err(e) => match e.downcast_ref::<SurfaceError>() {
            Some(se) => Err(format!("{se:?}")),
            None => Err(format!("other: {e:#}")),
        },
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let sc: Scenario = serde_json::from_reader(
        std::fs::File::open(args.dir.join("scenario.json")).context("scenario.json")?,
    )?;
    let is444 = sc.codec == Codec::Avc444;
    let per_frame = if is444 { 2 } else { 1 };
    let reverse = is444 && sc.chroma == ChromaMode::Box;
    let mut core = GpuCore::new(&CoreOptions {
        device: args.device.clone(),
        backend: rdpgfx_gpu::core::BackendChoice::parse(&args.backend)
            .ok_or_else(|| anyhow::anyhow!("unknown backend {}", args.backend))?,
        vaapi_device: args.vaapi_device.clone(),
        reverse_filter: reverse,
        max_cache_slots: args.slots,
        ..Default::default()
    })?;
    core.set_capture(true);
    let mut model = SurfaceModel::new(args.slots);
    println!("Vulkan-Gerät: {}", core.device_name());

    let mut streams: Vec<Stream> = sc
        .surfaces
        .iter()
        .map(|s| {
            let data = std::fs::read(args.dir.join(&s.stream))?;
            let aus = split_access_units(&data);
            Ok(Stream {
                id: s.id,
                width: s.width,
                height: s.height,
                data,
                aus,
                next_frame: 0,
            })
        })
        .collect::<Result<_>>()?;

    let both = |core: &mut GpuCore, model: &mut SurfaceModel, op: &SurfaceOp| -> Result<bool> {
        let g = outcome(&core.apply(op));
        let c = outcome(&model.apply(op).map_err(anyhow::Error::from));
        if g != c {
            bail!("GPU und CPU uneinig bei {op:?}: GPU {g:?}, CPU {c:?}");
        }
        Ok(g.is_ok())
    };

    for s in &streams {
        let op = SurfaceOp::Create {
            id: s.id,
            width: s.width,
            height: s.height,
            format: PixelFormat::Xrgb8888,
        };
        both(&mut core, &mut model, &op)?;
    }
    for (id, w, h, f) in [
        (EXTRA_STATIC, 333, 217, PixelFormat::Argb8888),
        (EXTRA_DYNAMIC, 61, 47, PixelFormat::Xrgb8888),
    ] {
        both(
            &mut core,
            &mut model,
            &SurfaceOp::Create {
                id,
                width: w,
                height: h,
                format: f,
            },
        )?;
    }

    let mut rng = Rng(args.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut counts = std::collections::BTreeMap::<&str, (u32, u32)>::new();
    let mut compares = 0u32;
    for step in 1..=args.steps {
        let ids: Vec<u16> = model.surfaces.keys().copied().collect();
        let pick = |rng: &mut Rng| ids[rng.below(ids.len() as u64) as usize];
        let kind = rng.below(100);
        let (name, ok) = if kind < 30 && !streams.is_empty() {
            // AVC update of one fixture surface.
            let k = rng.below(streams.len() as u64) as usize;
            let s = &mut streams[k];
            let frames = s.aus.len() / per_frame;
            let f = s.next_frame % frames; // frame 0 is an IDR: looping restarts cleanly
            s.next_frame += 1;
            let full = vec![Rect::new(0, 0, s.width, s.height)];
            let set = |rng: &mut Rng| -> Vec<Rect> {
                match rng.below(10) {
                    0 => Vec::new(),
                    1..=3 => full.clone(),
                    _ => (0..1 + rng.below(4))
                        .map(|_| rng.rect_in(s.width, s.height))
                        .collect(),
                }
            };
            // The first frame after (re)start covers everything (IDR).
            let (mr, ar) = if f == 0 {
                (full.clone(), full.clone())
            } else {
                (set(&mut rng), set(&mut rng))
            };
            let au = |i: usize| {
                let a = &s.aus[i];
                &s.data[a.offset..][..a.len]
            };
            let main = AvcView {
                au: au(f * per_frame),
                rects: &mr,
            };
            let aux = is444.then(|| AvcView {
                au: au(f * per_frame + 1),
                rects: &ar,
            });
            core.avc(s.id, sc.avc444_layout, Some(main), aux)?;
            let cap = core.take_captured(s.id);
            let (pw, ph) = (pad(s.width), pad(s.height));
            let mv = Yuv420Frame::from_i420(&cap.main.context("main not captured")?, pw, ph);
            let av = cap.aux.map(|a| Yuv420Frame::from_i420(&a, pw, ph));
            model.avc(
                s.id,
                sc.avc444_layout,
                Some((&mv, &mr)),
                av.as_ref().map(|a| (a, ar.as_slice())),
                reverse,
            )?;
            ("AVC", true)
        } else {
            let op = match kind {
                30..=44 => {
                    let id = pick(&mut rng);
                    let s = &model.surfaces[&id];
                    let (w, h) = (s.width, s.height);
                    let rects = (0..1 + rng.below(3))
                        .map(|_| rng.rect_loose(w, h))
                        .collect();
                    let c = rng.next();
                    SurfaceOp::SolidFill {
                        id,
                        color: Color {
                            r: c as u8,
                            g: (c >> 8) as u8,
                            b: (c >> 16) as u8,
                        },
                        rects,
                    }
                }
                45..=62 => {
                    let src = pick(&mut rng);
                    // Same surface (scrolling) half of the time.
                    let dst = if rng.chance(50) { src } else { pick(&mut rng) };
                    let (sw, sh) = {
                        let s = &model.surfaces[&src];
                        (s.width, s.height)
                    };
                    let (dw, dh) = {
                        let s = &model.surfaces[&dst];
                        (s.width, s.height)
                    };
                    let mut rect = rng.rect_in(sw, sh);
                    rect.right = rect.right.min(rect.left + dw);
                    rect.bottom = rect.bottom.min(rect.top + dh);
                    let mut points: Vec<_> = (0..1 + rng.below(3))
                        .map(|_| rng.point_for(dw, dh, rect.width(), rect.height()))
                        .collect();
                    if src == dst && rng.chance(50) {
                        // Classic scroll: shift by a few rows/columns, overlapping.
                        let dy = rng.below(40) as u32;
                        points = vec![(rect.left, rect.top.saturating_sub(dy))];
                    }
                    if rng.chance(5) {
                        points.push((dw, dh)); // invalid: must be rejected by both
                    }
                    SurfaceOp::SurfaceToSurface {
                        src,
                        rect,
                        dst,
                        points,
                    }
                }
                63..=72 => {
                    let id = pick(&mut rng);
                    let s = &model.surfaces[&id];
                    let mut rect = rng.rect_in(s.width, s.height);
                    rect.right = rect.right.min(rect.left + 300);
                    rect.bottom = rect.bottom.min(rect.top + 200);
                    if rng.chance(4) {
                        rect.right = s.width + 1; // invalid
                    }
                    SurfaceOp::SurfaceToCache {
                        id,
                        rect,
                        slot: 1 + rng.below(u64::from(args.slots)) as u16,
                    }
                }
                73..=84 => {
                    let slot = filled_or_any(&mut rng, &model, args.slots);
                    let dst = pick(&mut rng);
                    let s = &model.surfaces[&dst];
                    let (cw, ch) = model
                        .cache
                        .get(&slot)
                        .map_or((0, 0), |e| (e.width, e.height));
                    let points = (0..1 + rng.below(3))
                        .map(|_| rng.point_for(s.width, s.height, cw, ch))
                        .collect();
                    SurfaceOp::CacheToSurface { slot, dst, points }
                }
                85..=87 => SurfaceOp::EvictCache {
                    // Slot 0 and max+1 are invalid and must be rejected.
                    slot: if rng.chance(15) {
                        rng.below(u64::from(args.slots) + 2) as u16
                    } else {
                        filled_or_any(&mut rng, &model, args.slots)
                    },
                },
                88..=94 => {
                    let id = pick(&mut rng);
                    let s = &model.surfaces[&id];
                    let mut rect = rng.rect_in(s.width, s.height);
                    rect.right = rect.right.min(rect.left + 256);
                    rect.bottom = rect.bottom.min(rect.top + 256);
                    let n = (rect.width() * rect.height() * 4) as usize;
                    let bgra = (0..n).map(|_| rng.next() as u8).collect();
                    SurfaceOp::Upload { id, rect, bgra }
                }
                95..=97 => {
                    if model.surfaces.contains_key(&EXTRA_DYNAMIC) {
                        SurfaceOp::Delete { id: EXTRA_DYNAMIC }
                    } else {
                        SurfaceOp::Create {
                            id: EXTRA_DYNAMIC,
                            width: 1 + rng.below(120) as u32,
                            height: 1 + rng.below(90) as u32,
                            format: PixelFormat::Xrgb8888,
                        }
                    }
                }
                _ => SurfaceOp::MapToOutput {
                    id: pick(&mut rng),
                    x: rng.below(4000) as u32,
                    y: rng.below(1500) as u32,
                },
            };
            let name = match &op {
                SurfaceOp::Create { .. } => "Create",
                SurfaceOp::Delete { .. } => "Delete",
                SurfaceOp::MapToOutput { .. } => "MapToOutput",
                SurfaceOp::SolidFill { .. } => "SolidFill",
                SurfaceOp::SurfaceToSurface { .. } => "SurfaceToSurface",
                SurfaceOp::SurfaceToCache { .. } => "SurfaceToCache",
                SurfaceOp::CacheToSurface { .. } => "CacheToSurface",
                SurfaceOp::EvictCache { .. } => "EvictCache",
                SurfaceOp::Upload { .. } => "Upload",
            };
            (name, both(&mut core, &mut model, &op)?)
        };
        let e = counts.entry(name).or_default();
        if ok {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
        // Several commands per RDPGFX frame, like a real server.
        if rng.chance(25) {
            core.end_frame()?;
        }
        if step % args.compare_every == 0 || step == args.steps {
            core.end_frame()?;
            compare(&mut core, &model, step)?;
            compares += 1;
        }
    }
    let gpu_map = core.mapped_surfaces();
    let cpu_map: Vec<_> = model
        .map_order
        .iter()
        .map(|id| (*id, model.surfaces[id].output_origin))
        .collect();
    let gpu_map: Vec<_> = gpu_map.iter().map(|m| (m.id, Some(m.origin))).collect();
    if gpu_map != cpu_map {
        bail!("Ausgangszuordnung weicht ab: GPU {gpu_map:?}, CPU {cpu_map:?}");
    }
    let (entries, used, alloc) = core.cache_usage();
    println!("Befehle (ausgeführt / abgelehnt):");
    for (name, (ok, rejected)) in &counts {
        println!("  {name:<17} {ok:>5} / {rejected}");
    }
    println!(
        "Cache am Ende: {entries} Einträge, {:.1} MB belegt in {:.0} MB GPU-Speicher",
        used as f64 / 1e6,
        alloc as f64 / 1e6
    );
    println!(
        "{}: {} Schritte, {} Vergleiche aller Surfaces, Ausgangszuordnung gleich -> OK",
        sc.name, args.steps, compares
    );
    Ok(())
}

/// A filled cache slot most of the time, otherwise any valid slot (possibly empty).
fn filled_or_any(rng: &mut Rng, model: &SurfaceModel, slots: u16) -> u16 {
    let mut filled: Vec<u16> = model.cache.keys().copied().collect();
    filled.sort_unstable();
    if !filled.is_empty() && rng.chance(85) {
        filled[rng.below(filled.len() as u64) as usize]
    } else {
        1 + rng.below(u64::from(slots)) as u16
    }
}

fn compare(core: &mut GpuCore, model: &SurfaceModel, step: u32) -> Result<()> {
    for (id, s) in &model.surfaces {
        let gpu = core.read_surface(*id)?;
        if gpu != s.pixels {
            let i = gpu
                .iter()
                .zip(&s.pixels)
                .position(|(a, b)| a != b)
                .unwrap_or(0)
                / 4;
            let (x, y) = (i as u32 % s.width, i as u32 / s.width);
            let n = gpu
                .chunks(4)
                .zip(s.pixels.chunks(4))
                .filter(|(a, b)| a != b)
                .count();
            bail!(
                "Schritt {step}: Surface {id} weicht in {n} Pixeln ab, erstes bei ({x},{y}): GPU {:?} CPU {:?}",
                &gpu[i * 4..i * 4 + 4],
                &s.pixels[i * 4..i * 4 + 4]
            );
        }
    }
    Ok(())
}
