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
//! 3. Floods the driver with 2000 frames back to back, then sends 20
//!    rounds of hostile frames (shorter than a header, runts, oversized up
//!    to 9000 bytes, bad checksums, truncated packets); a recovery check
//!    follows each. The host fails the run if the driver hands it a frame
//!    outside 14-1514 bytes.
//! 4. With `--qmp SOCKET` (QEMU's monitor), drops the link three times
//!    (`set_link`): while it is down, pings must go unanswered; after it
//!    comes back, a recovery check (20 pings, the first answered within
//!    5 s, the rest within 2 s each) must pass. This forces the NIC
//!    robustness contract's link-loss items (docs/design/robust-nic.md).
//! 5. Sends the host a "done" control frame.
//!
//! ```text
//! nic-peer --ports-file FILE [--stop-file FILE] [--deadline-s N] [--pings N] [--bulk N]
//!          [--qmp SOCKET]
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

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
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
/// Control frames to grind-host (host/src/net.rs): EtherType 0x88B5, the
/// magic, then an op byte.
const ETHERTYPE_CONTROL: u16 = 0x88B5;
const CONTROL_MAGIC: &[u8; 8] = b"GRINDCTL";
const CONTROL_DONE: u8 = 1;

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

/// A control frame to the host: `op` and its argument bytes.
fn control_frame(op: u8, arg: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&DUT_MAC);
    f.extend_from_slice(&PEER_MAC);
    f.extend_from_slice(&ETHERTYPE_CONTROL.to_be_bytes());
    f.extend_from_slice(CONTROL_MAGIC);
    f.push(op);
    f.extend_from_slice(arg);
    f.resize(f.len().max(60), 0);
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
    qmp: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let (mut ports_file, mut stop_file, mut deadline, mut pings, mut bulk, mut qmp) =
        (None, None, Duration::from_secs(120), 3u16, 300u16, None);
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
            "--qmp" => qmp = Some(value("--qmp")?),
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
    Ok(Args { ports_file: ports_file.ok_or("--ports-file is required")?, stop_file, deadline, pings, bulk, qmp })
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
    // 3. A flood, then hostile frames.
    let mut seq = args.pings + args.bulk;
    if ok {
        ok = flood(&mut wire, &mut seq);
    }
    if ok {
        ok = hostile(&mut wire, &mut seq);
    }

    // 4. Link flaps, through QEMU's monitor.
    if ok {
        if let Some(path) = &args.qmp {
            ok = link_flaps(&mut wire, path, &mut seq);
        }
    }

    // The timeline is over: tell the host to stop serving.
    wire.send(&control_frame(CONTROL_DONE, &[ok as u8]));
    if wire.seen > wire.printed {
        println!("    ({} more frames not shown)", wire.seen - wire.printed);
    }
    if ok { Ok(()) } else { Err(()) }
}

/// Link flaps: how many, how long the link stays down each time (long
/// enough for the contract's "link-up false within 1 s" to be seen by the
/// host's 100 ms polling), and the pings sent meanwhile.
const FLAPS: u32 = 3;
const LINK_DOWN_FOR: Duration = Duration::from_secs(2);
const DOWN_PINGS: u32 = 3;
/// After raising the link, keep it up at least this long (the recovery
/// check runs inside it): the contract gives a driver 5 s to report the
/// link up again, and the host must see each up before the next drop.
const LINK_UP_HOLD: Duration = Duration::from_secs(5);
/// Recovery check: this many pings, the first answered within
/// RECOVER_FIRST (retried every RECOVER_RETRY), the rest within PING_WAIT.
const RECOVER_PINGS: u32 = 20;
const RECOVER_FIRST: Duration = Duration::from_secs(5);
const RECOVER_RETRY: Duration = Duration::from_millis(500);

