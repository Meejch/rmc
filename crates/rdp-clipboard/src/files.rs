//! File lists for the clipboard: Windows `FileGroupDescriptorW` (MS-RDPECLIP 2.2.5.2.3.1)
//! and Linux `text/uri-list`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};

/// Registered format name of the file list.
pub const FILE_LIST_NAME: &str = "FileGroupDescriptorW";
/// Registered format "Preferred DropEffect" (DROPEFFECT_COPY = 1): Explorer copies.
pub const DROP_EFFECT_NAME: &str = "Preferred DropEffect";
pub const URI_LIST_MIME: &str = "text/uri-list";
/// GNOME/Nautilus style ("copy\nfile://...").
pub const GNOME_FILES_MIME: &str = "x-special/gnome-copied-files";

const DESCRIPTOR_SIZE: usize = 592;
const FD_ATTRIBUTES: u32 = 0x4;
const FD_WRITESTIME: u32 = 0x20;
const FD_FILESIZE: u32 = 0x40;
const FD_SHOWPROGRESSUI: u32 = 0x4000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const MAX_NAME: usize = 259;

/// One entry of a file list: relative path with `\\` separators.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
}

/// Local files of a uri-list, directories expanded recursively: (entry, local path).
pub fn collect(uris: &[PathBuf]) -> Result<Vec<(Entry, PathBuf)>> {
    let mut out = Vec::new();
    for p in uris {
        let base = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Datei".into());
        walk(p, &base, &mut out)?;
    }
    Ok(out)
}

fn walk(p: &Path, rel: &str, out: &mut Vec<(Entry, PathBuf)>) -> Result<()> {
    let meta = std::fs::metadata(p)?;
    if meta.is_dir() {
        out.push((
            Entry {
                name: rel.into(),
                dir: true,
                size: 0,
            },
            p.to_path_buf(),
        ));
        let mut children: Vec<_> = std::fs::read_dir(p)?.flatten().collect();
        children.sort_by_key(|e| e.file_name());
        for c in children {
            let name = c.file_name().to_string_lossy().into_owned();
            walk(&c.path(), &format!("{rel}\\{name}"), out)?;
        }
    } else {
        out.push((
            Entry {
                name: rel.into(),
                dir: false,
                size: meta.len(),
            },
            p.to_path_buf(),
        ));
    }
    Ok(())
}

fn filetime_now() -> u64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    (secs + 11_644_473_600) * 10_000_000
}

/// FILEGROUPDESCRIPTORW for the entries.
pub fn build_descriptor(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + entries.len() * DESCRIPTOR_SIZE);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    let now = filetime_now();
    for e in entries {
        let mut d = vec![0u8; DESCRIPTOR_SIZE];
        let flags = FD_ATTRIBUTES | FD_WRITESTIME | FD_FILESIZE | FD_SHOWPROGRESSUI;
        d[0..4].copy_from_slice(&flags.to_le_bytes());
        let attrs = if e.dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        d[36..40].copy_from_slice(&attrs.to_le_bytes());
        d[56..64].copy_from_slice(&now.to_le_bytes());
        d[64..68].copy_from_slice(&((e.size >> 32) as u32).to_le_bytes());
        d[68..72].copy_from_slice(&(e.size as u32).to_le_bytes());
        for (i, u) in e.name.encode_utf16().take(MAX_NAME).enumerate() {
            d[72 + i * 2..74 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&d);
    }
    out
}

/// Parses a FILEGROUPDESCRIPTORW.
pub fn parse_descriptor(data: &[u8]) -> Result<Vec<Entry>> {
    if data.len() < 4 {
        bail!("file list too short");
    }
    let n = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + n * DESCRIPTOR_SIZE {
        bail!("file list truncated ({n} entries)");
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let d = &data[4 + i * DESCRIPTOR_SIZE..][..DESCRIPTOR_SIZE];
        let u32_at = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
        let attrs = u32_at(36);
        let size = (u64::from(u32_at(64)) << 32) | u64::from(u32_at(68));
        let units: Vec<u16> = d[72..72 + 520]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        out.push(Entry {
            name: String::from_utf16_lossy(&units),
            dir: attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
            size,
        });
    }
    Ok(out)
}

/// A relative Windows path as safe local path components (no "..", no absolute parts).
pub fn safe_relative(name: &str) -> Option<PathBuf> {
    let mut p = PathBuf::new();
    for part in name.split(['\\', '/']) {
        if part.is_empty() || part == "." || part == ".." || part.contains(':') {
            if part == ".." || part.contains(':') {
                return None;
            }
            continue;
        }
        p.push(part);
    }
    (!p.as_os_str().is_empty()).then_some(p)
}

/// Paths of a `text/uri-list` (file:// only, percent-decoded).
pub fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("file://"))
        .map(|rest| {
            // file://host/path: drop a host part
            let path = if rest.starts_with('/') {
                rest
            } else {
                rest.find('/').map_or(rest, |i| &rest[i..])
            };
            PathBuf::from(percent_decode(path))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `file://` URI of a local path (percent-encoded).
pub fn file_uri(p: &Path) -> String {
    let mut s = String::from("file://");
    for &c in p.to_string_lossy().as_bytes() {
        if c.is_ascii_alphanumeric() || b"/-_.~".contains(&c) {
            s.push(c as char);
        } else {
            s.push_str(&format!("%{c:02X}"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_roundtrip() {
        let e = vec![
            Entry {
                name: "Ordner".into(),
                dir: true,
                size: 0,
            },
            Entry {
                name: "Ordner\\Bericht Ä.pdf".into(),
                dir: false,
                size: 5_000_000_000,
            },
        ];
        let d = build_descriptor(&e);
        assert_eq!(d.len(), 4 + 2 * 592);
        assert_eq!(parse_descriptor(&d).unwrap(), e);
    }

    #[test]
    fn uri_list_and_safety() {
        let p = parse_uri_list(
            "file:///home/u/Gr%C3%BC%C3%9Fe%20Datei.txt\r\n#c\r\nfile://host/tmp/x\r\n",
        );
        assert_eq!(
            p,
            vec![
                PathBuf::from("/home/u/Grüße Datei.txt"),
                PathBuf::from("/tmp/x")
            ]
        );
        assert_eq!(file_uri(Path::new("/a b/ü")), "file:///a%20b/%C3%BC");
        assert_eq!(
            safe_relative("a\\b\\c.txt"),
            Some(PathBuf::from("a/b/c.txt"))
        );
        assert_eq!(safe_relative("..\\etc\\passwd"), None);
        assert_eq!(safe_relative("C:\\x"), None);
    }

    #[test]
    fn collect_walks_directories() {
        let dir = std::env::temp_dir().join(format!("rdpc-files-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), b"hallo").unwrap();
        let e = collect(std::slice::from_ref(&dir)).unwrap();
        let names: Vec<_> = e.iter().map(|(e, _)| e.name.clone()).collect();
        let base = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            names,
            vec![
                base.clone(),
                format!("{base}\\sub"),
                format!("{base}\\sub\\a.txt")
            ]
        );
        assert_eq!(e[2].0.size, 5);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
