// SPDX-License-Identifier: AGPL-3.0-or-later

//! A minimal stand-in for a network stack: answers ARP requests and ICMP
//! echo requests for one IPv4 address. It runs in the host, on frames the
//! driver hands over, so a NIC driver only moves frames.
//!
//! Written from RFC 826 (ARP), RFC 791 (IPv4), RFC 792 (ICMP) and RFC 1071
//! (the Internet checksum).

use std::net::Ipv4Addr;

pub type Mac = [u8; 6];

const ETH_HEADER: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const ARP_LEN: usize = 28;
const IP_PROTO_ICMP: u8 = 1;
const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;

/// EtherType of the NIC test's control frames: 0x88B5, which IEEE 802
/// reserves for local experimental use, so no real protocol uses it.
pub const ETHERTYPE_CONTROL: u16 = 0x88B5;
/// Control frames start their payload with this, so a stray frame with the
/// same EtherType is not taken for one.
pub const CONTROL_MAGIC: &[u8; 8] = b"GRINDCTL";

/// A message from the test's peer to the host (docs/design/robust-nic.md),
/// never handed to the responder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The peer's timeline is over: stop serving. `complete` is false if
    /// the peer stopped early because a step failed.
    Done { complete: bool },
    /// Open a device-fault window (host/src/faults.rs): `ms` long, the
    /// `kinds` bits, at `per_mille` of the calls they apply to.
    Faults { ms: u32, kinds: u8, per_mille: u16 },
}

impl Control {
    pub fn name(self) -> &'static str {
        match self {
            Control::Done { complete: true } => "done",
            Control::Done { complete: false } => "done-early",
            Control::Faults { .. } => "faults",
        }
    }
}

/// The control message in `frame`, if it is one: EtherType 0x88B5, then
/// `CONTROL_MAGIC`, then one op byte: 1 = done, then 1 byte (1 if the
/// timeline completed, 0 if it stopped early); 2 = faults, then the window
/// in ms (u32), the kinds (u8) and the rate per mille (u16), big-endian.
pub fn control(frame: &[u8]) -> Option<Control> {
    if frame.len() < ETH_HEADER + CONTROL_MAGIC.len() + 1
        || u16::from_be_bytes([frame[12], frame[13]]) != ETHERTYPE_CONTROL
        || &frame[ETH_HEADER..ETH_HEADER + CONTROL_MAGIC.len()] != CONTROL_MAGIC
    {
        return None;
    }
    match frame[ETH_HEADER + CONTROL_MAGIC.len()] {
        1 => Some(Control::Done {
            complete: frame.get(ETH_HEADER + CONTROL_MAGIC.len() + 1) == Some(&1),
        }),
        2 => {
            let a = frame.get(ETH_HEADER + CONTROL_MAGIC.len() + 1..ETH_HEADER + CONTROL_MAGIC.len() + 8)?;
            Some(Control::Faults {
                ms: u32::from_be_bytes([a[0], a[1], a[2], a[3]]),
                kinds: a[4],
                per_mille: u16::from_be_bytes([a[5], a[6]]),
            })
        }
        _ => None,
    }
}

/// What the responder made of one received frame.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Answer with this frame. `summary` says what was answered.
    Reply { frame: Vec<u8>, summary: String },
    /// Not for us, or not something we answer. `summary` describes the frame.
    Ignored { summary: String },
    /// Addressed to us but malformed: `reason` says what was wrong.
    Malformed { reason: String },
}

pub fn fmt_mac(m: &Mac) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn ip_at(b: &[u8], at: usize) -> Ipv4Addr {
    Ipv4Addr::new(b[at], b[at + 1], b[at + 2], b[at + 3])
}

/// RFC 1071 one's-complement checksum. A buffer that already holds a valid
/// checksum sums to 0.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for chunk in data.chunks(2) {
        let word = if chunk.len() == 2 {
            u16::from_be_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], 0])
        };
        sum += u32::from(word);
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

pub struct Responder {
    pub mac: Mac,
    pub ip: Ipv4Addr,
}

