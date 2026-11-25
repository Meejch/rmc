//! Per-stage timing of the GPU core: wall time and CPU time of the calling thread.
//! CPU time close to wall time while waiting means the driver spins instead of sleeping.

use std::cell::RefCell;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Decode,
    Upload,
    Record,
    Submit,
    Wait,
    Acquire,
    PresentRecord,
    PresentSubmit,
    QueuePresent,
    PresentWait,
}

pub const STAGES: [Stage; 10] = [
    Stage::Decode,
    Stage::Upload,
    Stage::Record,
    Stage::Submit,
    Stage::Wait,
    Stage::Acquire,
    Stage::PresentRecord,
    Stage::PresentSubmit,
    Stage::QueuePresent,
    Stage::PresentWait,
];

fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid pointer to a timespec; CLOCK_THREAD_CPUTIME_ID is always available on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

#[derive(Default, Clone, Copy)]
struct Acc {
    wall: Duration,
    cpu: Duration,
    count: u64,
}

/// Accumulated timings since the last `take`.
#[derive(Default)]
pub struct Profile {
    acc: RefCell<[Acc; STAGES.len()]>,
}

pub struct Span {
    stage: Stage,
    wall: Instant,
    cpu: Duration,
}

impl Profile {
    pub fn start(&self, stage: Stage) -> Span {
        Span {
            stage,
            wall: Instant::now(),
            cpu: thread_cpu(),
        }
    }

    pub fn end(&self, s: Span) {
        let i = STAGES
            .iter()
            .position(|x| *x == s.stage)
            .expect("stage listed");
        let mut acc = self.acc.borrow_mut();
        acc[i].wall += s.wall.elapsed();
        acc[i].cpu += thread_cpu().saturating_sub(s.cpu);
        acc[i].count += 1;
    }

    /// Report per frame (`frames` frames since the last call) and reset.
    pub fn take_report(&self, frames: u64) -> String {
        let acc = std::mem::take(&mut *self.acc.borrow_mut());
        let n = frames.max(1) as f64;
        let mut out = String::new();
        for (st, a) in STAGES.iter().zip(acc.iter()) {
            if a.count == 0 {
                continue;
            }
            out.push_str(&format!(
                "  {:<14} Wand {:>6.3} ms  CPU {:>6.3} ms  pro Frame\n",
                format!("{st:?}"),
                a.wall.as_secs_f64() * 1000.0 / n,
                a.cpu.as_secs_f64() * 1000.0 / n
            ));
        }
        out
    }
}
