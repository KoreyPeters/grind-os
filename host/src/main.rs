// SPDX-License-Identifier: AGPL-3.0-or-later

//! grind-host: run one Wasm driver against one device, through portals only.
//!
//! ```text
//! grind-host --device <pci-address> --module <driver.wasm>
//!            [--timeout-ms N] [--dma-limit-mib N] [--trace]
//!            [--allow-unreviewed-deny-list]
//!            [--serve-ping <ipv4> [--serve-ms N]]
//!            [--exercise-block --expect-block-size N --expect-blocks N]
//! ```
//!
//! With `--exercise-block`, the device must take NVMe commands and the
//! module must target world `block-driver`. After `start` succeeds, the
//! host reads, writes, flushes and reads back through the driver's `block`
//! exports and checks every answer (see `block.rs`).
//!
//! The world depends on the device and the options. For an NVMe controller
//! the module targets world `nvme-driver`, which adds the `nvme` portal;
//! the host links that portal for no other device. With `--serve-ping`, the
//! module must target world `nic`. After `start`
//! succeeds, the host polls the driver's `receive` for `--serve-ms` and
//! answers ARP and ICMP echo requests for the address through `transmit`
//! (see `net.rs`).
//!
//! The module must be a core Wasm module built for wasm32-unknown-unknown
//! with wit-bindgen against `portals/wit` (world `driver`). Its only allowed
//! imports are the portal interfaces; anything else (WASI included) is
//! rejected before the device is touched.
//!
//! Output is JSON lines on stdout. The last line is always
//! `{"event":"result","status":"pass"|"fail",...}`.
//!
//! Exit status: 0 pass; 1 the driver failed (error, trap, timeout, a
//! denied register write or a refused command); 2 the run could not start (bad arguments,
//! rejected module, device could not be opened).

use grind_shim::log::{emit, Value};
use grind_shim::{Device, DmaBuffer, DmaError, MmioError, NvmeError};
use std::process::ExitCode;
use std::time::{Duration, Instant};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Trap};

mod block;
mod net;

/// World `driver`: the portals plus `start`.
mod driver_world {
    wasmtime::component::bindgen!({
        path: "../portals/wit",
        world: "driver",
        imports: { default: trappable },
        with: { "grind:portals/dma.buffer": grind_shim::DmaBuffer },
    });
}

/// World `nic`: the same portals (shared with `driver_world`), plus the
/// `ethernet` exports.
mod nic_world {
    wasmtime::component::bindgen!({
        path: "../portals/wit",
        world: "nic",
        imports: { default: trappable },
        with: {
            "grind:portals/mmio": crate::driver_world::grind::portals::mmio,
            "grind:portals/dma": crate::driver_world::grind::portals::dma,
            "grind:portals/clock": crate::driver_world::grind::portals::clock,
            "grind:portals/log": crate::driver_world::grind::portals::log,
        },
    });
}

/// World `nvme-driver`: the portals plus `nvme`.
mod nvme_world {
    wasmtime::component::bindgen!({
        path: "../portals/wit",
        world: "nvme-driver",
        imports: { default: trappable },
        with: {
            "grind:portals/mmio": crate::driver_world::grind::portals::mmio,
            "grind:portals/dma": crate::driver_world::grind::portals::dma,
            "grind:portals/clock": crate::driver_world::grind::portals::clock,
            "grind:portals/log": crate::driver_world::grind::portals::log,
        },
    });
}

use driver_world::grind::portals::{clock, dma, log, mmio};
use driver_world::Driver;
use nic_world::Nic;
/// World `block-driver`: `nvme-driver` plus the `block` exports.
mod block_world {
    wasmtime::component::bindgen!({
        path: "../portals/wit",
        world: "block-driver",
        imports: { default: trappable },
        with: {
            "grind:portals/mmio": crate::driver_world::grind::portals::mmio,
            "grind:portals/dma": crate::driver_world::grind::portals::dma,
            "grind:portals/clock": crate::driver_world::grind::portals::clock,
            "grind:portals/log": crate::driver_world::grind::portals::log,
            "grind:portals/nvme": crate::nvme_world::grind::portals::nvme,
        },
    });
}

