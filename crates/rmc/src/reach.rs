//! Reachability of favorites: a TCP connection to the RDP port
//! and the first step of the RDP handshake (X.224 Connection Request with RDP_NEG_REQ), no
//! login. Many hosts drop ICMP, so ping says little; a port that answers the handshake is
//! an RDP server that accepts connections.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Result of one check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Not checked yet.
    Unknown,
    /// The RDP handshake was answered.
    Rdp,
    /// TCP connects, but no RDP answer (other service, or the host is still starting).
    NoRdp(String),
    /// No connection (name unknown, host off, port closed or filtered).
    Down(String),
}

/// TPKT + X.224 Connection Request + RDP_NEG_REQ (TLS | CredSSP), MS-RDPBCGR 2.2.1.1.
const CONNECTION_REQUEST: [u8; 19] = [
    0x03, 0x00, 0x00, 0x13, // TPKT, length 19
    0x0E, 0xE0, 0x00, 0x00, 0x00, 0x00, 0x00, // X.224 CR TPDU
    0x01, 0x00, 0x08, 0x00, 0x03, 0x00, 0x00, 0x00, // RDP_NEG_REQ
];

/// Whether `buf` starts with a TPKT header carrying an X.224 Connection Confirm.
fn is_connection_confirm(buf: &[u8]) -> bool {
    buf.len() >= 7 && buf[0] == 0x03 && buf[1] == 0x00 && (buf[5] & 0xF0) == 0xD0
}

/// Checks `host:port` (blocking, a few seconds at most).
pub fn probe(host: &str, port: u16) -> Reach {
    let addrs = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect::<Vec<_>>(),
        Err(e) => return Reach::Down(format!("Name nicht auflösbar ({e})")),
    };
    let mut last = String::from("keine Adresse");
    for addr in addrs.iter().take(3) {
        let mut s = match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            Ok(s) => s,
            Err(e) => {
                last = match e.kind() {
                    std::io::ErrorKind::TimedOut => "keine Antwort".into(),
                    std::io::ErrorKind::ConnectionRefused => "Port geschlossen".into(),
                    _ => e.to_string(),
                };
                continue;
            }
        };
        let _ = s.set_read_timeout(Some(READ_TIMEOUT));
        let _ = s.set_write_timeout(Some(READ_TIMEOUT));
        if let Err(e) = s.write_all(&CONNECTION_REQUEST) {
            return Reach::NoRdp(format!("Senden fehlgeschlagen ({e})"));
        }
        let mut buf = [0u8; 64];
        let mut n = 0;
        while n < 7 {
            match s.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => break,
            }
        }
        return if is_connection_confirm(&buf[..n]) {
            Reach::Rdp
        } else if n == 0 {
            Reach::NoRdp("Port offen, aber keine RDP-Antwort".into())
        } else {
            Reach::NoRdp("Port offen, kein RDP".into())
        };
    }
    Reach::Down(last)
}

/// Check intervals: stage i lasts `stage` seconds with interval `intervals[i]`; the last
/// interval repeats. Starts again when the launcher opens (or comes back from the tray).
#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    pub intervals: Vec<u32>,
    pub stage: u32,
}

impl Schedule {
    /// Interval after `elapsed` since the start of the schedule.
    pub fn interval_at(&self, elapsed: Duration) -> Duration {
        let stage = u64::from(self.stage.max(1));
        let i = (elapsed.as_secs() / stage) as usize;
        let secs = self
            .intervals
            .get(i)
            .or(self.intervals.last())
            .copied()
            .unwrap_or(60)
            .max(1);
        Duration::from_secs(u64::from(secs))
    }
}

/// Parses "5, 15, 25" (seconds, ascending not required); None if empty or invalid.
pub fn parse_intervals(s: &str) -> Option<Vec<u32>> {
    let v: Option<Vec<u32>> = s
        .split([',', ' ', ';'])
        .filter(|t| !t.is_empty())
        .map(|t| t.parse::<u32>().ok().filter(|&n| n >= 1))
        .collect();
    v.filter(|v| !v.is_empty())
}

pub fn format_intervals(v: &[u32]) -> String {
    v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn schedule_steps() {
        let s = Schedule {
            intervals: vec![5, 15, 25, 40, 60],
            stage: 60,
        };
        let at = |secs| s.interval_at(Duration::from_secs(secs)).as_secs();
        assert_eq!(
            (at(0), at(59), at(60), at(130), at(200), at(250)),
            (5, 5, 15, 25, 40, 60)
        );
        assert_eq!(at(10_000), 60);
    }

    #[test]
    fn intervals_text() {
        assert_eq!(parse_intervals("5, 15,25 40"), Some(vec![5, 15, 25, 40]));
        assert_eq!(parse_intervals(""), None);
        assert_eq!(parse_intervals("5, x"), None);
        assert_eq!(parse_intervals("0"), None);
        assert_eq!(format_intervals(&[5, 15]), "5, 15");
    }

    /// A fake server answering with `reply` after reading the request.
    fn serve(reply: &'static [u8]) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut req = [0u8; 19];
                let _ = s.read_exact(&mut req);
                assert_eq!(req, CONNECTION_REQUEST);
                let _ = s.write_all(reply);
            }
        });
        port
    }

    #[test]
    fn probe_results() {
        // TPKT + X.224 CC + RDP_NEG_RSP (CredSSP)
        let cc: &[u8] = &[
            0x03, 0x00, 0x00, 0x13, 0x0E, 0xD0, 0x00, 0x00, 0x12, 0x34, 0x00, 0x02, 0x1F, 0x08,
            0x00, 0x02, 0x00, 0x00, 0x00,
        ];
        assert_eq!(probe("127.0.0.1", serve(cc)), Reach::Rdp);
        assert!(matches!(
            probe("127.0.0.1", serve(b"SSH-2.0-x\r\n")),
            Reach::NoRdp(_)
        ));
        // closed port: bind and drop to get a free one
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(matches!(probe("127.0.0.1", port), Reach::Down(_)));
        assert!(matches!(probe("name.invalid", 3389), Reach::Down(_)));
    }
}
