//! From local monitors to the monitor layout sent to the server, and back: which local
//! monitor shows which session output.
//!
//! RDP wants the primary monitor's top-left at (0, 0); the others are placed relative to it
//! (negative coordinates allowed, MS-RDPBCGR 2.2.1.3.6). The server answers with its own
//! layout (RDPGFX_RESET_GRAPHICS_PDU), normally the same; a server with a fixed screen (the
//! FreeRDP shadow server) answers with a single monitor.

use anyhow::{Result, bail};
use rdpgfx_model::outputs::{DesktopLayout, Monitor};

use crate::wl_outputs::LocalOutput;

/// The monitors used for a session.
#[derive(Clone, Debug)]
pub struct Selection {
    pub outputs: Vec<LocalOutput>,
    /// Index of the primary output in `outputs`.
    pub primary: usize,
}

/// Picks the monitors (`names` empty = all) and the primary one: explicit name, else KDE's
/// output order, else the largest.
pub fn select(all: &[LocalOutput], names: &[String], primary: Option<&str>) -> Result<Selection> {
    let outputs: Vec<LocalOutput> = if names.is_empty() {
        all.to_vec()
    } else {
        names
            .iter()
            .map(|n| {
                all.iter()
                    .find(|o| o.name == *n)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("monitor {n} not found"))
            })
            .collect::<Result<_>>()?
    };
    if outputs.is_empty() {
        bail!("no monitors");
    }
    let primary = match primary {
        Some(p) => outputs
            .iter()
            .position(|o| o.name == p)
            .ok_or_else(|| anyhow::anyhow!("primary monitor {p} is not selected"))?,
        None => outputs
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.order.map(|ord| (ord, i)))
            .min()
            .map(|(_, i)| i)
            .unwrap_or_else(|| {
                outputs
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, o)| u64::from(o.width) * u64::from(o.height))
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            }),
    };
    Ok(Selection { outputs, primary })
}

/// Monitor layout for the server, in the order of `sel.outputs`, primary at (0, 0).
pub fn rdp_monitors(sel: &Selection) -> Vec<Monitor> {
    let p = &sel.outputs[sel.primary];
    sel.outputs
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let left = o.x - p.x;
            let top = o.y - p.y;
            Monitor {
                left,
                top,
                right: left + o.width as i32,
                bottom: top + o.height as i32,
                primary: i == sel.primary,
            }
        })
        .collect()
}

/// Size of the virtual desktop (bounding box of all monitors).
pub fn desktop_size(mons: &[Monitor]) -> (u32, u32) {
    let b = DesktopLayout {
        monitors: mons.to_vec(),
    }
    .bounding_box();
    (b.width(), b.height())
}

/// For every session output the index of the local monitor (into the order of `sent`) that
/// shows it: same rectangle as sent (after normalising both layouts), else a sent monitor of
/// the same size (primary first), else the primary.
pub fn match_outputs(sent: &[Monitor], session: &[Monitor]) -> Vec<usize> {
    let norm = |m: &[Monitor]| {
        DesktopLayout {
            monitors: m.to_vec(),
        }
        .outputs()
    };
    let (sent_n, sess_n) = (norm(sent), norm(session));
    let primary = sent.iter().position(|m| m.primary).unwrap_or(0);
    let mut by_size: Vec<usize> = (0..sent.len()).collect();
    by_size.sort_by_key(|&i| i != primary);
    sess_n
        .iter()
        .map(|r| {
            sent_n
                .iter()
                .position(|s| s == r)
                .or_else(|| {
                    by_size.iter().copied().find(|&i| {
                        sent_n[i].width() == r.width() && sent_n[i].height() == r.height()
                    })
                })
                .unwrap_or(primary)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdpgfx_model::Rect;

    /// Three monitors: portrait left, ultrawide primary in the middle, QHD right.
    fn local() -> Vec<LocalOutput> {
        let o = |name: &str, x, y, w, h, order| LocalOutput {
            name: name.into(),
            x,
            y,
            width: w,
            height: h,
            scale: 1,
            refresh_mhz: 60000,
            portrait: h > w,
            order: Some(order),
        };
        vec![
            o("DP-4", 0, 0, 1440, 2560, 1),
            o("DP-5", 1440, 546, 3440, 1440, 0),
            o("HDMI-A-3", 4880, 546, 2560, 1440, 2),
        ]
    }

    #[test]
    fn primary_from_kde_order_and_layout_relative_to_it() {
        let sel = select(&local(), &[], None).unwrap();
        assert_eq!(sel.outputs[sel.primary].name, "DP-5");
        let m = rdp_monitors(&sel);
        assert_eq!(
            (m[0].left, m[0].top, m[0].right, m[0].bottom),
            (-1440, -546, 0, 2014)
        );
        assert_eq!(
            (m[1].left, m[1].top, m[1].right, m[1].bottom),
            (0, 0, 3440, 1440)
        );
        assert_eq!((m[2].left, m[2].top), (3440, 0));
        assert!(m[1].primary && !m[0].primary);
        assert_eq!(desktop_size(&m), (7440, 2560));
    }

    #[test]
    fn explicit_selection_and_primary() {
        let sel = select(
            &local(),
            &["HDMI-A-3".into(), "DP-5".into()],
            Some("HDMI-A-3"),
        )
        .unwrap();
        assert_eq!(sel.outputs.len(), 2);
        assert_eq!(sel.primary, 0);
        let m = rdp_monitors(&sel);
        assert_eq!((m[1].left, m[1].top), (-3440, 0));
        assert!(select(&local(), &["DP-9".into()], None).is_err());
        // Without KDE order: the largest monitor.
        let mut l = local();
        for o in &mut l {
            o.order = None;
        }
        let sel = select(&l, &[], None).unwrap();
        assert_eq!(sel.outputs[sel.primary].name, "DP-5");
    }

    #[test]
    fn session_outputs_map_back_to_local_monitors() {
        let sent = rdp_monitors(&select(&local(), &[], None).unwrap());
        // Server echoes the layout (possibly in another order).
        let echoed = vec![sent[2], sent[0], sent[1]];
        assert_eq!(match_outputs(&sent, &echoed), vec![2, 0, 1]);
        // Shadow server: one fixed 3440x1440 screen -> the primary ultrawide (same size).
        let single = [Monitor {
            left: 0,
            top: 0,
            right: 3440,
            bottom: 1440,
            primary: true,
        }];
        assert_eq!(match_outputs(&sent, &single), vec![1]);
        // Unknown size -> primary.
        let odd = [Monitor {
            left: 0,
            top: 0,
            right: 1024,
            bottom: 768,
            primary: true,
        }];
        assert_eq!(match_outputs(&sent, &odd), vec![1]);
        let _ = Rect::new(0, 0, 0, 0);
    }
}
