// SPDX-License-Identifier: AGPL-3.0-or-later

//! nic-peer: the far end of the wire for the NIC conformance test.
//!
//! QEMU's `-netdev socket,udp=...` carries each Ethernet frame (no FCS) as
//! one UDP datagram. This program is the other end of that socket. It
//!
//! 1. sends ARP who-has for the driver's address every 500 ms until a valid
//!    reply arrives (the VM boots meanwhile), then
//! 2. sends ICMP echo requests one at a time and checks every reply field
//!    by field: first `--pings` small ones (56 data bytes), then `--bulk`
//!    large ones (1000 to 1472 data bytes, varying, so frames of 1042 to
//!    1514 bytes land at shifting offsets). The bulk step moves enough
//!    traffic to wrap every receive and transmit ring a driver is likely to
//!    use many times over, so ring wrap-around is tested, not just the first
//!    few slots.
//!
//! ```text
//! nic-peer --ports-file FILE [--stop-file FILE] [--deadline-s N] [--pings N] [--bulk N]
//! ```
//!
//! When the stop file appears (the VM has powered off), the peer stops
//! waiting and reports the step it was on.
//!
//! It binds its own port, reserves one for QEMU, and writes "PEER LOCAL" to
//! FILE for `vm/run.sh --net-udp PEER:LOCAL`.
//!
//! Output: `NIC step=<step> result=pass|fail expected="..." got="..."`
//! lines with the frames it saw indented underneath, ending with
//! `NIC step=peer result=pass` on success. Exit 0 only on pass.
//!
//! Written from RFC 826, 791, 792 and 1071. Std only.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, UdpSocket};
use std::process::ExitCode;
use std::time::{Duration, Instant};

type Mac = [u8; 6];

/// The link partner (us).
const PEER_MAC: Mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
const PEER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// The device under test: the MAC vm/run.sh gives the e1000e, and the
/// address tests/nic/guest.sh tells the host to answer for.
const DUT_MAC: Mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const DUT_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

const ARP_INTERVAL: Duration = Duration::from_millis(500);
const PING_WAIT: Duration = Duration::from_secs(2);
const ICMP_ID: u16 = 0x6772; // "gr"
const PING_DATA: usize = 56;
/// Bulk pings carry BULK_MIN to BULK_MIN + BULK_SPAN - 1 data bytes; the
/// largest makes a 1514-byte frame (14 + 20 + 8 + 1472).
const BULK_MIN: usize = 1000;
const BULK_SPAN: usize = 473;
/// Frames printed before the rest are only counted.
const MAX_PRINTED: usize = 60;

fn fmt_mac(m: &[u8]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn ip_at(b: &[u8], at: usize) -> Ipv4Addr {
    Ipv4Addr::new(b[at], b[at + 1], b[at + 2], b[at + 3])
}

fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for c in data.chunks(2) {
        sum += u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]));
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn clean(s: &str) -> String {
    s.replace(['"', '\n'], " ")
}

fn step(name: &str, pass: bool, expected: &str, got: &str) {
    println!(
        "NIC step={name} result={} expected=\"{}\" got=\"{}\"",
        if pass { "pass" } else { "fail" },
        clean(expected),
        clean(got)
    );
}

/// One-line description of a frame, for the log.
fn summarize(f: &[u8]) -> String {
    if f.len() < 14 {
        return format!("runt, {} bytes", f.len());
    }
    let ethertype = be16(f, 12);
    let mut s = format!(
        "{} -> {} type 0x{ethertype:04x} len {}",
        fmt_mac(&f[6..12]),
        fmt_mac(&f[0..6]),
        f.len()
    );
    let p = &f[14..];
    match ethertype {
        0x0806 if p.len() >= 28 => s += &format!(
            ": ARP op {} sender {} {} target {} {}",
            be16(p, 6),
            fmt_mac(&p[8..14]),
            ip_at(p, 14),
            fmt_mac(&p[18..24]),
            ip_at(p, 24)
        ),
        0x0800 if p.len() >= 20 => {
            s += &format!(": IPv4 proto {} {} -> {}", p[9], ip_at(p, 12), ip_at(p, 16));
            let ihl = usize::from(p[0] & 0x0f) * 4;
            if p[9] == 1 && p.len() >= ihl + 8 {
                let i = &p[ihl..];
                s += &format!(" ICMP type {} id 0x{:04x} seq {}", i[0], be16(i, 4), be16(i, 6));
            }
        }
        0x86dd => s += ": IPv6",
        _ => {}
    }
    s
}

