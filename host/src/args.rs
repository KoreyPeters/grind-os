// SPDX-License-Identifier: AGPL-3.0-or-later

//! grind-host's command line: parse it into [`Args`].

use super::*;

pub(crate) struct Args {
    pub(crate) device: String,
    pub(crate) module: String,
    pub(crate) timeout: Duration,
    pub(crate) dma_limit: u64,
    pub(crate) trace: bool,
    pub(crate) allow_unreviewed: bool,
    pub(crate) serve_ping: Option<Ipv4Addr>,
    /// `--exercise-block`, with the geometry the test expects.
    pub(crate) exercise_block: Option<block::Expect>,
    pub(crate) serve: Duration,
    pub(crate) expect_link_flaps: u64,
    pub(crate) fault_seed: u64,
    pub(crate) fail_allocation: Option<u32>,
    pub(crate) fault_identify: bool,
    pub(crate) fault_mqes: Option<u32>,
}

pub(crate) fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut device = None;
    let mut module = None;
    let mut timeout = Duration::from_secs(10);
    let mut dma_limit = 64u64 << 20;
    let mut trace = false;
    let mut allow_unreviewed = false;
    let mut serve_ping = None;
    let mut expect_link_flaps = 0;
    let mut fault_seed = 1;
    let mut fail_allocation = None;
    let mut fault_identify = false;
    let mut fault_mqes = None;
    let mut exercise = false;
    let mut device_errors = false;
    let mut power_loss = false;
    let mut block_quick = false;
    let mut block_size = None;
    let mut blocks = None;
    let mut serve = Duration::from_secs(10);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--device" => device = Some(value("--device")?),
            "--module" => module = Some(value("--module")?),
            "--timeout-ms" => {
                let v = value("--timeout-ms")?;
                timeout = Duration::from_millis(
                    v.parse()
                        .map_err(|_| format!("--timeout-ms {v}: not a number"))?,
                );
            }
            "--dma-limit-mib" => {
                let v = value("--dma-limit-mib")?;
                dma_limit = v
                    .parse::<u64>()
                    .map_err(|_| format!("--dma-limit-mib {v}: not a number"))?
                    << 20;
            }
            "--trace" => trace = true,
            "--allow-unreviewed-deny-list" => allow_unreviewed = true,
            "--serve-ping" => {
                let v = value("--serve-ping")?;
                serve_ping = Some(
                    v.parse()
                        .map_err(|_| format!("--serve-ping {v}: not an IPv4 address"))?,
                );
            }
            "--exercise-block" => exercise = true,
            "--block-device-errors" => device_errors = true,
            "--block-power-loss" => power_loss = true,
            "--block-quick" => block_quick = true,
            "--expect-block-size" => {
                let v = value("--expect-block-size")?;
                block_size = Some(
                    v.parse::<u32>()
                        .map_err(|_| format!("--expect-block-size {v}: not a number"))?,
                );
            }
            "--expect-blocks" => {
                let v = value("--expect-blocks")?;
                blocks = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("--expect-blocks {v}: not a number"))?,
                );
            }
            "--fault-seed" => {
                let v = value("--fault-seed")?;
                fault_seed = v
                    .parse()
                    .map_err(|_| format!("--fault-seed {v}: not a number"))?;
            }
            "--fault-identify" => fault_identify = true,
            "--fault-mqes" => {
                let v = value("--fault-mqes")?;
                fault_mqes = Some(
                    v.parse::<u32>()
                        .ok()
                        .filter(|n| (2..=65536).contains(n))
                        .ok_or_else(|| format!("--fault-mqes {v}: not 2 to 65536 entries"))?,
                );
            }
            "--fail-allocation" => {
                let v = value("--fail-allocation")?;
                fail_allocation = Some(
                    v.parse()
                        .map_err(|_| format!("--fail-allocation {v}: not a number"))?,
                );
            }
            "--expect-link-flaps" => {
                let v = value("--expect-link-flaps")?;
                expect_link_flaps = v
                    .parse()
                    .map_err(|_| format!("--expect-link-flaps {v}: not a number"))?;
            }
            "--serve-ms" => {
                let v = value("--serve-ms")?;
                serve = Duration::from_millis(
                    v.parse()
                        .map_err(|_| format!("--serve-ms {v}: not a number"))?,
                );
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        device: device.ok_or("--device is required")?,
        module: module.ok_or("--module is required")?,
        timeout,
        dma_limit,
        trace,
        allow_unreviewed,
        serve_ping,
        serve,
        expect_link_flaps,
        fault_seed,
        fail_allocation,
        fault_identify,
        fault_mqes,
        exercise_block: match (exercise, block_size, blocks) {
            (false, None, None) => None,
            (true, Some(block_size), Some(blocks)) => Some(block::Expect {
                block_size,
                blocks,
                device_errors,
                power_loss,
                quick: block_quick,
            }),
            _ => {
                return Err(
                    "--exercise-block, --expect-block-size and --expect-blocks go together".into(),
                )
            }
        },
    })
}
