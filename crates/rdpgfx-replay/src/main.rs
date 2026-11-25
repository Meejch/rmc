//! `rdpgfx-replay`: plays fixture streams through the GPU core into Wayland windows (development tool).
//!
//! Windows, modes, overlay and hotkeys come from `rdp-viewer` (shared with the RDP client):
//! fullscreen per output, windowed 1:1 crop (overview as option), connection bar, keyboard
//! grab. A simulated server sends frames at the fixture's rate as long as fewer than
//! `--max-unacked` frames are open; frames are acknowledged when their present completed.
//! A left click prints the session coordinates (check of the input transformation).
//!
//!   rdpgfx-replay fixtures/out/multi-v2-box [--windowed | --test-window]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::monitor::MonitorHandle;
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::WindowId;

use rdp_viewer::{Action, SessionOutput, Viewer, ViewerOptions};
use rdpgfx_fixtures::annexb::{AccessUnit, split_access_units};
use rdpgfx_fixtures::meta::{Codec, Scenario};
use rdpgfx_gpu::core::{AvcUpdate, AvcView, CoreOptions, GpuCore};
use rdpgfx_model::outputs::{DesktopLayout, Monitor};
use rdpgfx_model::{ChromaMode, PixelFormat, Rect, SurfaceOp};

#[derive(Parser)]
#[command(about = "Replay fixtures through the GPU core into Wayland windows")]
struct Args {
    dir: PathBuf,
    /// Start in windowed mode instead of fullscreen.
    #[arg(long)]
    windowed: bool,
    /// Reverse chroma filter (default: on for AVC444 with averaged chroma).
    #[arg(long)]
    reverse: Option<bool>,
    #[arg(long, default_value = "0")]
    device: String,
    /// Test window: windowed, app id "rdp-replay-test" (no focus; a KWin window rule can place
    /// it on a spare monitor). Never fullscreen.
    #[arg(long)]
    test_window: bool,
    /// Quit after this many seconds and print a summary.
    #[arg(long)]
    duration: Option<u64>,
    /// Print per-stage timings with every statistics line.
    #[arg(long)]
    profile: bool,
    /// Simulated server: at most this many unacknowledged frames (flow control).
    #[arg(long, default_value_t = 2)]
    max_unacked: usize,
    /// Decode the surfaces of a frame one after the other (default: in parallel).
    #[arg(long)]
    serial_decode: bool,
    /// Frame rate of the simulated server (default: the fixture's); above what the client
    /// can decode it shows the flow control at work.
    #[arg(long)]
    server_fps: Option<u32>,
}

/// Acknowledgement from the core's waiter thread: frame id, time of completion.
type Ack = (u64, Instant);

/// Latency statistics in milliseconds: (mean, p95, max).
fn lat_stats(v: &mut [f64]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let p95 = v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)];
    (mean, p95, v[v.len() - 1])
}

struct Stream {
    id: u16,
    data: Vec<u8>,
    aus: Vec<AccessUnit>,
    width: u32,
    height: u32,
}

