//! Fixture generation: render -> YUV444 -> views -> x264 (via the ffmpeg CLI) -> metadata.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use rdpgfx_model::frame::pad;
use rdpgfx_model::{Avc444Layout, ChromaMode, Rect, Yuv444Frame, avc444};

use rdpgfx_fixtures::annexb::split_access_units;
use rdpgfx_fixtures::meta::{
    Codec, Encoder, FORMAT_VERSION, Layout, Lc, Picture, Scenario, Surface, Update, desktop_outputs,
};
use rdpgfx_fixtures::scene::{Content, Scene};

pub struct Options {
    pub layout: Layout,
    pub codec: Codec,
    pub avc444_layout: Avc444Layout,
    pub chroma: ChromaMode,
    pub content: Content,
    pub frames: u32,
    pub fps: u32,
    pub seed: u64,
    pub crf: u32,
    /// Average bitrate in bit/s instead of CRF (imitates a rate-controlled RDP encoder).
    pub bitrate: Option<u64>,
    pub preset: String,
    pub out: PathBuf,
}

pub const X264_PARAMS: &str =
    "keyint=100000:min-keyint=100000:scenecut=0:bframes=0:ref=2:aud=1:repeat-headers=1";

pub fn generate(opts: &Options) -> Result<Scenario> {
    std::fs::create_dir_all(&opts.out).with_context(|| format!("create {}", opts.out.display()))?;
    let (dw, dh, outputs) = desktop_outputs();
    let scene = Scene {
        seed: opts.seed,
        content: opts.content,
        width: dw,
        height: dh,
    };

    let surface_rects: Vec<Rect> = match opts.layout {
        Layout::Multi => outputs.iter().map(|o| o.rect).collect(),
        Layout::Single => vec![Rect::new(0, 0, dw, dh)],
    };
    let surfaces: Vec<Surface> = surface_rects
        .iter()
        .enumerate()
        .map(|(i, r)| Surface {
            id: i as u16,
            width: r.width(),
            height: r.height(),
            origin: (r.left, r.top),
            stream: format!("surface-{i}.h264"),
            updates: format!("surface-{i}.jsonl"),
        })
        .collect();

    // Encode all surfaces in parallel (one ffmpeg/x264 process each).
    std::thread::scope(|s| -> Result<()> {
        let handles: Vec<_> = surfaces
            .iter()
            .zip(&surface_rects)
            .map(|(surf, rect)| s.spawn(|| encode_surface(opts, &scene, surf, *rect)))
            .collect();
        for h in handles {
            h.join()
                .map_err(|_| anyhow::anyhow!("encoder thread panicked"))??;
        }
        Ok(())
    })?;

    let name = opts
        .out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let scenario = Scenario {
        format_version: FORMAT_VERSION,
        name,
        layout: opts.layout,
        codec: opts.codec,
        avc444_layout: opts.avc444_layout,
        chroma: opts.chroma,
        fps: opts.fps,
        frames: opts.frames,
        scene,
        outputs,
        surfaces,
        encoder: Encoder {
            tool: "ffmpeg/libx264".into(),
            preset: opts.preset.clone(),
            crf: opts.crf,
            params: X264_PARAMS.into(),
        },
    };
    let f = File::create(opts.out.join("scenario.json"))?;
    serde_json::to_writer_pretty(f, &scenario)?;
    Ok(scenario)
}

fn encode_surface(opts: &Options, scene: &Scene, surf: &Surface, rect: Rect) -> Result<()> {
    let (pw, ph) = (pad(surf.width), pad(surf.height));
    let stream_path = opts.out.join(&surf.stream);
    let pics_per_frame = match opts.codec {
        Codec::Avc420 => 1,
        Codec::Avc444 => 2,
    };
    let log = File::create(opts.out.join(format!("surface-{}-x264.log", surf.id)))?;
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostats",
            "-y",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
        ])
        .args([
            "-s",
            &format!("{pw}x{ph}"),
            "-r",
            &(opts.fps * pics_per_frame).to_string(),
            "-i",
            "-",
        ])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            &opts.preset,
            "-tune",
            "zerolatency",
            "-profile:v",
            "high",
        ])
        .args(match opts.bitrate {
            Some(b) => vec![
                "-b:v".to_string(),
                b.to_string(),
                "-maxrate".to_string(),
                b.to_string(),
                "-bufsize".to_string(),
                b.to_string(),
            ],
            None => vec!["-crf".to_string(), opts.crf.to_string()],
        })
        .args(["-x264-params", X264_PARAMS, "-f", "h264"])
        .arg(&stream_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .context("start ffmpeg (is it installed?)")?;

    {
        let mut stdin = BufWriter::with_capacity(1 << 24, child.stdin.take().expect("stdin"));
        for frame in 0..opts.frames {
            let rgb = scene.render(frame, rect);
            let yuv = Yuv444Frame::from_rgb(&rgb, surf.width, surf.height);
            match opts.codec {
                Codec::Avc444 => {
                    let (main, aux) = avc444::split(&yuv, opts.avc444_layout, opts.chroma);
                    stdin.write_all(&main.to_i420())?;
                    stdin.write_all(&aux.to_i420())?;
                }
                Codec::Avc420 => {
                    // Plain 4:2:0: the main view with box-filtered chroma.
                    let (main, _) = avc444::split(&yuv, opts.avc444_layout, ChromaMode::Box);
                    stdin.write_all(&main.to_i420())?;
                }
            }
        }
        stdin.flush()?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!(
            "ffmpeg failed for surface {} (see surface-{}-x264.log)",
            surf.id,
            surf.id
        );
    }

    write_updates(opts, surf, &stream_path, pics_per_frame)
}

fn write_updates(
    opts: &Options,
    surf: &Surface,
    stream_path: &Path,
    pics_per_frame: u32,
) -> Result<()> {
    let data = std::fs::read(stream_path)?;
    let aus = split_access_units(&data);
    ensure!(
        aus.len() == (opts.frames * pics_per_frame) as usize,
        "surface {}: {} access units, expected {}",
        surf.id,
        aus.len(),
        opts.frames * pics_per_frame
    );
    let full = Rect::new(0, 0, surf.width, surf.height);
    let mut w = BufWriter::new(File::create(opts.out.join(&surf.updates))?);
    for frame in 0..opts.frames {
        let i = (frame * pics_per_frame) as usize;
        let pic = |k: usize| Picture {
            offset: aus[k].offset,
            len: aus[k].len,
            rects: vec![full],
        };
        let update = match opts.codec {
            Codec::Avc420 => Update {
                frame,
                codec: Codec::Avc420,
                dest: full,
                lc: None,
                main: pic(i),
                aux: None,
            },
            Codec::Avc444 => Update {
                frame,
                codec: Codec::Avc444,
                dest: full,
                lc: Some(Lc::Both),
                main: pic(i),
                aux: Some(pic(i + 1)),
            },
        };
        serde_json::to_writer(&mut w, &update)?;
        w.write_all(b"\n")?;
    }
    w.flush()?;
    Ok(())
}