use block_world::BlockDriver;
use nvme_world::grind::portals::nvme;
use nvme_world::NvmeDriver;
use std::net::Ipv4Addr;

/// Core-module import names a driver may use: the portal interfaces.
const PORTAL_IMPORTS: &[&str] = &[
    "grind:portals/mmio@0.1.0",
    "grind:portals/dma@0.1.0",
    "grind:portals/clock@0.1.0",
    "grind:portals/log@0.1.0",
    NVME_IMPORT,
];
/// The command portal, linked only for devices that take commands.
const NVME_IMPORT: &str = "grind:portals/nvme@0.1.0";

/// Longest single sleep a driver can request.
const MAX_SLEEP_US: u32 = 1_000_000;
/// Linear memory limit for a driver.
const MAX_MEMORY: usize = 64 << 20;
/// The epoch ticks this often; call time limits are counted in ticks.
const TICK: Duration = Duration::from_millis(10);
/// Time limit for each `ethernet` call while serving.
const CALL_LIMIT: Duration = Duration::from_secs(1);
/// Pause between `receive` polls that found nothing.
const IDLE_POLL: Duration = Duration::from_millis(1);
/// Frame sizes the `ethernet` interface allows (no FCS).
const MIN_FRAME: usize = 14;
const MAX_FRAME: usize = 1514;

struct Args {
    device: String,
    module: String,
    timeout: Duration,
    dma_limit: u64,
    trace: bool,
    allow_unreviewed: bool,
    serve_ping: Option<Ipv4Addr>,
    /// `--exercise-block`, with the geometry the test expects.
    exercise_block: Option<block::Expect>,
    serve: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut device = None;
    let mut module = None;
    let mut timeout = Duration::from_secs(10);
    let mut dma_limit = 64u64 << 20;
    let mut trace = false;
    let mut allow_unreviewed = false;
    let mut serve_ping = None;
    let mut exercise = false;
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
        exercise_block: match (exercise, block_size, blocks) {
            (false, None, None) => None,
            (true, Some(block_size), Some(blocks)) => Some(block::Expect { block_size, blocks }),
            _ => {
                return Err(
                    "--exercise-block, --expect-block-size and --expect-blocks go together".into(),
                )
            }
        },
    })
}

/// Per-run host state: the device and the driver's resources.
///
/// Teardown order matters: bus mastering must be off before DMA buffers
/// (in `table`) are unmapped. `main` calls `Device::quiesce` explicitly
/// before dropping the store; `Device`'s own drop is only a backstop.
struct State {
    device: Device,
    table: ResourceTable,
    limits: StoreLimits,
    trace: bool,
    started: Instant,
}

impl mmio::Host for State {
    fn size(&mut self) -> wasmtime::Result<u32> {
        Ok(self.device.window().size())
    }

    fn read8(&mut self, offset: u32) -> wasmtime::Result<Result<u8, mmio::Error>> {
        let r = self.device.window().read8(offset);
        self.trace_read("read8", offset, r.as_ref().ok().map(|v| u64::from(*v)));
        mmio_result("read8", offset, None, r)
    }

    fn read16(&mut self, offset: u32) -> wasmtime::Result<Result<u16, mmio::Error>> {
        let r = self.device.window().read16(offset);
        self.trace_read("read16", offset, r.as_ref().ok().map(|v| u64::from(*v)));
        mmio_result("read16", offset, None, r)
    }

    fn read32(&mut self, offset: u32) -> wasmtime::Result<Result<u32, mmio::Error>> {
        let r = self.device.window().read32(offset);
        self.trace_read("read32", offset, r.as_ref().ok().map(|v| u64::from(*v)));
        mmio_result("read32", offset, None, r)
    }

