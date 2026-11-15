// SPDX-License-Identifier: AGPL-3.0-or-later

//! Serving a network driver (`--serve-ping`): poll `receive`, answer ARP
//! and ICMP echo through `transmit` (frames: `net.rs`), count what happens.

use super::*;

/// Per-kind cap on routine log lines while serving. A flood of thousands of
/// frames must not stall the VM on its serial console; past the cap, events
/// are only counted, and the counts are logged at the end.
const LOG_CAP: u64 = 20;
/// How often the host asks the driver whether the link is up.
const LINK_POLL: Duration = Duration::from_millis(100);

/// Counts of serve events, logging only the first `LOG_CAP` of each kind.
#[derive(Default)]
struct Tally(std::collections::BTreeMap<&'static str, u64>);

impl Tally {
    /// Count one `kind` event; true if it should still be logged.
    fn log(&mut self, kind: &'static str) -> bool {
        let n = self.0.entry(kind).or_default();
        *n += 1;
        *n <= LOG_CAP
    }

    fn get(&self, kind: &str) -> u64 {
        self.0.get(kind).copied().unwrap_or(0)
    }
}

/// What serving needs to know beyond the address.
pub struct ServeOptions {
    pub ip: Ipv4Addr,
    pub duration: Duration,
    /// Link drops the test will cause; the run fails if the driver reports
    /// fewer (robustness contract: link loss reported within 1 s).
    pub expect_link_flaps: u64,
}

/// A driver error while serving: logged, and fatal only if it says nothing
/// (rule 3). Robustness contract: reporting "link down" or "ring full" is
/// correct behaviour, not a failure.
fn serve_driver_error(tally: &mut Tally, call: &str, msg: &str) -> Result<(), &'static str> {
    if msg.trim().is_empty() {
        emit(
            "driver-error-unreadable",
            &[
                ("call", Value::Str(call)),
                (
                    "expected",
                    Value::Str("an error saying what was attempted, expected and got (rule 3)"),
                ),
                ("got", Value::Str("an empty error")),
            ],
        );
        return Err("driver-error-unreadable");
    }
    if tally.log("driver-error") {
        emit(
            "driver-error",
            &[
                ("call", Value::Str(call)),
                ("got", Value::Str(msg)),
                ("fatal", Value::Bool(false)),
            ],
        );
    }
    Ok(())
}