/// A minimal QMP client: one command at a time, events skipped.
struct Qmp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Qmp {
    fn connect(path: &str) -> Result<Qmp, String> {
        let stream = UnixStream::connect(path).map_err(|e| format!("connect to {path}: {e}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut q = Qmp { stream, reader };
        let mut greeting = String::new();
        q.reader.read_line(&mut greeting).map_err(|e| format!("QMP greeting: {e}"))?;
        if !greeting.contains("\"QMP\"") {
            return Err(format!("QMP greeting: got {:?}", greeting.trim()));
        }
        q.command(r#"{"execute":"qmp_capabilities"}"#)?;
        Ok(q)
    }

    /// Send one command; Ok on `"return"`, Err with QEMU's reply otherwise.
    fn command(&mut self, json: &str) -> Result<(), String> {
        writeln!(self.stream, "{json}").map_err(|e| format!("QMP write: {e}"))?;
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).map_err(|e| format!("QMP read: {e}"))?;
            if n == 0 {
                return Err("QMP: the monitor closed the connection".into());
            }
            if line.contains("\"event\"") {
                continue;
            }
            return if line.contains("\"return\"") { Ok(()) } else { Err(format!("QMP {json}: {}", line.trim())) };
        }
    }

    fn set_link(&mut self, up: bool) -> Result<(), String> {
        self.command(&format!(r#"{{"execute":"set_link","arguments":{{"name":"net0","up":{up}}}}}"#))
    }
}

/// Send echo request `seq` with `data` and wait up to `wait` for its exact
/// reply. Ok with the round-trip time.
fn ping_once(wire: &mut Wire, seq: u16, data: &[u8], wait: Duration) -> Result<Duration, String> {
    let sent = Instant::now();
    wire.send(&echo_request(seq, data));
    loop {
        let left = wait.saturating_sub(sent.elapsed());
        if left.is_zero() {
            return Err("no reply".into());
        }
        let Some(f) = wire.recv(left) else { continue };
        if let Some(r) = check_echo_reply(&f, seq, data) {
            return r.map(|()| sent.elapsed());
        }
    }
}

/// The recovery check after a hostile phase: RECOVER_PINGS pings, the
/// first answered within RECOVER_FIRST, every other within PING_WAIT.
fn recovery(wire: &mut Wire, after: &str, seq: &mut u16) -> bool {
    let name = format!("recover-after-{after}");
    let expected = format!(
        "{RECOVER_PINGS} echo replies: the first within {} ms, the rest within {} ms each",
        RECOVER_FIRST.as_millis(),
        PING_WAIT.as_millis()
    );
    let data: Vec<u8> = (0..PING_DATA as u8).collect();
    let begun = Instant::now();
    let mut tries = 0;
    let first = loop {
        if begun.elapsed() >= RECOVER_FIRST {
            step(&name, false, &expected, &format!("no reply within {} ms ({tries} tries)", RECOVER_FIRST.as_millis()));
            return false;
        }
        *seq += 1;
        tries += 1;
        if ping_once(wire, *seq, &data, RECOVER_RETRY).is_ok() {
            break begun.elapsed();
        }
    };
    for i in 2..=RECOVER_PINGS {
        *seq += 1;
        if let Err(why) = ping_once(wire, *seq, &data, PING_WAIT) {
            step(&name, false, &expected, &format!("first reply after {} ms, then ping {i} (seq {seq}) failed: {why}", first.as_millis()));
            return false;
        }
    }
    step(&name, true, &expected, &format!("first reply after {} ms ({tries} tries), then {} more", first.as_millis(), RECOVER_PINGS - 1));
    true
}

/// Frames in the flood phase.
const FLOOD_FRAMES: u32 = 2000;
/// Rounds of the hostile-frame set.
const HOSTILE_ROUNDS: u32 = 20;
/// After sending a phase's frames, how long to drain replies before the
/// recovery check.
const DRAIN: Duration = Duration::from_millis(500);

/// Receive and discard whatever arrives for `d`.
fn drain(wire: &mut Wire, d: Duration) -> u32 {
    let until = Instant::now() + d;
    let mut n = 0;
    while let Some(_f) = wire.recv(until.saturating_duration_since(Instant::now())) {
        n += 1;
        if Instant::now() >= until {
            break;
        }
    }
    n
}

/// A frame from us to the driver's MAC with this EtherType and payload.
fn raw_frame(ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = DUT_MAC.to_vec();
    f.extend_from_slice(&PEER_MAC);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// Flood: FLOOD_FRAMES frames back to back, without waiting: pings of
/// varying size (answered), ARP for other addresses and an unknown
/// EtherType (ignored). Then the recovery check.
fn flood(wire: &mut Wire, seq: &mut u16) -> bool {
    let begun = Instant::now();
    for i in 0..FLOOD_FRAMES {
        let f = match i % 4 {
            0 | 1 => {
                *seq += 1;
                let len = 18 + (i as usize * 97) % 1455;
                echo_request(*seq, &bulk_data(*seq, len))
            }
            2 => {
                let mut a = arp_request();
                a[41] = (i % 250) as u8 + 1; // who-has 10.0.2.x, not us
                a
            }
            _ => raw_frame(0x88B6, &vec![i as u8; 46 + (i as usize % 200)]),
        };
        wire.send(&f);
    }
    let sent_ms = begun.elapsed().as_millis();
    let back = drain(wire, DRAIN);
    step(
        "flood",
        true,
        &format!("{FLOOD_FRAMES} frames sent back to back"),
        &format!("sent in {sent_ms} ms; {back} frames came back while draining"),
    );
    recovery(wire, "flood", seq)
}

/// One round of hostile frames: what a driver must neither hand up (outside
/// 14-1514 bytes) nor be broken by.
fn hostile_frames(seq: u16) -> Vec<(&'static str, Vec<u8>)> {
    let mut bad_ip = echo_request(seq, &[1; 32]);
    bad_ip[24] ^= 0xff; // IPv4 header checksum
    let mut bad_icmp = echo_request(seq, &[2; 32]);
    bad_icmp[36] ^= 0xff; // ICMP checksum
    let mut truncated = echo_request(seq, &[3; 32]);
    truncated[16..18].copy_from_slice(&1400u16.to_be_bytes()); // total length past the end
    let mut bad_arp = arp_request();
    bad_arp[18] = 9; // hardware address length
    vec![
        ("1 byte", vec![0x42]),
        ("13 bytes, shorter than a header", DUT_MAC.iter().chain(&PEER_MAC).chain(&[0x08]).copied().collect()),
        ("a bare header", raw_frame(0x88B6, &[])),
        ("a 59-byte runt", raw_frame(0x88B6, &[7; 45])),
        ("1515 bytes, one over", raw_frame(0x88B6, &[8; 1501])),
        ("a 1600-byte ping", echo_request(seq, &[9; 1558])),
        ("4000 bytes", raw_frame(0x88B6, &[10; 3986])),
        ("9000 bytes, jumbo", raw_frame(0x88B6, &[11; 8986])),
        ("IPv4 with a bad header checksum", bad_ip),
        ("ICMP with a bad checksum", bad_icmp),
        ("IPv4 truncated", truncated),
        ("ARP with a bad address length", bad_arp),
    ]
}

/// HOSTILE_ROUNDS rounds of hostile frames, then the recovery check.
fn hostile(wire: &mut Wire, seq: &mut u16) -> bool {
    let mut names = Vec::new();
    let mut sent = 0;
    for _ in 0..HOSTILE_ROUNDS {
        *seq += 1;
        for (name, f) in hostile_frames(*seq) {
            wire.send(&f);
            sent += 1;
            if names.len() < 12 {
                names.push(name);
            }
        }
    }
    let back = drain(wire, DRAIN);
    step(
        "hostile",
        true,
        &format!("{sent} hostile frames sent: {}", names.join(", ")),
        &format!("sent; {back} frames came back while draining"),
    );
    recovery(wire, "hostile", seq)
}

/// Drop the link FLAPS times; while it is down, pings must go unanswered;
/// after each, a recovery check.
fn link_flaps(wire: &mut Wire, qmp_path: &str, seq: &mut u16) -> bool {
    let mut qmp = match Qmp::connect(qmp_path) {
        Ok(q) => q,
        Err(e) => {
            step("link-flap", false, "a QMP connection to QEMU's monitor", &e);
            return false;
        }
    };
    let data: Vec<u8> = (0..PING_DATA as u8).collect();
    for k in 1..=FLAPS {
        let name = format!("link-down-{k}");
        let expected = format!(
            "the link down for {} ms (QMP set_link), and no reply to {DOWN_PINGS} pings meanwhile",
            LINK_DOWN_FOR.as_millis()
        );
        if let Err(e) = qmp.set_link(false) {
            step(&name, false, &expected, &e);
            return false;
        }
        let down_at = Instant::now();
        for _ in 0..DOWN_PINGS {
            *seq += 1;
            if ping_once(wire, *seq, &data, LINK_DOWN_FOR / DOWN_PINGS).is_ok() {
                let _ = qmp.set_link(true);
                step(&name, false, &expected, &format!("seq {seq} was answered while the link was down: the test cannot drop the link"));
                return false;
            }
        }
        std::thread::sleep(LINK_DOWN_FOR.saturating_sub(down_at.elapsed()));
        if let Err(e) = qmp.set_link(true) {
            step(&name, false, &expected, &format!("raising the link again: {e}"));
            return false;
        }
        let up_at = Instant::now();
        step(&name, true, &expected, &format!("down for {} ms, {DOWN_PINGS} pings unanswered", down_at.elapsed().as_millis()));
        if !recovery(wire, &format!("link-flap-{k}"), seq) {
            return false;
        }
        std::thread::sleep(LINK_UP_HOLD.saturating_sub(up_at.elapsed()));
    }
    true
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
    fn hostile_frames_are_what_they_say() {
        let set = hostile_frames(7);
        let len = |n: &str| set.iter().find(|(name, _)| *name == n).unwrap().1.len();
        assert_eq!(len("1 byte"), 1);
        assert_eq!(len("13 bytes, shorter than a header"), 13);
        assert_eq!(len("a bare header"), 14);
        assert_eq!(len("a 59-byte runt"), 59);
        assert_eq!(len("1515 bytes, one over"), 1515);
        assert_eq!(len("a 1600-byte ping"), 1600);
        assert_eq!(len("9000 bytes, jumbo"), 9000);
        for n in ["IPv4 with a bad header checksum", "ICMP with a bad checksum", "IPv4 truncated"] {
            let f = &set.iter().find(|(name, _)| *name == n).unwrap().1;
            assert_eq!(check_echo_reply(f, 7, &[]).map(|r| r.is_ok()), None, "{n} is not a reply");
        }
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