    fn write8(&mut self, offset: u32, value: u8) -> wasmtime::Result<Result<(), mmio::Error>> {
        let r = self.device.window().write8(offset, value);
        self.trace_write("write8", offset, value.into(), r.is_ok());
        mmio_result("write8", offset, Some(value.into()), r)
    }

    fn write16(&mut self, offset: u32, value: u16) -> wasmtime::Result<Result<(), mmio::Error>> {
        let r = self.device.window().write16(offset, value);
        self.trace_write("write16", offset, value.into(), r.is_ok());
        mmio_result("write16", offset, Some(value.into()), r)
    }

    fn write32(&mut self, offset: u32, value: u32) -> wasmtime::Result<Result<(), mmio::Error>> {
        let r = self.device.window().write32(offset, value);
        self.trace_write("write32", offset, value, r.is_ok());
        mmio_result("write32", offset, Some(value), r)
    }
}

impl State {
    fn trace_read(&self, op: &str, offset: u32, value: Option<u64>) {
        if let (true, Some(v)) = (self.trace, value) {
            emit(
                "mmio",
                &[
                    ("op", op.into()),
                    ("offset", Value::Hex(offset.into())),
                    ("value", Value::Hex(v)),
                ],
            );
        }
    }

    fn trace_write(&self, op: &str, offset: u32, value: u32, ok: bool) {
        if self.trace && ok {
            emit(
                "mmio",
                &[
                    ("op", op.into()),
                    ("offset", Value::Hex(offset.into())),
                    ("value", Value::Hex(value.into())),
                ],
            );
        }
    }
}

/// Map a shim result to the portal's: driver mistakes become errors the
/// driver sees (and are logged); host-side failures trap the driver.
fn mmio_result<T>(
    op: &str,
    offset: u32,
    value: Option<u32>,
    r: Result<T, MmioError>,
) -> wasmtime::Result<Result<T, mmio::Error>> {
    let err = match r {
        Ok(v) => return Ok(Ok(v)),
        Err(MmioError::Io(e)) => {
            return Err(wasmtime::Error::msg(format!(
                "{op} at 0x{offset:x}: register access failed: {e}"
            )))
        }
        // Already logged and counted by the shim.
        Err(MmioError::Denied(_)) => return Ok(Err(mmio::Error::Denied)),
        Err(MmioError::OutOfRange) => mmio::Error::OutOfRange,
        Err(MmioError::Misaligned) => mmio::Error::Misaligned,
    };
    let mut fields = vec![
        ("attempted", Value::Str(op)),
        ("offset", Value::Hex(offset.into())),
        (
            "error",
            Value::Str(match err {
                mmio::Error::OutOfRange => "out-of-range",
                mmio::Error::Misaligned => "misaligned",
                mmio::Error::Denied => "denied",
            }),
        ),
    ];
    if let Some(v) = value {
        fields.push(("value", Value::Hex(v.into())));
    }
    emit("mmio-error", &fields);
    Ok(Err(err))
}

fn dma_error(op: &str, e: DmaError) -> wasmtime::Result<dma::Error> {
    let err = match e {
        DmaError::Io(io) => {
            return Err(wasmtime::Error::msg(format!(
                "dma {op} failed in the host: {io}"
            )))
        }
        DmaError::BadSize => dma::Error::BadSize,
        DmaError::OutOfMemory => dma::Error::OutOfMemory,
        DmaError::OutOfRange => dma::Error::OutOfRange,
    };
    let name = match err {
        dma::Error::BadSize => "bad-size",
        dma::Error::OutOfMemory => "out-of-memory",
        dma::Error::OutOfRange => "out-of-range",
    };
    emit(
        "dma-error",
        &[("attempted", Value::Str(op)), ("error", Value::Str(name))],
    );
    Ok(err)
}

