//! Per-process TCP traffic without root or eBPF: the kernel's `sock_diag` netlink interface
//! reports `tcp_info.bytes_acked` / `bytes_received` for every socket to any user, and a
//! socket's inode ties it to the process whose fd table holds it (readable for the caller's own
//! processes, or for all of them as root). TCP only; UDP has no byte counters in `sock_diag`.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::mem::size_of;

const NETLINK_SOCK_DIAG: i32 = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST_DUMP: u16 = 0x1 | 0x300;
const INET_DIAG_INFO: u16 = 2;
/// Ask for `INET_DIAG_INFO` (the `tcp_info` attribute).
const EXT_INFO: u8 = 1 << (INET_DIAG_INFO - 1);

const NLMSG_HDR: usize = 16;
/// `struct inet_diag_msg` without its attributes.
const DIAG_MSG: usize = 72;
const DIAG_INODE: usize = 68;
/// Offsets of `tcpi_bytes_acked` and `tcpi_bytes_received` in `struct tcp_info`.
const TCPI_ACKED: usize = 120;
const TCPI_RECEIVED: usize = 128;

/// Cumulative bytes of one TCP socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockBytes {
    pub inode: u64,
    pub sent: u64,
    pub received: u64,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// Parses one `recv` buffer. Returns the sockets found and whether the dump is complete.
fn parse(buf: &[u8], out: &mut Vec<SockBytes>) -> io::Result<bool> {
    let mut at = 0;
    while at + NLMSG_HDR <= buf.len() {
        let len = u32_at(buf, at).unwrap_or(0) as usize;
        let kind = u16_at(buf, at + 4).unwrap_or(0);
        if len < NLMSG_HDR || at + len > buf.len() {
            break;
        }
        let body = &buf[at + NLMSG_HDR..at + len];
        match kind {
            NLMSG_DONE => return Ok(true),
            NLMSG_ERROR => return Err(io::Error::other("sock_diag refused")),
            SOCK_DIAG_BY_FAMILY if body.len() >= DIAG_MSG => {
                if let Some(s) = parse_sock(body) {
                    out.push(s);
                }
            }
            _ => {}
        }
        at += len.next_multiple_of(4);
    }
    Ok(false)
}

fn parse_sock(body: &[u8]) -> Option<SockBytes> {
    let inode = u64::from(u32_at(body, DIAG_INODE)?);
    // Attributes follow the fixed message: u16 len, u16 type, payload, padded to 4 bytes.
    let mut at = DIAG_MSG;
    while at + 4 <= body.len() {
        let len = u16_at(body, at)? as usize;
        let ty = u16_at(body, at + 2)?;
        if len < 4 || at + len > body.len() {
            break;
        }
        if ty == INET_DIAG_INFO {
            let info = &body[at + 4..at + len];
            return Some(SockBytes {
                inode,
                sent: u64_at(info, TCPI_ACKED)?,
                received: u64_at(info, TCPI_RECEIVED)?,
            });
        }
        at += len.next_multiple_of(4);
    }
    None
}

struct Fd(i32);
impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: closing a descriptor this struct owns.
        unsafe { libc::close(self.0) };
    }
}

fn dump_family(family: u8, out: &mut Vec<SockBytes>) -> io::Result<()> {
    // SAFETY: plain syscalls on buffers that outlive the calls.
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            NETLINK_SOCK_DIAG,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = Fd(fd);
        let timeout = libc::timeval {
            tv_sec: 1,
            tv_usec: 0,
        };
        libc::setsockopt(
            fd.0,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const timeout).cast(),
            size_of::<libc::timeval>() as u32,
        );

        // nlmsghdr (16) + inet_diag_req_v2 (8 + 48-byte socket id, all zero = wildcard).
        let mut req = [0u8; NLMSG_HDR + 56];
        let total = req.len() as u32;
        req[0..4].copy_from_slice(&total.to_ne_bytes());
        req[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        req[6..8].copy_from_slice(&NLM_F_REQUEST_DUMP.to_ne_bytes());
        req[NLMSG_HDR] = family;
        req[NLMSG_HDR + 1] = libc::IPPROTO_TCP as u8;
        req[NLMSG_HDR + 2] = EXT_INFO;
        req[NLMSG_HDR + 4..NLMSG_HDR + 8].copy_from_slice(&u32::MAX.to_ne_bytes());
        if libc::send(fd.0, req.as_ptr().cast(), req.len(), 0) < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = libc::recv(fd.0, buf.as_mut_ptr().cast(), buf.len(), 0);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 || parse(&buf[..n as usize], out)? {
                return Ok(());
            }
        }
    }
}

