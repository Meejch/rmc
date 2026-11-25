//! Progress of a file transfer in KDE's job tracker (org.kde.JobViewServer), the same view
//! Dolphin uses for copies: percent, amount, speed and remaining time in the notification
//! area, visible whatever the session windows do. A job view belongs to the D-Bus
//! connection that requested it, so each job keeps its own connection on a small thread.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use zbus::zvariant::{OwnedObjectPath, Value};

enum Msg {
    Update { done: u64, total: u64, speed: u64 },
    Finish(Option<String>),
}

/// A running job view; dropping it without `finish` ends the job as aborted.
pub struct JobView {
    tx: mpsc::UnboundedSender<Msg>,
    last: Instant,
    finished: bool,
}

impl JobView {
    /// Starts a job view (None if KDE's job tracker is not reachable).
    pub fn start(title: &str) -> Option<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let title = title.to_string();
        std::thread::Builder::new()
            .name("jobview".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(rt) = rt
                    && let Err(e) = rt.block_on(run(title, rx))
                {
                    eprintln!("Fortschrittsanzeige (KDE) nicht verfügbar: {e}");
                }
            })
            .ok()?;
        Some(Self {
            tx,
            last: Instant::now() - Duration::from_secs(1),
            finished: false,
        })
    }

    /// Progress (sent at most 4 times per second unless `force`).
    pub fn update(&mut self, done: u64, total: u64, speed: u64, force: bool) {
        if force || self.last.elapsed() >= Duration::from_millis(250) {
            self.last = Instant::now();
            let _ = self.tx.send(Msg::Update { done, total, speed });
        }
    }

    /// Ends the job (`error` = message shown as failure).
    pub fn finish(&mut self, error: Option<String>) {
        if !self.finished {
            self.finished = true;
            let _ = self.tx.send(Msg::Finish(error));
        }
    }
}

impl Drop for JobView {
    fn drop(&mut self) {
        self.finish(Some("abgebrochen".into()));
    }
}

async fn run(title: String, mut rx: mpsc::UnboundedReceiver<Msg>) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    let mut hints: HashMap<&str, Value> = HashMap::new();
    hints.insert("title", Value::from(title.as_str()));
    let reply = conn
        .call_method(
            Some("org.kde.JobViewServer"),
            "/JobViewServer",
            Some("org.kde.JobViewServerV2"),
            "requestView",
            &("rmc", 0i32, hints),
        )
        .await?;
    let path: OwnedObjectPath = reply.body().deserialize()?;
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Update { done, total, speed } => {
                let percent = done
                    .saturating_mul(100)
                    .checked_div(total)
                    .map_or(0, |p| p.min(100) as u32);
                let _ = job(&conn, &path, "setTotalAmount", &(total, "bytes")).await;
                let _ = job(&conn, &path, "setProcessedAmount", &(done, "bytes")).await;
                let _ = job(&conn, &path, "setPercent", &(percent,)).await;
                let _ = job(&conn, &path, "setSpeed", &(speed,)).await;
            }
            Msg::Finish(error) => {
                let _ = job(&conn, &path, "terminate", &(error.unwrap_or_default(),)).await;
                break;
            }
        }
    }
    Ok(())
}

async fn job<B>(
    conn: &zbus::Connection,
    path: &OwnedObjectPath,
    method: &str,
    body: &B,
) -> zbus::Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    conn.call_method(
        Some("org.kde.JobViewServer"),
        path.as_str(),
        Some("org.kde.JobViewV2"),
        method,
        body,
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    /// Manual check against Plasma (shows a job for 4 s): cargo test jobview -- --ignored
    #[test]
    #[ignore]
    fn jobview_shows_progress() {
        let mut j = super::JobView::start("RMC Test").expect("job view");
        let total = 400_000_000u64;
        for i in 0..=8u64 {
            j.update(i * total / 8, total, 100_000_000, true);
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        j.finish(None);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