impl dma::Host for State {
    fn allocate(&mut self, size: u32) -> wasmtime::Result<Result<Resource<DmaBuffer>, dma::Error>> {
        match self.device.dma().allocate(size) {
            Ok(buf) => {
                if self.trace {
                    emit(
                        "dma-allocate",
                        &[
                            ("size", Value::Hex(buf.size().into())),
                            ("iova", Value::Hex(buf.iova())),
                        ],
                    );
                }
                Ok(Ok(self.table.push(buf)?))
            }
            Err(e) => Ok(Err(dma_error("allocate", e)?)),
        }
    }
}

impl dma::HostBuffer for State {
    fn iova(&mut self, buf: Resource<DmaBuffer>) -> wasmtime::Result<u64> {
        Ok(self.table.get(&buf)?.iova())
    }

    fn size(&mut self, buf: Resource<DmaBuffer>) -> wasmtime::Result<u32> {
        Ok(self.table.get(&buf)?.size())
    }

    fn read(
        &mut self,
        buf: Resource<DmaBuffer>,
        offset: u32,
        len: u32,
    ) -> wasmtime::Result<Result<Vec<u8>, dma::Error>> {
        match self.table.get(&buf)?.read(offset, len) {
            Ok(v) => Ok(Ok(v)),
            Err(e) => Ok(Err(dma_error("read", e)?)),
        }
    }

    fn write(
        &mut self,
        buf: Resource<DmaBuffer>,
        offset: u32,
        data: Vec<u8>,
    ) -> wasmtime::Result<Result<(), dma::Error>> {
        match self.table.get(&buf)?.write(offset, &data) {
            Ok(()) => Ok(Ok(())),
            Err(e) => Ok(Err(dma_error("write", e)?)),
        }
    }

    fn drop(&mut self, buf: Resource<DmaBuffer>) -> wasmtime::Result<()> {
        // Dropping the buffer unmaps it, then frees it.
        self.table.delete(buf)?;
        Ok(())
    }
}

impl clock::Host for State {
    fn now_ns(&mut self) -> wasmtime::Result<u64> {
        Ok(self.started.elapsed().as_nanos() as u64)
    }

    fn sleep_us(&mut self, us: u32) -> wasmtime::Result<()> {
        std::thread::sleep(Duration::from_micros(us.min(MAX_SLEEP_US).into()));
        Ok(())
    }
}

impl log::Host for State {
    fn emit(
        &mut self,
        level: log::Level,
        message: String,
        fields: Vec<(String, String)>,
    ) -> wasmtime::Result<()> {
        let level = match level {
            log::Level::Debug => "debug",
            log::Level::Info => "info",
            log::Level::Warn => "warn",
            log::Level::Error => "error",
        };
        // Driver keys are prefixed so they cannot collide with ours.
        let keys: Vec<String> = fields.iter().map(|(k, _)| format!("field.{k}")).collect();
        let mut out = vec![
            ("level", Value::Str(level)),
            ("message", Value::Str(&message)),
        ];
        for (key, (_, v)) in keys.iter().zip(&fields) {
            out.push((key.as_str(), Value::Str(v)));
        }
        emit("driver-log", &out);
        Ok(())
    }
}

impl nvme::Host for State {
    fn admin(&mut self, command: Vec<u8>) -> wasmtime::Result<Result<Vec<u8>, nvme::Error>> {
        let r = self.device.nvme_admin(&command).ok_or_else(|| {
            wasmtime::Error::msg("nvme portal used on a device that takes no commands")
        })?;
        if self.trace {
            emit(
                "nvme",
                &[
                    ("op", "admin".into()),
                    (
                        "opcode",
                        Value::Hex(command.first().copied().unwrap_or(0).into()),
                    ),
                    ("ok", Value::Bool(r.is_ok())),
                ],
            );
        }
        Ok(r.map(|c| c.to_vec()).map_err(nvme_error))
    }