struct App {
    args: Args,
    sc: Scenario,
    streams: Vec<Stream>,
    core: Option<GpuCore>,
    viewer: Option<Viewer>,
    paused: bool,
    dirty: bool,
    frame: u64,
    next_tick: Instant,
    interval: Duration,
    stats_at: Instant,
    stats_frames: u64,
    stats_cpu: f64,
    late: u64,
    error: Option<anyhow::Error>,
    proxy: Option<EventLoopProxy<Ack>>,
    /// Simulated server: frames sent and not yet acknowledged (id -> arrival time).
    unacked: HashMap<u64, Instant>,
    next_id: u64,
    lat: Vec<f64>,
    lat_all: Vec<f64>,
    stalls: u64,
    stalls_total: u64,
    depth_max: usize,
    started: Instant,
    total_frames: u64,
    total_late: u64,
    cpu_at_start: f64,
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

impl App {
    fn new(args: Args) -> Result<Self> {
        let sc: Scenario = serde_json::from_reader(
            std::fs::File::open(args.dir.join("scenario.json")).context("scenario.json")?,
        )?;
        let streams = sc
            .surfaces
            .iter()
            .map(|s| {
                let data = std::fs::read(args.dir.join(&s.stream))?;
                let aus = split_access_units(&data);
                Ok(Stream {
                    id: s.id,
                    data,
                    aus,
                    width: s.width,
                    height: s.height,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let interval = Duration::from_secs_f64(1.0 / f64::from(args.server_fps.unwrap_or(sc.fps)));
        Ok(Self {
            args,
            sc,
            streams,
            core: None,
            viewer: None,
            paused: false,
            dirty: true,
            frame: 0,
            next_tick: Instant::now(),
            interval,
            stats_at: Instant::now(),
            stats_frames: 0,
            stats_cpu: cpu_seconds(),
            late: 0,
            error: None,
            proxy: None,
            unacked: HashMap::new(),
            next_id: 1,
            lat: Vec::new(),
            lat_all: Vec::new(),
            stalls: 0,
            stalls_total: 0,
            depth_max: 0,
            started: Instant::now(),
            total_frames: 0,
            total_late: 0,
            cpu_at_start: cpu_seconds(),
        })
    }

    fn init(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let display = el.display_handle()?.as_raw();
        let is444 = self.sc.codec == Codec::Avc444;
        let reverse = self
            .args
            .reverse
            .unwrap_or(is444 && self.sc.chroma == ChromaMode::Box);
        let mut core = GpuCore::new(&CoreOptions {
            device: self.args.device.clone(),
            reverse_filter: reverse,
            display: Some(display),
            parallel_decode: !self.args.serial_decode,
            ..Default::default()
        })?;
        println!("Vulkan-Gerät: {}", core.device_name());
        if let Some(proxy) = self.proxy.clone() {
            core.on_presented(move |id| {
                let _ = proxy.send_event((id, Instant::now()));
            });
        }
        for (s, meta) in self.streams.iter().zip(&self.sc.surfaces) {
            core.apply(&SurfaceOp::Create {
                id: s.id,
                width: s.width,
                height: s.height,
                format: PixelFormat::Xrgb8888,
            })?;
            core.apply(&SurfaceOp::MapToOutput {
                id: s.id,
                x: meta.origin.0,
                y: meta.origin.1,
            })?;
        }
        let outputs = self.session_outputs(el);
        let mut viewer = Viewer::new(
            ViewerOptions {
                title: "rdp-replay".into(),
                test_window: self.args.test_window,
                start_windowed: self.args.windowed,
                grab_in_fullscreen: false,
                bar_pinned: false,
                keep_active: false,
                label: "rdp-replay".into(),
                offer_favorite: false,
            },
            outputs,
        );
        viewer.open(el, &mut core)?;
        self.core = Some(core);
        self.viewer = Some(viewer);
        self.next_tick = Instant::now();
        self.started = Instant::now();
        self.cpu_at_start = cpu_seconds();
        Ok(())
    }

    /// Fixture outputs and the local monitors that show them (by position relative to the
    /// bounding box).
    fn session_outputs(&self, el: &ActiveEventLoop) -> Vec<SessionOutput> {
        let layout = DesktopLayout {
            monitors: self
                .sc
                .outputs
                .iter()
                .map(|o| Monitor {
                    left: o.rect.left as i32,
                    top: o.rect.top as i32,
                    right: o.rect.right as i32,
                    bottom: o.rect.bottom as i32,
                    primary: o.primary,
                })
                .collect(),
        };
        let mons: Vec<MonitorHandle> = el.available_monitors().collect();
        let minx = mons.iter().map(|m| m.position().x).min().unwrap_or(0);
        let miny = mons.iter().map(|m| m.position().y).min().unwrap_or(0);
        println!("Zuordnung Session-Ausgang -> Monitor:");
        layout
            .outputs()
            .into_iter()
            .zip(&self.sc.outputs)
            .map(|(r, meta)| {
                let monitor = mons
                    .iter()
                    .min_by_key(|m| {
                        let dx = i64::from(m.position().x - minx) - i64::from(r.left);
                        let dy = i64::from(m.position().y - miny) - i64::from(r.top);
                        dx * dx + dy * dy
                    })
                    .cloned();
                println!(
                    "  {:<14} {:>4},{:<4} {}x{}{} -> {}",
                    meta.name,
                    r.left,
                    r.top,
                    r.width(),
                    r.height(),
                    if meta.primary { " (primär)" } else { "" },
                    monitor
                        .as_ref()
                        .and_then(|m| m.name())
                        .unwrap_or_else(|| "?".into())
                );
                SessionOutput {
                    name: meta.name.clone(),
                    rect: r,
                    primary: meta.primary,
                    monitor,
                }
            })
            .collect()
    }

    fn summary(&mut self) {
        let secs = self.started.elapsed().as_secs_f64();
        let mut all = std::mem::take(&mut self.lat_all);
        all.extend_from_slice(&self.lat);
        let n = all.len();
        let (lm, lp, lx) = lat_stats(&mut all);
        println!(
            "Zusammenfassung: {:.0} s, {:.1} fps im Mittel, CPU {:.1} % eines Kerns, {} Frames zu spät, Fokus erhalten: {}x",
            secs,
            (self.total_frames + self.stats_frames) as f64 / secs,
            (cpu_seconds() - self.cpu_at_start) / secs * 100.0,
            self.total_late + self.late,
            self.viewer.as_ref().map_or(0, |v| v.focus_events())
        );
        println!(
            "Flusskontrolle: {n} Frames quittiert, Quittung Ø {lm:.2} / p95 {lp:.2} / max {lx:.2} ms (Ankunft bis Present fertig), gedrosselt {}x, Warteschlange max {} (Grenze {}), offen am Ende {}",
            self.stalls_total + self.stalls,
            self.depth_max,
            self.args.max_unacked,
            self.unacked.len()
        );
    }

    fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        if !self.paused && self.unacked.len() >= self.args.max_unacked {
            // Flow control: the server waits for acknowledgements before sending more.
            self.stalls += 1;
        } else if !self.paused {
            let is444 = self.sc.codec == Codec::Avc444;
            let per_frame = if is444 { 2 } else { 1 };
            let frames = self.sc.frames as usize;
            let f = (self.frame as usize) % frames; // frame 0 is an IDR: clean loop
            let core = self.core.as_mut().expect("core");
            let full: Vec<[Rect; 1]> = self
                .streams
                .iter()
                .map(|s| [Rect::new(0, 0, s.width, s.height)])
                .collect();
            let updates: Vec<AvcUpdate<'_>> = self
                .streams
                .iter()
                .zip(&full)
                .map(|(s, full)| {
                    let au = |i: usize| {
                        let a = &s.aus[i];
                        &s.data[a.offset..][..a.len]
                    };
                    AvcUpdate {
                        id: s.id,
                        layout: self.sc.avc444_layout,
                        main: Some(AvcView {
                            au: au(f * per_frame),
                            rects: full,
                        }),
                        aux: is444.then(|| AvcView {
                            au: au(f * per_frame + 1),
                            rects: full,
                        }),
                    }
                })
                .collect();
            core.avc_frame(&updates)?;
            let id = self.next_id;
            self.next_id += 1;
            core.end_frame_ack(id)?;
            self.unacked.insert(id, now);
            self.depth_max = self.depth_max.max(self.unacked.len());
            self.frame += 1;
            self.stats_frames += 1;
            self.dirty = true;
        }
        let (core, viewer) = (
            self.core.as_mut().expect("core"),
            self.viewer.as_mut().expect("viewer"),
        );
        viewer.tick(core)?;
        if (self.dirty || viewer.needs_present()) && !viewer.minimized() {
            core.present()?;
            viewer.presented();
            self.dirty = false;
        } else if self.dirty {
            // Minimised: frames are still decoded and acknowledged.
            core.present()?;
            self.dirty = false;
        }
        if now.duration_since(self.stats_at) >= Duration::from_secs(5) {
            let secs = now.duration_since(self.stats_at).as_secs_f64();
            let cpu = cpu_seconds();
            let viewer = self.viewer.as_ref().expect("viewer");
            let views: Vec<String> = viewer
                .views()
                .into_iter()
                .filter_map(|v| {
                    let ((vw, vh), pm) = self.core.as_ref()?.view_info(v)?;
                    Some(format!("{vw}x{vh} {pm:?}"))
                })
                .collect();
            let (mode, minimized) = (viewer.mode(), viewer.minimized());
            let (lm, lp, lx) = lat_stats(&mut self.lat);
            println!(
                "{:.1} fps, CPU {:.1} % eines Kerns, {} zu spät, Quittung Ø {lm:.1} / p95 {lp:.1} / max {lx:.1} ms, gedrosselt {}, Warteschlange max {}, Modus {:?}{}, Ansichten: {}",
                self.stats_frames as f64 / secs,
                (cpu - self.stats_cpu) / secs * 100.0,
                self.late,
                self.stalls,
                self.depth_max,
                mode,
                if minimized { " (minimiert)" } else { "" },
                views.join(", ")
            );
            self.lat_all.append(&mut self.lat);
            self.stalls_total += self.stalls;
            self.stalls = 0;
            if self.args.profile {
                print!(
                    "{}",
                    self.core
                        .as_ref()
                        .expect("core")
                        .profile
                        .take_report(self.stats_frames)
                );
            }
            self.total_frames += self.stats_frames;
            self.total_late += self.late;
            self.stats_at = now;
            self.stats_frames = 0;
            self.stats_cpu = cpu;
            self.late = 0;
        }
        Ok(())
    }

    fn handle(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) -> Result<()> {
        let (Some(core), Some(viewer)) = (self.core.as_mut(), self.viewer.as_mut()) else {
            return Ok(());
        };
        for action in viewer.handle(el, core, id, event)? {
            match action {
                Action::Quit => el.exit(),
                Action::CancelTransfer | Action::SaveFavorite | Action::Menu(_) => {}
                Action::Pause => {
                    self.paused = !self.paused;
                    println!("{}", if self.paused { "Pause" } else { "Weiter" });
                }
                Action::Clicked {
                    output,
                    view,
                    session,
                } => match session {
                    Some((sx, sy)) => {
                        let hit = core.mapped_surfaces().into_iter().rev().find(|m| {
                            Rect::from_xywh(m.origin.0, m.origin.1, m.width, m.height)
                                .intersect(&Rect::from_xywh(sx, sy, 1, 1))
                                .is_some()
                        });
                        println!(
                            "Klick {output} ({:.0},{:.0}) -> Session ({sx},{sy}){}",
                            view.0,
                            view.1,
                            hit.map_or(" (keine Surface)".into(), |m| format!(
                                ", Surface {} bei ({},{})",
                                m.id,
                                sx - m.origin.0,
                                sy - m.origin.1
                            ))
                        );
                    }
                    None => println!(
                        "Klick {output} ({:.0},{:.0}) -> Rand (außerhalb)",
                        view.0, view.1
                    ),
                },
                Action::Input(_) => {}
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        if let (Some(core), Some(viewer)) = (self.core.as_mut(), self.viewer.as_mut()) {
            viewer.shutdown(core);
        }
        self.viewer = None;
        self.core = None;
    }
}

impl ApplicationHandler<Ack> for App {
    fn user_event(&mut self, _el: &ActiveEventLoop, (id, at): Ack) {
        if let Some(t0) = self.unacked.remove(&id) {
            self.lat.push(at.duration_since(t0).as_secs_f64() * 1000.0);
        }
    }

    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.core.is_none()
            && let Err(e) = self.init(el)
        {
            self.error = Some(e);
            el.exit();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Err(e) = self.handle(el, id, event) {
            self.error = Some(e);
            el.exit();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.core.is_none() {
            return;
        }
        if self
            .args
            .duration
            .is_some_and(|d| self.started.elapsed() >= Duration::from_secs(d))
        {
            self.summary();
            el.exit();
            return;
        }
        let now = Instant::now();
        if now >= self.next_tick {
            if let Err(e) = self.tick() {
                self.error = Some(e);
                el.exit();
                return;
            }
            self.next_tick += self.interval;
            let after = Instant::now();
            if after > self.next_tick {
                self.late += 1;
                // More than a frame behind: resynchronise instead of catching up.
                if after > self.next_tick + self.interval {
                    self.next_tick = after + self.interval;
                }
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_tick));
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        self.shutdown();
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut app = App::new(args)?;
    let el = EventLoop::<Ack>::with_user_event().build()?;
    app.proxy = Some(el.create_proxy());
    el.run_app(&mut app)?;
    app.shutdown();
    if let Some(e) = app.error {
        return Err(e);
    }
    Ok(())
}
