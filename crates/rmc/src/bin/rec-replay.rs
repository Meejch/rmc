//! `rec-replay`: replays a recording of `rmc --record` through the GPU core
//! (headless) and writes the surfaces as PPM every N frames: `<out>/ours-<id>-<frame>.ppm`,
//! the counterpart of `tools/rec/freerdp-rec.c` (FreeRDP's decoder on the same data).
//!
//!   rec-replay <recording dir> <out dir> [--every 50] [--reverse-filter]

use std::collections::HashMap;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::Value;

use rdpgfx_gpu::core::{AvcView, CoreOptions, GpuCore};
use rdpgfx_model::{Avc444Layout, PixelFormat, Rect, SurfaceOp};

#[derive(Parser)]
#[command(about = "Replay a recording through the GPU core and dump surfaces")]
struct Args {
    dir: PathBuf,
    out: PathBuf,
    #[arg(long, default_value_t = 50)]
    every: u32,
    #[arg(long)]
    reverse_filter: bool,
    /// Stop after this many frames.
    #[arg(long)]
    frames: Option<u32>,
}

fn rects(v: &Value) -> Vec<Rect> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|r| {
                    let n = |i: usize| r[i].as_u64().unwrap_or(0) as u32;
                    Rect::new(n(0), n(1), n(2), n(3))
                })
                .collect()
        })
        .unwrap_or_default()
}

struct Streams {
    dir: PathBuf,
    files: HashMap<u16, std::fs::File>,
}

impl Streams {
    fn au(&mut self, id: u16, au: &Value) -> Result<Vec<u8>> {
        if !self.files.contains_key(&id) {
            let f = std::fs::File::open(self.dir.join(format!("surface-{id}.h264")))?;
            self.files.insert(id, f);
        }
        let f = self.files.get_mut(&id).expect("inserted");
        let off = au["offset"].as_u64().context("offset")?;
        let len = au["len"].as_u64().context("len")? as usize;
        f.seek(SeekFrom::Start(off))?;
        let mut buf = vec![0; len];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }
}

fn dump(core: &mut GpuCore, out: &Path, frame: u32) -> Result<()> {
    for id in core.surface_ids() {
        let (w, h) = core.surface_size(id)?;
        let bgra = core.read_surface(id)?;
        let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
        for p in bgra.as_chunks::<4>().0 {
            data.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        std::fs::write(out.join(format!("ours-{id}-{frame:06}.ppm")), data)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)?;
    let mut core = GpuCore::new(&CoreOptions {
        reverse_filter: args.reverse_filter,
        ..Default::default()
    })?;
    let mut streams = Streams {
        dir: args.dir.clone(),
        files: HashMap::new(),
    };
    let log = std::io::BufReader::new(std::fs::File::open(args.dir.join("commands.jsonl"))?);
    let (mut frames, mut avc, mut errors) = (0u32, 0u32, 0u32);
    for line in log.lines() {
        let v: Value = serde_json::from_str(&line?)?;
        let id = v["id"].as_u64().unwrap_or(0) as u16;
        let r: Result<()> = match v["cmd"].as_str().unwrap_or("") {
            "create" => core.apply(&SurfaceOp::Create {
                id,
                width: v["width"].as_u64().unwrap_or(0) as u32,
                height: v["height"].as_u64().unwrap_or(0) as u32,
                format: if v["format"] == "Argb8888" {
                    PixelFormat::Argb8888
                } else {
                    PixelFormat::Xrgb8888
                },
            }),
            "delete" => {
                if core.surface_ids().contains(&id) {
                    core.delete_surface(id)
                } else {
                    Ok(())
                }
            }
            "map" => core.apply(&SurfaceOp::MapToOutput {
                id,
                x: v["x"].as_u64().unwrap_or(0) as u32,
                y: v["y"].as_u64().unwrap_or(0) as u32,
            }),
            "reset" => core.reset_graphics(),
            "gfx_closed" => core.clear_all(),
            "avc444" | "avc420" => {
                avc += 1;
                let layout = if v["v2"].as_bool().unwrap_or(false) {
                    Avc444Layout::V2
                } else {
                    Avc444Layout::V1
                };
                let load = |s: &mut Streams, x: &Value| -> Result<Option<(Vec<u8>, Vec<Rect>)>> {
                    if x.is_null() {
                        return Ok(None);
                    }
                    Ok(Some((s.au(id, &x["au"])?, rects(&x["rects"]))))
                };
                let m = load(&mut streams, &v["main"])?;
                let a = if v["cmd"] == "avc444" {
                    load(&mut streams, &v["aux"])?
                } else {
                    None
                };
                core.avc(
                    id,
                    layout,
                    m.as_ref().map(|(d, r)| AvcView { au: d, rects: r }),
                    a.as_ref().map(|(d, r)| AvcView { au: d, rects: r }),
                )
            }
            "end" => {
                frames += 1;
                core.end_frame()?;
                if frames % args.every == 0 {
                    dump(&mut core, &args.out, frames)?;
                }
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(e) = r {
            errors += 1;
            if errors <= 5 {
                eprintln!("Fehler bei {}: {e:#}", v["cmd"]);
            }
        }
        if args.frames.is_some_and(|f| frames >= f) {
            break;
        }
    }
    core.flush()?;
    dump(&mut core, &args.out, frames)?;
    println!("rec-replay: {frames} Frames, {avc} AVC-Updates, {errors} Fehler");
    Ok(())
}