    fn submit(&mut self, sqid: u16, command: Vec<u8>) -> wasmtime::Result<Result<(), nvme::Error>> {
        let r = self.device.nvme_submit(sqid, &command).ok_or_else(|| {
            wasmtime::Error::msg("nvme portal used on a device that takes no commands")
        })?;
        if self.trace {
            emit(
                "nvme",
                &[
                    ("op", "submit".into()),
                    ("sqid", Value::U64(sqid.into())),
                    (
                        "opcode",
                        Value::Hex(command.first().copied().unwrap_or(0).into()),
                    ),
                    ("ok", Value::Bool(r.is_ok())),
                ],
            );
        }
        Ok(r.map_err(nvme_error))
    }

    fn complete(&mut self, cqid: u16) -> wasmtime::Result<Result<Vec<Vec<u8>>, nvme::Error>> {
        let r = self.device.nvme_complete(cqid).ok_or_else(|| {
            wasmtime::Error::msg("nvme portal used on a device that takes no commands")
        })?;
        if self.trace {
            if let Ok(c) = &r {
                if !c.is_empty() {
                    emit(
                        "nvme",
                        &[
                            ("op", "complete".into()),
                            ("cqid", Value::U64(cqid.into())),
                            ("completions", Value::U64(c.len() as u64)),
                        ],
                    );
                }
            }
        }
        Ok(r.map(|c| c.iter().map(|e| e.to_vec()).collect())
            .map_err(nvme_error))
    }
}

/// The shim's errors as the portal's. Every one is already logged by the shim.
fn nvme_error(e: NvmeError) -> nvme::Error {
    match e {
        NvmeError::Refused(_) => nvme::Error::Refused,
        NvmeError::UnknownQueue => nvme::Error::UnknownQueue,
        NvmeError::QueueFull => nvme::Error::QueueFull,
        NvmeError::Timeout => nvme::Error::Timeout,
        NvmeError::Failed => nvme::Error::Failed,
    }
}

/// Whether a module imports the `nvme` portal.
fn imports_nvme(bytes: &[u8]) -> bool {
    use wasmparser::{Parser, Payload};
    Parser::new(0).parse_all(bytes).any(|p| match p {
        Ok(Payload::ImportSection(reader)) => reader
            .into_imports()
            .any(|i| i.is_ok_and(|i| i.module == NVME_IMPORT)),
        _ => false,
    })
}