impl Responder {
    pub fn handle(&self, frame: &[u8]) -> Outcome {
        if frame.len() < ETH_HEADER {
            return Outcome::Malformed {
                reason: format!("{} bytes, shorter than an Ethernet header (14)", frame.len()),
            };
        }
        let dst: Mac = frame[0..6].try_into().unwrap();
        let src: Mac = frame[6..12].try_into().unwrap();
        let ethertype = be16(frame, 12);
        let to_us = dst == self.mac || dst == [0xff; 6];
        let base = format!(
            "{} -> {} type 0x{ethertype:04x} len {}",
            fmt_mac(&src),
            fmt_mac(&dst),
            frame.len()
        );
        if !to_us {
            return Outcome::Ignored {
                summary: format!("{base}: not addressed to {} or broadcast", fmt_mac(&self.mac)),
            };
        }
        match ethertype {
            ETHERTYPE_ARP => self.arp(frame, base),
            ETHERTYPE_IPV4 if dst == self.mac => self.ipv4(frame, src, base),
            _ => Outcome::Ignored { summary: base },
        }
    }

    fn arp(&self, frame: &[u8], base: String) -> Outcome {
        let a = &frame[ETH_HEADER..];
        if a.len() < ARP_LEN {
            return Outcome::Malformed {
                reason: format!("{base}: ARP body is {} bytes, expected at least 28", a.len()),
            };
        }
        let (htype, ptype, hlen, plen, oper) = (be16(a, 0), be16(a, 2), a[4], a[5], be16(a, 6));
        if (htype, ptype, hlen, plen) != (1, ETHERTYPE_IPV4, 6, 4) {
            return Outcome::Ignored {
                summary: format!("{base}: ARP for htype {htype} ptype 0x{ptype:04x}"),
            };
        }
        let sha: Mac = a[8..14].try_into().unwrap();
        let spa = ip_at(a, 14);
        let tpa = ip_at(a, 24);
        if oper != 1 || tpa != self.ip {
            return Outcome::Ignored {
                summary: format!("{base}: ARP op {oper} who-has {tpa} tell {spa}"),
            };
        }
        let mut out = Vec::with_capacity(ETH_HEADER + ARP_LEN);
        out.extend_from_slice(&sha);
        out.extend_from_slice(&self.mac);
        out.extend_from_slice(&ETHERTYPE_ARP.to_be_bytes());
        out.extend_from_slice(&[0, 1, 0x08, 0x00, 6, 4, 0, 2]);
        out.extend_from_slice(&self.mac);
        out.extend_from_slice(&self.ip.octets());
        out.extend_from_slice(&sha);
        out.extend_from_slice(&spa.octets());
        Outcome::Reply {
            frame: out,
            summary: format!("ARP {tpa} is-at {} (asked by {spa})", fmt_mac(&self.mac)),
        }
    }