struct Wire {
    sock: UdpSocket,
    qemu: String,
    printed: usize,
    seen: usize,
}

impl Wire {
    fn send(&self, frame: &[u8]) {
        // QEMU may not be listening yet; the ARP loop simply retries.
        let _ = self.sock.send_to(frame, &self.qemu);
    }

    /// Wait up to `wait` for one frame.
    fn recv(&mut self, wait: Duration) -> Option<Vec<u8>> {
        let mut buf = [0u8; 65536];
        self.sock
            .set_read_timeout(Some(wait.max(Duration::from_millis(1))))
            .ok()?;
        match self.sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let f = buf[..n].to_vec();
                self.seen += 1;
                if self.printed < MAX_PRINTED {
                    self.printed += 1;
                    println!("    rx: {}", summarize(&f));
                }
                Some(f)
            }
            // A refused send earlier can surface here; it is not a frame.
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => None,
            Err(_) => None,
        }
    }
}

fn arp_request() -> Vec<u8> {
    let mut f = vec![0xff; 6];
    f.extend_from_slice(&PEER_MAC);
    f.extend_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0x00, 6, 4, 0, 1]);
    f.extend_from_slice(&PEER_MAC);
    f.extend_from_slice(&PEER_IP.octets());
    f.extend_from_slice(&[0; 6]);
    f.extend_from_slice(&DUT_IP.octets());
    f.resize(60, 0);
    f
}

/// If `f` is an ARP reply about DUT_IP, check it. Ok(None): not one.
fn check_arp_reply(f: &[u8]) -> Option<Result<(), String>> {
    if f.len() < 42 || be16(f, 12) != 0x0806 {
        return None;
    }
    let a = &f[14..];
    if be16(a, 6) != 2 || ip_at(a, 14) != DUT_IP {
        return None;
    }
    let mut wrong = Vec::new();
    if &f[0..6] != PEER_MAC {
        wrong.push(format!("Ethernet destination {} (expected {})", fmt_mac(&f[0..6]), fmt_mac(&PEER_MAC)));
    }
    if &f[6..12] != DUT_MAC {
        wrong.push(format!("Ethernet source {} (expected {})", fmt_mac(&f[6..12]), fmt_mac(&DUT_MAC)));
    }
    if (be16(a, 0), be16(a, 2), a[4], a[5]) != (1, 0x0800, 6, 4) {
        wrong.push(format!("htype/ptype/hlen/plen {}/0x{:04x}/{}/{} (expected 1/0x0800/6/4)", be16(a, 0), be16(a, 2), a[4], a[5]));
    }
    if &a[8..14] != DUT_MAC {
        wrong.push(format!("sender MAC {} (expected {})", fmt_mac(&a[8..14]), fmt_mac(&DUT_MAC)));
    }
    if &a[18..24] != PEER_MAC || ip_at(a, 24) != PEER_IP {
        wrong.push(format!("target {} {} (expected {} {PEER_IP})", fmt_mac(&a[18..24]), ip_at(a, 24), fmt_mac(&PEER_MAC)));
    }
    Some(if wrong.is_empty() { Ok(()) } else { Err(wrong.join("; ")) })
}

fn echo_request(seq: u16, data: &[u8]) -> Vec<u8> {
    let mut f = DUT_MAC.to_vec();
    f.extend_from_slice(&PEER_MAC);
    f.extend_from_slice(&[0x08, 0x00]);
    let mut ip = [0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&((28 + data.len()) as u16).to_be_bytes());
    ip[4..6].copy_from_slice(&seq.to_be_bytes());
    ip[8] = 64;
    ip[9] = 1;
    ip[12..16].copy_from_slice(&PEER_IP.octets());
    ip[16..20].copy_from_slice(&DUT_IP.octets());
    let c = checksum(&ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&ip);
    let mut icmp = vec![8, 0, 0, 0];
    icmp.extend_from_slice(&ICMP_ID.to_be_bytes());
    icmp.extend_from_slice(&seq.to_be_bytes());
    icmp.extend_from_slice(data);
    let c = checksum(&icmp);
    icmp[2..4].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&icmp);
    f
}