/// Every TCP socket (IPv4 and IPv6) with its cumulative byte counts.
/// `Err` means the kernel does not offer the interface here (container, old kernel, seccomp).
pub fn dump_tcp() -> io::Result<Vec<SockBytes>> {
    let mut out = Vec::new();
    dump_family(libc::AF_INET as u8, &mut out)?;
    // A kernel without IPv6 is fine as long as IPv4 worked.
    let _ = dump_family(libc::AF_INET6 as u8, &mut out);
    Ok(out)
}

/// Inodes of the sockets in a process's fd table. `Err` when the table is not readable.
pub fn socket_inodes(pid: i32) -> io::Result<Vec<u64>> {
    let mut v = Vec::new();
    for e in fs::read_dir(format!("/proc/{pid}/fd"))?.flatten() {
        if let Ok(t) = fs::read_link(e.path())
            && let Some(s) = t.to_str().and_then(|s| s.strip_prefix("socket:["))
            && let Some(n) = s.strip_suffix(']').and_then(|n| n.parse().ok())
        {
            v.push(n);
        }
    }
    Ok(v)
}

/// Turns successive socket dumps into per-process receive/send rates.
#[derive(Default)]
pub struct NetTracker {
    prev: Option<HashMap<u64, (u64, u64)>>,
    /// Socket inode -> owning PID, kept while the socket lives.
    owner: HashMap<u64, i32>,
    /// Sockets already searched for in every readable fd table without success (other
    /// users' processes, when not root); they are not searched again.
    orphan: std::collections::HashSet<u64>,
}

impl NetTracker {
    /// Rates for each PID in `scan` (bytes/s received, sent), or `None` when `sock_diag` is
    /// unavailable. `scan` lists the PIDs whose fd tables may be read.
    pub fn update(
        &mut self,
        scan: &[i32],
        elapsed: Option<f64>,
    ) -> Option<HashMap<i32, (u64, u64)>> {
        let socks = dump_tcp().ok()?;
        Some(self.apply(&socks, scan, elapsed, |pid| socket_inodes(pid).ok()))
    }