/// Check that a module is a core module whose imports are all portals.
fn check_module(bytes: &[u8]) -> Result<(), String> {
    use wasmparser::{Encoding, Parser, Payload};
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|e| format!("not a valid Wasm module: {e}"))? {
            Payload::Version {
                encoding: Encoding::Component,
                ..
            } => {
                return Err(
                    "expected a core Wasm module (wasm32-unknown-unknown), got a component".into(),
                );
            }
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import = import.map_err(|e| format!("bad import section: {e}"))?;
                    if !PORTAL_IMPORTS.contains(&import.module) {
                        return Err(format!(
                            "import {}::{} is not a portal (allowed: {})",
                            import.module,
                            import.name,
                            PORTAL_IMPORTS.join(", ")
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Emit the final result line and return the exit code.
fn finish(status: &str, reason: &str, extra: &[(&str, Value)], code: u8) -> ExitCode {
    let mut fields = vec![
        ("status", Value::Str(status)),
        ("reason", Value::Str(reason)),
    ];
    fields.extend_from_slice(extra);
    emit("result", &fields);
    ExitCode::from(code)
}

fn setup_error(step: &str, expected: &str, got: &str) -> ExitCode {
    emit(
        "setup-error",
        &[
            ("step", Value::Str(step)),
            ("expected", Value::Str(expected)),
            ("got", Value::Str(got)),
        ],
    );
    finish("fail", "setup", &[("step", Value::Str(step))], 2)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            return setup_error(
                "args",
                "--device <pci-address> --module <file.wasm> [options]",
                &e,
            )
        }
    };

    // 1. The module: load, check imports, wrap as a component. All before
    //    the device is touched.
    let bytes = match std::fs::read(&args.module) {
        Ok(b) => b,
        Err(e) => {
            return setup_error(
                "module-read",
                &format!("{} is readable", args.module),
                &e.to_string(),
            )
        }
    };
    if let Err(e) = check_module(&bytes) {
        return setup_error("module-imports", "a core module importing only portals", &e);
    }
    let component_bytes = match wit_component::ComponentEncoder::default()
        .module(&bytes)
        .and_then(|enc| enc.validate(true).encode())
    {
        Ok(c) => c,
        Err(e) => {
            return setup_error(
                "module-component",
                "a module built with wit-bindgen for world grind:portals/driver",
                &format!("{e:#}"),
            )
        }
    };

    let mut config = Config::new();
    config.epoch_interruption(true);
    let engine = match Engine::new(&config) {
        Ok(e) => e,
        Err(e) => return setup_error("engine", "Wasmtime engine starts", &format!("{e:#}")),
    };
    let component = match Component::new(&engine, &component_bytes) {
        Ok(c) => c,
        Err(e) => {
            return setup_error(
                "module-compile",
                "the component compiles",
                &format!("{e:#}"),
            )
        }
    };

    // 2. The device.
    let device = match Device::open(&args.device, args.allow_unreviewed, args.dma_limit) {
        Ok(d) => d,
        Err(e) => return setup_error(&format!("device-{}", e.step), &e.expected, &e.got),
    };

    // 3. Which world: the nvme portal exists only for a device that takes
    //    commands. Setup errors from here on close the device first, so the
    //    result line stays last.
    let commands = device.takes_commands();
    let world = match (
        commands,
        args.serve_ping.is_some(),
        args.exercise_block.is_some(),
    ) {
        (false, false, false) => World::Driver,
        (false, true, false) => World::Nic,
        (true, false, false) => World::NvmeDriver,
        (true, false, true) => World::BlockDriver,
        _ => {
            drop(device);
            return setup_error(
                "world",
                "--serve-ping only with a network device; --exercise-block only with a device that takes NVMe commands",
                &format!(
                    "{} {}",
                    args.device,
                    if commands { "takes NVMe commands" } else { "takes no commands" }
                ),
            );
        }
    };
    if imports_nvme(&bytes) && !matches!(world, World::NvmeDriver | World::BlockDriver) {
        drop(device);
        return setup_error(
            "module-imports",
            &format!("{NVME_IMPORT} only for a device that takes NVMe commands"),
            &format!(
                "{} imports it; {} takes no commands",
                args.module, args.device
            ),
        );
    }

    // 4. Instantiate with portals only, under memory and time limits.
    let mut linker: Linker<State> = Linker::new(&engine);
    let linked = match world {
        World::NvmeDriver => NvmeDriver::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s),
        World::BlockDriver => {
            BlockDriver::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)
        }
        _ => Driver::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s),
    };
    if let Err(e) = linked {
        drop(device);
        return setup_error("linker", "portals added to the linker", &format!("{e:#}"));
    }
    let state = State {
        device,
        table: ResourceTable::new(),
        limits: StoreLimitsBuilder::new().memory_size(MAX_MEMORY).build(),
        trace: args.trace,
        started: Instant::now(),
    };
    let mut store = Store::new(&engine, state);
    store.limiter(|s| &mut s.limits);
    store.epoch_deadline_trap();
    // Each call into the driver gets a deadline counted in ticks.
    let ticker = engine.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        ticker.increment_epoch();
    });

    // Instantiation runs the module's start section, so it needs a deadline
    // too.
    set_deadline(&mut store, args.timeout);
    let instance = match world {
        World::Driver => Driver::instantiate(&mut store, &component, &linker).map(Instance::Driver),
        World::Nic => Nic::instantiate(&mut store, &component, &linker).map(Instance::Nic),
        World::NvmeDriver => {
            NvmeDriver::instantiate(&mut store, &component, &linker).map(Instance::NvmeDriver)
        }
        World::BlockDriver => {
            BlockDriver::instantiate(&mut store, &component, &linker).map(Instance::Block)
        }
    };
    let instance = match instance {
        Ok(i) => i,
        Err(e) => {
            drop(store);
            return setup_error(
                "instantiate",
                &format!(
                    "the driver instantiates as world {} against the portals",
                    world.name()
                ),
                &format!("{e:#}"),
            );
        }
    };

    // 5. Run: start, then (for a NIC) serve.
    emit(
        "start",
        &[
            ("module", Value::Str(&args.module)),
            ("timeout_ms", Value::U64(args.timeout.as_millis() as u64)),
        ],
    );
    set_deadline(&mut store, args.timeout);
    let started = match &instance {
        Instance::Driver(d) => d.call_start(&mut store),
        Instance::Nic(n) => n.call_start(&mut store),
        Instance::NvmeDriver(n) => n.call_start(&mut store),
        Instance::Block(b) => b.call_start(&mut store),
    };
    let failure: Option<&'static str> = match started {
        Err(e) => Some(call_failed("start", args.timeout, e)),
        Ok(Err(msg)) => {
            emit(
                "driver-error",
                &[
                    ("call", Value::Str("start")),
                    ("expected", Value::Str("start returns ok")),
                    ("got", Value::Str(&msg)),
                ],
            );
            Some("driver-error")
        }
        Ok(Ok(())) => match (&instance, args.serve_ping) {
            (Instance::Nic(nic), Some(ip)) => serve(&mut store, nic, ip, args.serve).err(),
            (Instance::Block(b), _) => {
                let expect = args.exercise_block.as_ref().expect("world BlockDriver");
                block::exercise(&mut store, b.grind_portals_block(), expect).err()
            }
            _ => None,
        },
    };

    let elapsed_ms = store.data().started.elapsed().as_millis() as u64;
    let violations = store.data().device.window().violations();
    let refused = store.data().device.refused_commands();
    let dma_used = store.data().device.dma().used();
    // Stop DMA before the driver's buffers are unmapped (logged either way).
    let quiesced = store.data().device.quiesce();
    // Release the driver's resources (DMA buffers are unmapped, then freed)
    // before reporting.
    drop(instance);
    drop(store);

    let extra = [
        ("violations", Value::U64(violations.into())),
        ("refused_commands", Value::U64(refused.into())),
        ("elapsed_ms", Value::U64(elapsed_ms)),
        ("dma_bytes_at_exit", Value::U64(dma_used)),
    ];
    match failure {
        Some(reason) => finish("fail", reason, &extra, 1),
        None if quiesced.is_err() => finish("fail", "quiesce", &extra, 1),
        None if violations > 0 => finish("fail", "deny-list-violation", &extra, 1),
        None if refused > 0 => finish("fail", "command-refused", &extra, 1),
        None => finish("pass", "ok", &extra, 0),
    }
}