/// If `f` is an ICMP echo reply with our id and `seq`, check it.
fn check_echo_reply(f: &[u8], seq: u16, data: &[u8]) -> Option<Result<(), String>> {
    if f.len() < 34 || be16(f, 12) != 0x0800 {
        return None;
    }
    let p = &f[14..];
    let ihl = usize::from(p[0] & 0x0f) * 4;
    if p[9] != 1 || ihl < 20 || p.len() < ihl + 8 {
        return None;
    }
    let i = &p[ihl..];
    if i[0] != 0 || be16(i, 4) != ICMP_ID || be16(i, 6) != seq {
        return None;
    }
    let mut wrong = Vec::new();
    if &f[0..6] != PEER_MAC {
        wrong.push(format!("Ethernet destination {} (expected {})", fmt_mac(&f[0..6]), fmt_mac(&PEER_MAC)));
    }
    if &f[6..12] != DUT_MAC {
        wrong.push(format!("Ethernet source {} (expected {})", fmt_mac(&f[6..12]), fmt_mac(&DUT_MAC)));
    }
    if checksum(&p[..ihl]) != 0 {
        wrong.push("IPv4 header checksum does not verify".into());
    }
    if ip_at(p, 12) != DUT_IP || ip_at(p, 16) != PEER_IP {
        wrong.push(format!("addresses {} -> {} (expected {DUT_IP} -> {PEER_IP})", ip_at(p, 12), ip_at(p, 16)));
    }
    let total = usize::from(be16(p, 2));
    if total < ihl + 8 || total > p.len() {
        wrong.push(format!("IPv4 total length {total} with {} bytes present", p.len()));
    } else {
        let icmp = &p[ihl..total];
        if checksum(icmp) != 0 {
            wrong.push("ICMP checksum does not verify".into());
        }
        if &icmp[8..] != data {
            let first = icmp[8..].iter().zip(data).position(|(a, b)| a != b);
            wrong.push(format!(
                "echo data differs: {} bytes back for {} sent, first difference at {:?}",
                icmp.len() - 8,
                data.len(),
                first
            ));
        }
    }
    Some(if wrong.is_empty() { Ok(()) } else { Err(wrong.join("; ")) })
}

