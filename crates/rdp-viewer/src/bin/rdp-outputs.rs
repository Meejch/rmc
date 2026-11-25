//! `rdp-outputs`: local monitors (Wayland) and the monitor layout the client sends.
fn main() -> anyhow::Result<()> {
    let all = rdp_viewer::wl_outputs::query()?;
    println!("Lokale Monitore (Wayland, xdg-output, KDE-Reihenfolge):");
    for o in &all {
        println!(
            "  {:<10} Position {:>5},{:<5} Größe {:>4}x{:<4} {:>7.2} Hz Skalierung {}{}{}",
            o.name,
            o.x,
            o.y,
            o.width,
            o.height,
            f64::from(o.refresh_mhz) / 1000.0,
            o.scale,
            if o.portrait { " Hochformat" } else { "" },
            o.order
                .map_or(String::new(), |n| format!(" Reihenfolge {n}"))
        );
    }
    let sel = rdp_viewer::layout::select(&all, &[], None)?;
    let mons = rdp_viewer::layout::rdp_monitors(&sel);
    let (w, h) = rdp_viewer::layout::desktop_size(&mons);
    println!("Layout für den Server (Primär bei 0,0), Desktop {w}x{h}:");
    for (o, m) in sel.outputs.iter().zip(&mons) {
        println!(
            "  {:<10} {:>6},{:<6} {}x{}{}",
            o.name,
            m.left,
            m.top,
            m.right - m.left,
            m.bottom - m.top,
            if m.primary { " (primär)" } else { "" }
        );
    }
    Ok(())
}