enum Instance {
    Driver(Driver),
    Nic(Nic),
    NvmeDriver(NvmeDriver),
    Block(BlockDriver),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum World {
    Driver,
    Nic,
    NvmeDriver,
    BlockDriver,
}

impl World {
    fn name(self) -> &'static str {
        match self {
            World::Driver => "driver",
            World::Nic => "nic",
            World::NvmeDriver => "nvme-driver",
            World::BlockDriver => "block-driver",
        }
    }
}

/// Give the next call into the driver `limit` before it is interrupted.
fn set_deadline(store: &mut Store<State>, limit: Duration) {
    let ticks = limit.as_millis().div_ceil(TICK.as_millis()) as u64;
    // One extra tick: the current tick may be about to end.
    store.set_epoch_deadline(ticks + 1);
}

/// Log a call into the driver that trapped or ran out of time; return the
/// result reason.
fn call_failed(call: &str, limit: Duration, e: wasmtime::Error) -> &'static str {
    let timed_out = matches!(e.downcast_ref::<Trap>(), Some(Trap::Interrupt));
    let reason = if timed_out { "timeout" } else { "trap" };
    emit(
        "trap",
        &[
            ("reason", Value::Str(reason)),
            ("call", Value::Str(call)),
            (
                "expected",
                Value::Str(&format!("{call} returns within {} ms", limit.as_millis())),
            ),
            ("got", Value::Str(&format!("{e:?}"))),
        ],
    );
    reason
}