struct Args {
    ports_file: String,
    stop_file: Option<String>,
    deadline: Duration,
    pings: u16,
    bulk: u16,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let (mut ports_file, mut stop_file, mut deadline, mut pings, mut bulk) =
        (None, None, Duration::from_secs(120), 3u16, 300u16);
    while let Some(a) = args.next() {
        let mut value = |n: &str| args.next().ok_or_else(|| format!("{n} needs a value"));
        match a.as_str() {
            "--ports-file" => ports_file = Some(value("--ports-file")?),
            "--stop-file" => stop_file = Some(value("--stop-file")?),
            "--deadline-s" => {
                let v = value("--deadline-s")?;
                deadline = Duration::from_secs(v.parse().map_err(|_| format!("--deadline-s {v}: not a number"))?);
            }
            "--pings" => {
                let v = value("--pings")?;
                pings = v.parse().map_err(|_| format!("--pings {v}: not a number"))?;
            }
            "--bulk" => {
                let v = value("--bulk")?;
                bulk = v.parse().map_err(|_| format!("--bulk {v}: not a number"))?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if u32::from(pings) + u32::from(bulk) > u32::from(u16::MAX) {
        return Err(format!("--pings {pings} plus --bulk {bulk}: more than 65535 sequence numbers"));
    }
    Ok(Args { ports_file: ports_file.ok_or("--ports-file is required")?, stop_file, deadline, pings, bulk })
}

fn run(args: &Args) -> Result<(), ()> {
    let fail = |s: &str, e: &str, g: &str| {
        step(s, false, e, g);
    };
    let sock = UdpSocket::bind("127.0.0.1:0").map_err(|e| fail("bind", "a UDP socket on 127.0.0.1", &e.to_string()))?;
    let peer_port = sock.local_addr().map_err(|e| fail("bind", "a local address", &e.to_string()))?.port();
    // Reserve a port for QEMU: bind, read it, release it.
    let qemu_port = UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map_err(|e| fail("bind", "a free port for QEMU", &e.to_string()))?
        .port();
    std::fs::write(&args.ports_file, format!("{peer_port} {qemu_port}\n"))
        .map_err(|e| fail("ports-file", &format!("{} is writable", args.ports_file), &e.to_string()))?;
    let mut wire = Wire { sock, qemu: format!("127.0.0.1:{qemu_port}"), printed: 0, seen: 0 };
    let start = Instant::now();
    let stopped = || args.stop_file.as_ref().is_some_and(|f| std::path::Path::new(f).exists());

    // 1. ARP, repeated until answered: the VM boots while this runs.
    let expected = format!("an ARP reply: {DUT_IP} is-at {} to {} within {} s", fmt_mac(&DUT_MAC), fmt_mac(&PEER_MAC), args.deadline.as_secs());
    let mut next_arp = Instant::now();
    let mut requests = 0u32;
    loop {
        if start.elapsed() >= args.deadline {
            fail("arp", &expected, &format!("no ARP reply after {requests} requests; {} frames received in all", wire.seen));
            return Err(());
        }
        if stopped() {
            fail("arp", &expected, &format!("the VM powered off; no ARP reply after {requests} requests over {:.1} s; {} frames received in all", start.elapsed().as_secs_f32(), wire.seen));
            return Err(());
        }
        if Instant::now() >= next_arp {
            wire.send(&arp_request());
            requests += 1;
            next_arp = Instant::now() + ARP_INTERVAL;
        }
        let Some(f) = wire.recv(next_arp.saturating_duration_since(Instant::now())) else { continue };
        match check_arp_reply(&f) {
            None => {}
            Some(Ok(())) => {
                step("arp", true, &expected, &format!("reply after {requests} requests, {:.1} s", start.elapsed().as_secs_f32()));
                break;
            }
            Some(Err(why)) => {
                fail("arp", &expected, &why);
                return Err(());
            }
        }
    }

    // 2. Pings, one at a time.
    let mut ok = true;
    for seq in 1..=args.pings {
        let data: Vec<u8> = (0..PING_DATA).map(|i| (i as u8).wrapping_add(seq as u8 * 16)).collect();
        let name = format!("ping-{seq}");
        let expected = format!(
            "an echo reply from {DUT_IP} id 0x{ICMP_ID:04x} seq {seq} with the same {PING_DATA} data bytes within {} ms",
            PING_WAIT.as_millis()
        );
        let sent = Instant::now();
        wire.send(&echo_request(seq, &data));
        let verdict = loop {
            let left = PING_WAIT.saturating_sub(sent.elapsed());
            if left.is_zero() {
                break Err("no reply".to_string());
            }
            let Some(f) = wire.recv(left) else { continue };
            if let Some(r) = check_echo_reply(&f, seq, &data) {
                break r;
            }
        };
        match verdict {
            Ok(()) => step(&name, true, &expected, &format!("reply in {} us", sent.elapsed().as_micros())),
            Err(why) => {
                fail(&name, &expected, &why);
                ok = false;
            }
        }
    }
    // 3. Bulk: large pings, one at a time, until the first failure.
    if ok && args.bulk > 0 {
        let expected = format!(
            "{} echo replies, each with the same {BULK_MIN}-{} data bytes as its request, within {} ms each",
            args.bulk,
            BULK_MIN + BULK_SPAN - 1,
            PING_WAIT.as_millis()
        );
        let begun = Instant::now();
        let mut bytes = 0usize;
        for i in 0..args.bulk {
            let seq = args.pings + 1 + i;
            let len = bulk_len(seq);
            let data = bulk_data(seq, len);
            let sent = Instant::now();
            wire.send(&echo_request(seq, &data));
            let verdict = loop {
                let left = PING_WAIT.saturating_sub(sent.elapsed());
                if left.is_zero() {
                    break Err("no reply".to_string());
                }
                let Some(f) = wire.recv(left) else { continue };
                if let Some(r) = check_echo_reply(&f, seq, &data) {
                    break r;
                }
            };
            if let Err(why) = verdict {
                fail(
                    "bulk",
                    &expected,
                    &format!(
                        "ping {} of {} (seq {seq}, {len} data bytes) failed after {} replies ({bytes} data bytes each way): {why}",
                        i + 1,
                        args.bulk,
                        i
                    ),
                );
                ok = false;
                break;
            }
            bytes += len;
        }
        if ok {
            step(
                "bulk",
                true,
                &expected,
                &format!(
                    "{} replies, {bytes} data bytes each way, in {} ms",
                    args.bulk,
                    begun.elapsed().as_millis()
                ),
            );
        }
    }
    if wire.seen > wire.printed {
        println!("    ({} more frames not shown)", wire.seen - wire.printed);
    }
    if ok { Ok(()) } else { Err(()) }
}

/// Data length of bulk ping `seq`: varies so frames land at shifting offsets.
fn bulk_len(seq: u16) -> usize {
    BULK_MIN + (usize::from(seq) * 37) % BULK_SPAN
}

/// Data of bulk ping `seq`: differs per ping and per byte, so a reply that
/// carries another ping's data, or bytes from the wrong offset, cannot match.
fn bulk_data(seq: u16, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seq as u8).wrapping_add((i >> 8) as u8)).collect()
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            step("args", false, "--ports-file FILE [--deadline-s N] [--pings N] [--bulk N]", &e);
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => {
            step("peer", true, "valid ARP and ping replies", "all passed");
            ExitCode::SUCCESS
        }
        Err(()) => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the reply a correct responder would send, then corrupt it.
    fn good_echo_reply(seq: u16, data: &[u8]) -> Vec<u8> {
        let mut f = echo_request(seq, data);
        f[0..6].copy_from_slice(&PEER_MAC);
        f[6..12].copy_from_slice(&DUT_MAC);
        f[26..30].copy_from_slice(&DUT_IP.octets());
        f[30..34].copy_from_slice(&PEER_IP.octets());
        f[24..26].copy_from_slice(&[0, 0]);
        let c = checksum(&f[14..34]);
        f[24..26].copy_from_slice(&c.to_be_bytes());
        f[34] = 0;
        f[36..38].copy_from_slice(&[0, 0]);
        let c = checksum(&f[34..]);
        f[36..38].copy_from_slice(&c.to_be_bytes());
        f
    }

    #[test]
    fn accepts_a_correct_echo_reply() {
        let data = [7u8; 56];
        assert_eq!(check_echo_reply(&good_echo_reply(3, &data), 3, &data), Some(Ok(())));
    }

    #[test]
    fn rejects_changed_data_and_wrong_mac() {
        let data = [7u8; 56];
        let mut f = good_echo_reply(3, &data);
        let last = f.len() - 1;
        f[last] ^= 1;
        assert!(matches!(check_echo_reply(&f, 3, &data), Some(Err(_))));
        let mut f = good_echo_reply(3, &data);
        f[11] ^= 1;
        assert!(matches!(check_echo_reply(&f, 3, &data), Some(Err(_))));
    }

    #[test]
    fn bulk_pings_cover_the_size_range_and_differ() {
        let lens: Vec<usize> = (4..304u16).map(bulk_len).collect();
        assert!(lens.iter().all(|l| (BULK_MIN..BULK_MIN + BULK_SPAN).contains(l)));
        assert_eq!(*lens.iter().max().unwrap() + 42, 1514, "the largest frame is 1514 bytes");
        assert!(lens.windows(2).all(|w| w[0] != w[1]), "consecutive sizes differ");
        assert_ne!(bulk_data(4, 1000), bulk_data(5, 1000));
        // A full-size reply checks out like a small one.
        let data = bulk_data(9, 1472);
        assert_eq!(check_echo_reply(&good_echo_reply(9, &data), 9, &data), Some(Ok(())));
    }

    #[test]
    fn ignores_other_sequence_numbers_and_our_own_request() {
        let data = [7u8; 56];
        assert_eq!(check_echo_reply(&good_echo_reply(4, &data), 3, &data), None);
        assert_eq!(check_echo_reply(&echo_request(3, &data), 3, &data), None);
    }

    #[test]
    fn checks_arp_replies() {
        let mut f = PEER_MAC.to_vec();
        f.extend_from_slice(&DUT_MAC);
        f.extend_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0, 6, 4, 0, 2]);
        f.extend_from_slice(&DUT_MAC);
        f.extend_from_slice(&DUT_IP.octets());
        f.extend_from_slice(&PEER_MAC);
        f.extend_from_slice(&PEER_IP.octets());
        assert_eq!(check_arp_reply(&f), Some(Ok(())));
        f[22] ^= 1; // sender MAC
        assert!(matches!(check_arp_reply(&f), Some(Err(_))));
        assert_eq!(check_arp_reply(&arp_request()), None);
    }
}
