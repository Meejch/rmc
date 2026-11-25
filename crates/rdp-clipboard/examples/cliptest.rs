//! Manual check against the compositor: prints local changes, reads them as text, then offers
//! "remote" text for 3 s (read it with wl-paste meanwhile).
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    let cb = rdp_clipboard::WaylandClipboard::start(
        Box::new(move |m| {
            let _ = tx.send(m);
        }),
        Arc::new(|mime| {
            println!("provider asked for {mime}");
            Some(b"vom Server".to_vec())
        }),
    )?;
    let mimes = rx.recv_timeout(Duration::from_secs(3))?;
    println!("local selection: {mimes:?}");
    if let Some(m) = rdp_clipboard::convert::local_text_mime(&mimes) {
        println!("read {m}: {:?}", String::from_utf8_lossy(&cb.read(m)?));
    }
    cb.offer(&["text/plain;charset=utf-8".to_string()])?;
    println!("offered");
    // our own offer must not be reported as a local change
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(m) => println!("UNEXPECTED local change: {m:?}"),
        Err(_) => println!("no echo of the own offer"),
    }
    Ok(())
}