    fn ipv4(&self, frame: &[u8], src: Mac, base: String) -> Outcome {
        let p = &frame[ETH_HEADER..];
        if p.len() < 20 || p[0] >> 4 != 4 {
            return Outcome::Malformed {
                reason: format!("{base}: not an IPv4 header"),
            };
        }
        let ihl = usize::from(p[0] & 0x0f) * 4;
        let total = usize::from(be16(p, 2));
        if ihl < 20 || total < ihl || total > p.len() {
            return Outcome::Malformed {
                reason: format!(
                    "{base}: IPv4 header length {ihl}, total length {total}, {} bytes present",
                    p.len()
                ),
            };
        }
        if checksum(&p[..ihl]) != 0 {
            return Outcome::Malformed {
                reason: format!("{base}: IPv4 header checksum does not verify"),
            };
        }
        let (proto, saddr, daddr) = (p[9], ip_at(p, 12), ip_at(p, 16));
        if daddr != self.ip || proto != IP_PROTO_ICMP {
            return Outcome::Ignored {
                summary: format!("{base}: IPv4 proto {proto} {saddr} -> {daddr}"),
            };
        }
        let icmp = &p[ihl..total];
        if icmp.len() < 8 {
            return Outcome::Malformed {
                reason: format!("{base}: ICMP message is {} bytes, expected at least 8", icmp.len()),
            };
        }
        if checksum(icmp) != 0 {
            return Outcome::Malformed {
                reason: format!("{base}: ICMP checksum does not verify"),
            };
        }
        if icmp[0] != ICMP_ECHO_REQUEST || icmp[1] != 0 {
            return Outcome::Ignored {
                summary: format!("{base}: ICMP type {} code {} from {saddr}", icmp[0], icmp[1]),
            };
        }
        let (id, seq) = (be16(icmp, 4), be16(icmp, 6));

        // Reply: a fresh 20-byte IPv4 header (options are not echoed).
        let reply_icmp_len = icmp.len();
        let mut out = Vec::with_capacity(ETH_HEADER + 20 + reply_icmp_len);
        out.extend_from_slice(&src);
        out.extend_from_slice(&self.mac);
        out.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        let mut ip = [0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&((20 + reply_icmp_len) as u16).to_be_bytes());
        ip[4..6].copy_from_slice(&p[4..6]); // identification
        ip[8] = 64; // TTL
        ip[9] = IP_PROTO_ICMP;
        ip[12..16].copy_from_slice(&self.ip.octets());
        ip[16..20].copy_from_slice(&saddr.octets());
        let c = checksum(&ip);
        ip[10..12].copy_from_slice(&c.to_be_bytes());
        out.extend_from_slice(&ip);
        let at = out.len();
        out.extend_from_slice(icmp);
        out[at] = ICMP_ECHO_REPLY;
        out[at + 2] = 0;
        out[at + 3] = 0;
        let c = checksum(&out[at..]);
        out[at + 2..at + 4].copy_from_slice(&c.to_be_bytes());
        Outcome::Reply {
            frame: out,
            summary: format!(
                "ICMP echo reply to {saddr} id 0x{id:04x} seq {seq} ({} data bytes)",
                reply_icmp_len - 8
            ),
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn control_frames() {
        let mut f = vec![0xffu8; 12];
        f.extend_from_slice(&ETHERTYPE_CONTROL.to_be_bytes());
        f.extend_from_slice(CONTROL_MAGIC);
        f.push(1);
        f.push(1);
        assert_eq!(control(&f), Some(Control::Done { complete: true }));
        let mut early = f.clone();
        *early.last_mut().unwrap() = 0;
        assert_eq!(control(&early), Some(Control::Done { complete: false }));
        let mut unknown = f.clone();
        unknown[22] = 9;
        assert_eq!(control(&unknown), None);
        let mut no_magic = f.clone();
        no_magic[14] = b'X';
        assert_eq!(control(&no_magic), None);
        let mut ipv4 = f.clone();
        ipv4[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        assert_eq!(control(&ipv4), None);
        assert_eq!(control(&f[..20]), None, "too short");
        let mut faults = f[..22].to_vec();
        faults.push(2);
        faults.extend_from_slice(&1500u32.to_be_bytes());
        faults.push(3);
        faults.extend_from_slice(&200u16.to_be_bytes());
        assert_eq!(
            control(&faults),
            Some(Control::Faults { ms: 1500, kinds: 3, per_mille: 200 })
        );
        assert_eq!(control(&faults[..faults.len() - 1]), None, "truncated");
    }

    use super::*;

    const US: Mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const PEER: Mac = [0x02, 0, 0, 0, 0, 0x02];
    const US_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
    const PEER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

    fn responder() -> Responder {
        Responder { mac: US, ip: US_IP }
    }

    fn arp_request(target: Ipv4Addr) -> Vec<u8> {
        let mut f = vec![0xff; 6];
        f.extend_from_slice(&PEER);
        f.extend_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0, 6, 4, 0, 1]);
        f.extend_from_slice(&PEER);
        f.extend_from_slice(&PEER_IP.octets());
        f.extend_from_slice(&[0; 6]);
        f.extend_from_slice(&target.octets());
        f.resize(60, 0); // padded, as on the wire
        f
    }

    fn echo_request(dst_mac: Mac, data: &[u8]) -> Vec<u8> {
        let mut f = dst_mac.to_vec();
        f.extend_from_slice(&PEER);
        f.extend_from_slice(&[0x08, 0x00]);
        let mut ip = [0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&((28 + data.len()) as u16).to_be_bytes());
        ip[8] = 64;
        ip[9] = 1;
        ip[12..16].copy_from_slice(&PEER_IP.octets());
        ip[16..20].copy_from_slice(&US_IP.octets());
        let c = checksum(&ip);
        ip[10..12].copy_from_slice(&c.to_be_bytes());
        f.extend_from_slice(&ip);
        let mut icmp = vec![8, 0, 0, 0, 0x67, 0x72, 0, 7];
        icmp.extend_from_slice(data);
        let c = checksum(&icmp);
        icmp[2..4].copy_from_slice(&c.to_be_bytes());
        f.extend_from_slice(&icmp);
        f
    }

    #[test]
    fn checksum_matches_rfc1071_example() {
        // RFC 1071 section 3: 00 01 f2 03 f4 f5 f6 f7 sums to 0xddf2.
        assert_eq!(checksum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]), !0xddf2);
    }

    #[test]
    fn answers_arp_for_our_address() {
        let Outcome::Reply { frame, .. } = responder().handle(&arp_request(US_IP)) else {
            panic!("expected a reply");
        };
        assert_eq!(frame.len(), 42);
        assert_eq!(&frame[0..6], &PEER);
        assert_eq!(&frame[6..12], &US);
        assert_eq!(&frame[12..14], &[0x08, 0x06]);
        assert_eq!(&frame[20..22], &[0, 2]); // is-at
        assert_eq!(&frame[22..28], &US);
        assert_eq!(&frame[28..32], &US_IP.octets());
        assert_eq!(&frame[32..38], &PEER);
        assert_eq!(&frame[38..42], &PEER_IP.octets());
    }

    #[test]
    fn ignores_arp_for_other_addresses() {
        let out = responder().handle(&arp_request(Ipv4Addr::new(10, 0, 2, 99)));
        assert!(matches!(out, Outcome::Ignored { .. }), "{out:?}");
    }

    #[test]
    fn answers_echo_request() {
        let data: Vec<u8> = (0..56).collect();
        let Outcome::Reply { frame, .. } = responder().handle(&echo_request(US, &data)) else {
            panic!("expected a reply");
        };
        assert_eq!(&frame[0..6], &PEER);
        assert_eq!(&frame[6..12], &US);
        let ip = &frame[14..34];
        assert_eq!(checksum(ip), 0);
        assert_eq!(&ip[12..16], &US_IP.octets());
        assert_eq!(&ip[16..20], &PEER_IP.octets());
        let icmp = &frame[34..];
        assert_eq!(checksum(icmp), 0);
        assert_eq!(icmp[0], 0);
        assert_eq!(&icmp[4..8], &[0x67, 0x72, 0, 7]);
        assert_eq!(&icmp[8..], &data[..]);
    }

    #[test]
    fn rejects_bad_icmp_checksum() {
        let mut f = echo_request(US, &[1, 2, 3, 4]);
        let last = f.len() - 1;
        f[last] ^= 0xff;
        assert!(matches!(responder().handle(&f), Outcome::Malformed { .. }));
    }

    #[test]
    fn ignores_ip_to_other_macs_and_runts() {
        let f = echo_request([2, 0, 0, 0, 0, 9], &[]);
        assert!(matches!(responder().handle(&f), Outcome::Ignored { .. }));
        assert!(matches!(responder().handle(&[0; 10]), Outcome::Malformed { .. }));
    }

    #[test]
    fn tolerates_ethernet_padding_after_ip_packet() {
        let mut f = echo_request(US, &[]);
        f.resize(60, 0);
        let Outcome::Reply { frame, .. } = responder().handle(&f) else {
            panic!("expected a reply");
        };
        assert_eq!(frame.len(), 14 + 20 + 8);
    }
}