/// A driver-reported error from an `ethernet` call.
fn driver_error(call: &str, msg: &str) -> &'static str {
    emit(
        "driver-error",
        &[
            ("call", Value::Str(call)),
            ("expected", Value::Str(&format!("{call} returns ok"))),
            ("got", Value::Str(msg)),
        ],
    );
    "driver-error"
}

/// After `start`: poll the driver for frames and answer ARP and ping for
/// `ip` until `duration` has passed. Returns the failure reason, if any.
fn serve(
    store: &mut Store<State>,
    nic: &Nic,
    ip: Ipv4Addr,
    duration: Duration,
) -> Result<(), &'static str> {
    let eth = nic.grind_portals_ethernet();

    set_deadline(store, CALL_LIMIT);
    let m = eth
        .call_mac_address(&mut *store)
        .map_err(|e| call_failed("mac-address", CALL_LIMIT, e))?;
    let mac: net::Mac = [m.0, m.1, m.2, m.3, m.4, m.5];
    set_deadline(store, CALL_LIMIT);
    let link_up = eth
        .call_link_up(&mut *store)
        .map_err(|e| call_failed("link-up", CALL_LIMIT, e))?;
    emit(
        "serve-start",
        &[
            ("ipv4", Value::Str(&ip.to_string())),
            ("mac", Value::Str(&net::fmt_mac(&mac))),
            (
                "link_up",
                Value::Str(if link_up { "true" } else { "false" }),
            ),
            ("serve_ms", Value::U64(duration.as_millis() as u64)),
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
    let (mut received, mut answered) = (0u64, 0u64);
    let end = Instant::now() + duration;
    while Instant::now() < end {
        set_deadline(store, CALL_LIMIT);
        let frame = match eth.call_receive(&mut *store) {
            Err(e) => return Err(call_failed("receive", CALL_LIMIT, e)),
            Ok(Err(msg)) => return Err(driver_error("receive", &msg)),
            Ok(Ok(None)) => {
                std::thread::sleep(IDLE_POLL);
                continue;
            }
            Ok(Ok(Some(frame))) => frame,
        };
        received += 1;
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
            return Err("driver-error");
        }
        match responder.handle(&frame) {
            net::Outcome::Reply {
                frame: reply,
                summary,
            } => {
                set_deadline(store, CALL_LIMIT);
                match eth.call_transmit(&mut *store, &reply) {
                    Err(e) => return Err(call_failed("transmit", CALL_LIMIT, e)),
                    Ok(Err(msg)) => return Err(driver_error("transmit", &msg)),
                    Ok(Ok(())) => {}
                }
                answered += 1;
                emit(
                    "answered",
                    &[
                        ("summary", Value::Str(&summary)),
                        ("len", Value::U64(reply.len() as u64)),
                    ],
                );
            }
            net::Outcome::Ignored { summary } => {
                emit("frame-ignored", &[("summary", Value::Str(&summary))]);
            }
            net::Outcome::Malformed { reason } => {
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
    emit(
        "serve-end",
        &[
            ("frames_received", Value::U64(received)),
            ("frames_answered", Value::U64(answered)),
        ],
    );
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
