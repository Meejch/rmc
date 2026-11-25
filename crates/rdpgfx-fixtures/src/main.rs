//! `gen-fixtures`: synthetic AVC420/AVC444 streams for testing the GPU core.
//!
//!   gen-fixtures generate --layout multi --codec avc444 --frames 120 fixtures/out/multi-v1
//!   gen-fixtures verify fixtures/out/multi-v1 [--decoder cuda]

mod generate;
mod verify;
mod xcheck;

use rdpgfx_fixtures::{meta, scene};

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use rdpgfx_model::{Avc444Layout, ChromaMode};

#[derive(Parser)]
#[command(
    version,
    about = "Synthetic AVC420/AVC444 fixtures for the RMC GPU core"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    V1,
    V2,
}

#[derive(Clone, Copy, ValueEnum)]
enum ChromaArg {
    /// 2x2 average (spec model, Windows); the decoder applies the reverse filter.
    Box,
    /// Top-left sample (FreeRDP encoder); exact without filter.
    Sample,
}

#[derive(Subcommand)]
enum Cmd {
    /// Render, split and encode a scenario.
    Generate {
        /// Output directory.
        out: PathBuf,
        #[arg(long, value_enum, default_value = "multi")]
        layout: meta::Layout,
        #[arg(long, value_enum, default_value = "avc444")]
        codec: meta::Codec,
        #[arg(long = "avc444", value_enum, default_value = "v1")]
        avc444_layout: LayoutArg,
        #[arg(long, value_enum, default_value = "box")]
        chroma: ChromaArg,
        #[arg(long, value_enum, default_value = "desktop")]
        content: scene::Content,
        #[arg(long, default_value_t = 120)]
        frames: u32,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value_t = 16)]
        crf: u32,
        /// Average bitrate in bit/s (VBR) instead of CRF.
        #[arg(long)]
        bitrate: Option<u64>,
        #[arg(long, default_value = "veryfast")]
        preset: String,
    },
    /// Decode a scenario and compare it against the re-rendered source.
    Verify {
        dir: PathBuf,
        #[arg(long, value_enum, default_value = "sw")]
        decoder: verify::Decoder,
        /// Minimum PSNR (dB, worst YUV plane) of decoded vs. codec-free reference.
        #[arg(long, default_value_t = 30.0)]
        threshold: f64,
        /// Verify only the first N frames.
        #[arg(long)]
        frames: Option<u32>,
    },
    /// Render one frame of the synthetic desktop as binary PPM (for the shadow-server smoke test).
    Still {
        out: PathBuf,
        #[arg(long, default_value_t = 3440)]
        width: u32,
        #[arg(long, default_value_t = 1440)]
        height: u32,
        #[arg(long, default_value_t = 0)]
        frame: u32,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Compare FreeRDP's AVC444 encoder primitive with the spec split of the CPU reference.
    Xsplit {
        ppm: PathBuf,
        #[arg(long, default_value = "target/xcheck/freerdp-split-3.33.1")]
        tool: PathBuf,
        #[arg(long = "avc444", value_enum, default_value = "v2")]
        layout: LayoutArg,
        /// opt (SIMD) or generic
        #[arg(long = "impl", default_value = "opt")]
        implementation: String,
    },
    /// Compare the CPU reference combination with FreeRDP's YUV420CombineToYUV444.
    Xcheck {
        dir: PathBuf,
        /// Helper built by tools/xcheck/build.sh.
        #[arg(long, default_value = "target/xcheck/freerdp-combine-3.33.1")]
        tool: PathBuf,
        #[arg(long, default_value_t = 10)]
        frames: u32,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Generate {
            out,
            layout,
            codec,
            avc444_layout,
            chroma,
            content,
            frames,
            fps,
            seed,
            crf,
            bitrate,
            preset,
        } => {
            let t = Instant::now();
            let opts = generate::Options {
                layout,
                codec,
                avc444_layout: match avc444_layout {
                    LayoutArg::V1 => Avc444Layout::V1,
                    LayoutArg::V2 => Avc444Layout::V2,
                },
                chroma: match chroma {
                    ChromaArg::Box => ChromaMode::Box,
                    ChromaArg::Sample => ChromaMode::Sample,
                },
                content,
                frames,
                fps,
                seed,
                crf,
                bitrate,
                preset,
                out,
            };
            let sc = generate::generate(&opts)?;
            println!(
                "generated {} ({} surfaces, {} frames) in {:.1}s -> {}",
                sc.name,
                sc.surfaces.len(),
                sc.frames,
                t.elapsed().as_secs_f64(),
                opts.out.display()
            );
        }
        Cmd::Verify {
            dir,
            decoder,
            threshold,
            frames,
        } => {
            let t = Instant::now();
            let r = verify::verify(&dir, decoder, threshold, frames)?;
            for s in &r.surfaces {
                println!(
                    "surface {} {}x{}: vs. Referenz min {:.2} dB | RGB min {:.2} (ohne Codec {:.2}) | Y {:.2} U {:.2} V {:.2}",
                    s.id,
                    s.size.0,
                    s.size.1,
                    s.psnr_vs_reference.min,
                    s.psnr_rgb.min,
                    s.psnr_rgb_without_codec.min,
                    s.psnr_y.min,
                    s.psnr_u.min,
                    s.psnr_v.min
                );
            }
            println!(
                "{}: {} ({:?}, threshold {} dB, {:.1}s)",
                r.scenario,
                if r.passed { "OK" } else { "FEHLER" },
                r.decoder,
                r.threshold_db,
                t.elapsed().as_secs_f64()
            );
            if !r.passed {
                std::process::exit(1);
            }
        }
        Cmd::Still {
            out,
            width,
            height,
            frame,
            seed,
        } => {
            use std::io::Write;
            let scene = scene::Scene {
                seed,
                content: scene::Content::Desktop,
                width,
                height,
            };
            let rgb = scene.render(frame, rdpgfx_model::Rect::new(0, 0, width, height));
            let mut f = std::io::BufWriter::new(std::fs::File::create(&out)?);
            write!(f, "P6\n{width} {height}\n255\n")?;
            f.write_all(&rgb)?;
            println!("{}", out.display());
        }
        Cmd::Xsplit {
            ppm,
            tool,
            layout,
            implementation,
        } => {
            let layout = match layout {
                LayoutArg::V1 => Avc444Layout::V1,
                LayoutArg::V2 => Avc444Layout::V2,
            };
            xcheck::xsplit(&ppm, &tool, layout, &implementation)?;
        }
        Cmd::Xcheck { dir, tool, frames } => {
            if !xcheck::xcheck(&dir, &tool, frames)? {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