/// After `start`: poll the driver for frames and answer ARP and ping for
/// `ip` until the peer says it is done or `duration` has passed. Returns
/// the failure reason, if any.
///
/// The verdict (docs/design/robust-nic.md): a trap, a timeout, an empty
/// error, a frame outside 14-1514 bytes, DMA memory growing after `start`,
/// or fewer link drops seen than the test caused. Everything else, such as
/// whether each recovery succeeded, is judged by the peer.
pub(crate) fn serve(
    store: &mut Store<State>,
    nic: &Nic,
    opts: &ServeOptions,
) -> Result<(), &'static str> {
    let eth = nic.grind_portals_ethernet();
    let ip = opts.ip;

    set_deadline(store, CALL_LIMIT);
    let m = eth
        .call_mac_address(&mut *store)
        .map_err(|e| call_failed("mac-address", CALL_LIMIT, e))?;
    let mac: net::Mac = [m.0, m.1, m.2, m.3, m.4, m.5];
    set_deadline(store, CALL_LIMIT);
    let mut link_up = eth
        .call_link_up(&mut *store)
        .map_err(|e| call_failed("link-up", CALL_LIMIT, e))?;
    let dma_after_start = store.data().device.dma().used();
    emit(
        "serve-start",
        &[
            ("ipv4", Value::Str(&ip.to_string())),
            ("mac", Value::Str(&net::fmt_mac(&mac))),
            ("link_up", Value::Bool(link_up)),
            ("serve_ms", Value::U64(opts.duration.as_millis() as u64)),
            ("dma_bytes", Value::U64(dma_after_start)),
            ("expect_link_flaps", Value::U64(opts.expect_link_flaps)),
        ],
    );
    if !link_up {
        emit(
            "link-down",
            &[
                (
                    "expected",
                    Value::Str("link-up returns true once start has returned ok"),
                ),
                ("got", Value::Str("false")),
            ],
        );
        return Err("link-down");
    }

    let responder = net::Responder { mac, ip };
    let mut tally = Tally::default();
    let (mut link_drops, mut done, mut peer_complete) = (0u64, false, false);
    let begun = Instant::now();
    let end = begun + opts.duration;
    let mut next_link_poll = Instant::now() + LINK_POLL;
    while Instant::now() < end && !done {
        if Instant::now() >= next_link_poll {
            next_link_poll = Instant::now() + LINK_POLL;
            set_deadline(store, CALL_LIMIT);
            let now_up = eth
                .call_link_up(&mut *store)
                .map_err(|e| call_failed("link-up", CALL_LIMIT, e))?;
            if now_up != link_up {
                link_up = now_up;
                if !now_up {
                    link_drops += 1;
                }
                emit(
                    "link-change",
                    &[
                        ("link_up", Value::Bool(now_up)),
                        ("at_ms", Value::U64(begun.elapsed().as_millis() as u64)),
                    ],
                );
            }
        }
        set_deadline(store, CALL_LIMIT);
        let frame = match eth.call_receive(&mut *store) {
            Err(e) => return Err(call_failed("receive", CALL_LIMIT, e)),
            Ok(Err(msg)) => {
                serve_driver_error(&mut tally, "receive", &msg)?;
                std::thread::sleep(IDLE_POLL);
                continue;
            }
            Ok(Ok(None)) => {
                std::thread::sleep(IDLE_POLL);
                continue;
            }
            Ok(Ok(Some(frame))) => frame,
        };
        tally.log("received");
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) {
            emit(
                "frame-invalid",
                &[
                    ("call", Value::Str("receive")),
                    (
                        "expected",
                        Value::Str("a frame of 14 to 1514 bytes, without FCS"),
                    ),
                    ("got", Value::Str(&format!("{} bytes", frame.len()))),
                    ("head", Value::Str(&hex_head(&frame))),
                ],
            );
            return Err("frame-invalid");
        }
        if let Some(control) = net::control(&frame) {
            emit(
                "control",
                &[
                    ("op", Value::Str(control.name())),
                    ("at_ms", Value::U64(begun.elapsed().as_millis() as u64)),
                ],
            );
            match control {
                net::Control::Done { complete } => {
                    done = true;
                    peer_complete = complete;
                }
                net::Control::Faults {
                    ms,
                    kinds,
                    per_mille,
                } => store.data_mut().faults.open(ms, kinds, per_mille),
            }
            continue;
        }
        match responder.handle(&frame) {
            net::Outcome::Reply {
                frame: reply,
                summary,
            } => {
                set_deadline(store, CALL_LIMIT);
                match eth.call_transmit(&mut *store, &reply) {
                    Err(e) => return Err(call_failed("transmit", CALL_LIMIT, e)),
                    Ok(Err(msg)) => {
                        serve_driver_error(&mut tally, "transmit", &msg)?;
                        continue;
                    }
                    Ok(Ok(())) => {}
                }
                if tally.log("answered") {
                    emit(
                        "answered",
                        &[
                            ("summary", Value::Str(&summary)),
                            ("len", Value::U64(reply.len() as u64)),
                        ],
                    );
                }
            }
            net::Outcome::Ignored { summary } => {
                if tally.log("ignored") {
                    emit("frame-ignored", &[("summary", Value::Str(&summary))]);
                }
            }
            net::Outcome::Malformed { reason } => {
                if tally.log("malformed") {
                    emit(
                        "frame-malformed",
                        &[
                            ("reason", Value::Str(&reason)),
                            ("head", Value::Str(&hex_head(&frame))),
                        ],
                    );
                }
            }
        }
    }
    store.data_mut().faults.close();
    let dma_at_end = store.data().device.dma().used();
    emit(
        "serve-end",
        &[
            ("ended_by", Value::Str(if done { "peer" } else { "time" })),
            ("frames_received", Value::U64(tally.get("received"))),
            ("frames_answered", Value::U64(tally.get("answered"))),
            ("frames_ignored", Value::U64(tally.get("ignored"))),
            ("frames_malformed", Value::U64(tally.get("malformed"))),
            ("driver_errors", Value::U64(tally.get("driver-error"))),
            ("link_drops", Value::U64(link_drops)),
            (
                "faults_registers",
                Value::U64(store.data().faults.injected_registers),
            ),
            (
                "faults_device_data",
                Value::U64(store.data().faults.injected_device_data),
            ),
            ("faults_slow", Value::U64(store.data().faults.injected_slow)),
            ("dma_bytes_after_start", Value::U64(dma_after_start)),
            ("dma_bytes_at_end", Value::U64(dma_at_end)),
            ("log_cap_per_kind", Value::U64(LOG_CAP)),
        ],
    );
    if dma_at_end > dma_after_start {
        emit(
            "dma-growth",
            &[
                (
                    "expected",
                    Value::Str("no growth in DMA memory after start (robustness contract)"),
                ),
                (
                    "got",
                    Value::Str(&format!(
                        "{dma_after_start} bytes after start, {dma_at_end} at the end"
                    )),
                ),
            ],
        );
        return Err("dma-growth");
    }
    // The peer stopped its timeline at a failed step: its log says which.
    // The checks below assume the whole timeline ran, so they would only
    // point at the wrong thing.
    if done && !peer_complete {
        emit(
            "peer-ended-early",
            &[
                ("expected", Value::Str("the peer runs its whole timeline")),
                (
                    "got",
                    Value::Str(
                        "the peer stopped early at a failed step; see its log (peer.log) for which",
                    ),
                ),
            ],
        );
        return Err("peer-ended-early");
    }
    if !link_up {
        emit(
            "link-stuck-down",
            &[
                (
                    "expected",
                    Value::Str("link-up true at the end (robustness contract: link up again within 5 s of returning)"),
                ),
                ("got", Value::Str("false")),
            ],
        );
        return Err("link-stuck-down");
    }
    if link_drops < opts.expect_link_flaps {
        emit(
            "link-flaps-unseen",
            &[
                (
                    "expected",
                    Value::Str(&format!(
                        "link-up false at least {} times (the test dropped the link that often; robustness contract: within 1 s)",
                        opts.expect_link_flaps
                    )),
                ),
                ("got", Value::Str(&format!("{link_drops} times"))),
            ],
        );
        return Err("link-flaps-unseen");
    }
    Ok(())
}

/// The first 64 bytes of a frame in hex, for logs.
fn hex_head(frame: &[u8]) -> String {
    let n = frame.len().min(64);
    let mut s: String = frame[..n].iter().map(|b| format!("{b:02x}")).collect();
    if frame.len() > n {
        s.push_str("...");
    }
    s
}