    fn apply(
        &mut self,
        socks: &[SockBytes],
        scan: &[i32],
        elapsed: Option<f64>,
        inodes_of: impl Fn(i32) -> Option<Vec<u64>>,
    ) -> HashMap<i32, (u64, u64)> {
        let live: std::collections::HashSet<u64> = socks.iter().map(|s| s.inode).collect();
        self.owner.retain(|i, _| live.contains(i));
        self.orphan.retain(|i| live.contains(i));

        // Walking every fd table is the expensive part; only do it for sockets never seen.
        let unknown = |t: &Self| {
            socks
                .iter()
                .any(|s| !t.owner.contains_key(&s.inode) && !t.orphan.contains(&s.inode))
        };
        if unknown(self) {
            for &pid in scan {
                for i in inodes_of(pid).unwrap_or_default() {
                    if live.contains(&i) {
                        self.owner.entry(i).or_insert(pid);
                    }
                }
            }
            for s in socks {
                if !self.owner.contains_key(&s.inode) {
                    self.orphan.insert(s.inode);
                }
            }
        }

        let mut rates: HashMap<i32, (u64, u64)> = scan.iter().map(|p| (*p, (0, 0))).collect();
        if let (Some(prev), Some(dt)) = (&self.prev, elapsed.filter(|d| *d > 0.0)) {
            for s in socks {
                let Some(&pid) = self.owner.get(&s.inode) else {
                    continue;
                };
                // A socket born since the last round started from zero.
                let (ps, pr) = prev.get(&s.inode).copied().unwrap_or((0, 0));
                let r = rates.entry(pid).or_default();
                r.0 += (s.received.saturating_sub(pr) as f64 / dt) as u64;
                r.1 += (s.sent.saturating_sub(ps) as f64 / dt) as u64;
            }
        }
        self.prev = Some(
            socks
                .iter()
                .map(|s| (s.inode, (s.sent, s.received)))
                .collect(),
        );
        rates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(inode: u32, sent: u64, received: u64) -> Vec<u8> {
        let mut info = vec![0u8; 232];
        info[TCPI_ACKED..TCPI_ACKED + 8].copy_from_slice(&sent.to_ne_bytes());
        info[TCPI_RECEIVED..TCPI_RECEIVED + 8].copy_from_slice(&received.to_ne_bytes());
        let mut body = vec![0u8; DIAG_MSG];
        body[DIAG_INODE..DIAG_INODE + 4].copy_from_slice(&inode.to_ne_bytes());
        // An unrelated attribute first, then the tcp_info one.
        body.extend_from_slice(&[5, 0, 1, 0, 9, 0, 0, 0]);
        body.extend_from_slice(&((info.len() + 4) as u16).to_ne_bytes());
        body.extend_from_slice(&INET_DIAG_INFO.to_ne_bytes());
        body.extend_from_slice(&info);
        let mut out = Vec::new();
        out.extend_from_slice(&((NLMSG_HDR + body.len()) as u32).to_ne_bytes());
        out.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        out.extend_from_slice(&[0; 10]);
        out.extend_from_slice(&body);
        out
    }

    fn done() -> Vec<u8> {
        let mut m = vec![0u8; NLMSG_HDR];
        m[0] = NLMSG_HDR as u8;
        m[4..6].copy_from_slice(&NLMSG_DONE.to_ne_bytes());
        m
    }

    #[test]
    fn parses_sock_diag_dump() {
        let mut buf = msg(11, 100, 200);
        buf.extend(msg(12, 3, 4));
        let mut out = Vec::new();
        assert!(!parse(&buf, &mut out).unwrap());
        buf.extend(done());
        out.clear();
        assert!(parse(&buf, &mut out).unwrap());
        assert_eq!(
            out,
            [
                SockBytes {
                    inode: 11,
                    sent: 100,
                    received: 200
                },
                SockBytes {
                    inode: 12,
                    sent: 3,
                    received: 4
                },
            ]
        );
    }

    #[test]
    fn truncated_or_short_info_is_ignored() {
        let mut out = Vec::new();
        let m = msg(1, 1, 1);
        assert!(!parse(&m[..m.len() - 3], &mut out).unwrap());
        assert!(out.is_empty());
        assert!(parse(&[0u8; 4], &mut out).is_ok());
    }

    fn sock(inode: u64, sent: u64, received: u64) -> SockBytes {
        SockBytes {
            inode,
            sent,
            received,
        }
    }

    #[test]
    fn rates_are_deltas_per_owner() {
        let mut t = NetTracker::default();
        let owners = |pid: i32| match pid {
            10 => Some(vec![1, 2]),
            20 => Some(vec![3]),
            _ => None,
        };
        let scan = [10, 20, 30];
        let first = t.apply(&[sock(1, 0, 0), sock(3, 0, 0)], &scan, None, owners);
        assert_eq!(first[&10], (0, 0), "no baseline yet");
        let r = t.apply(
            &[
                sock(1, 1000, 4000),
                sock(2, 50, 100),
                sock(3, 0, 600),
                sock(9, 7, 7),
            ],
            &scan,
            Some(2.0),
            owners,
        );
        // Socket 2 is new, so all of its bytes count; 9 belongs to nobody we can read.
        assert_eq!(r[&10], ((4000 + 100) / 2, (1000 + 50) / 2));
        assert_eq!(r[&20], (300, 0));
        assert_eq!(r[&30], (0, 0));
        // A closed socket's counters vanishing must not produce a rate.
        let r = t.apply(&[sock(3, 0, 600)], &scan, Some(1.0), owners);
        assert_eq!(r[&10], (0, 0));
        assert_eq!(r[&20], (0, 0));
    }

    #[test]
    fn fd_tables_are_only_walked_for_unseen_sockets() {
        use std::cell::Cell;
        let mut t = NetTracker::default();
        let walks = Cell::new(0);
        let owners = |_: i32| {
            walks.set(walks.get() + 1);
            Some(vec![1])
        };
        let socks = [sock(1, 0, 0), sock(2, 0, 0)];
        t.apply(&socks, &[5], None, owners);
        assert_eq!(walks.get(), 1);
        t.apply(&socks, &[5], Some(1.0), owners);
        assert_eq!(walks.get(), 1, "inode 2 is known to be unowned");
        t.apply(&[sock(1, 0, 0), sock(3, 0, 0)], &[5], Some(1.0), owners);
        assert_eq!(walks.get(), 2, "a new socket triggers a rescan");
    }
}
