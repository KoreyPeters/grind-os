// SPDX-License-Identifier: AGPL-3.0-or-later

//! grind-host: run one Wasm driver against one device, through portals only.
//!
//! ```text
//! grind-host --device <pci-address> --module <driver.wasm>
//!            [--timeout-ms N] [--dma-limit-mib N] [--trace]
//!            [--allow-unreviewed-deny-list]
//! ```
//!
//! The module must be a core Wasm module built for wasm32-unknown-unknown
//! with wit-bindgen against `portals/wit` (world `driver`). Its only allowed
//! imports are the portal interfaces; anything else (WASI included) is
//! rejected before the device is touched.
//!
//! Output is JSON lines on stdout. The last line is always
//! `{"event":"result","status":"pass"|"fail",...}`.
//!
//! Exit status: 0 pass; 1 the driver failed (error, trap, timeout or a
//! denied register write); 2 the run could not start (bad arguments,
//! rejected module, device could not be opened).

use grind_shim::log::{emit, Value};
use grind_shim::{Device, DmaBuffer, DmaError, MmioError};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Trap};

wasmtime::component::bindgen!({
    path: "../portals/wit",
    world: "driver",
    imports: { default: trappable },
    with: { "grind:portals/dma.buffer": grind_shim::DmaBuffer },
});

use grind::portals::{clock, dma, log, mmio};

/// Core-module import names a driver may use: the portal interfaces.
const PORTAL_IMPORTS: &[&str] = &[
    "grind:portals/mmio@0.1.0",
    "grind:portals/dma@0.1.0",
    "grind:portals/clock@0.1.0",
    "grind:portals/log@0.1.0",
];

/// Longest single sleep a driver can request.
const MAX_SLEEP_US: u32 = 1_000_000;
/// Linear memory limit for a driver.
const MAX_MEMORY: usize = 64 << 20;

struct Args {
    device: String,
    module: String,
    timeout: Duration,
    dma_limit: u64,
    trace: bool,
    allow_unreviewed: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut device = None;
    let mut module = None;
    let mut timeout = Duration::from_secs(10);
    let mut dma_limit = 64u64 << 20;
    let mut trace = false;
    let mut allow_unreviewed = false;
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
    })
}

/// Per-run host state: the device and the driver's resources.
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

    fn read32(&mut self, offset: u32) -> wasmtime::Result<Result<u32, mmio::Error>> {
        let r = self.device.window().read32(offset);
        if self.trace {
            if let Ok(v) = &r {
                emit(
                    "mmio",
                    &[
                        ("op", "read32".into()),
                        ("offset", Value::Hex(offset.into())),
                        ("value", Value::Hex((*v).into())),
                    ],
                );
            }
        }
        mmio_result("read32", offset, None, r)
    }

    fn write32(&mut self, offset: u32, value: u32) -> wasmtime::Result<Result<(), mmio::Error>> {
        let r = self.device.window().write32(offset, value);
        if self.trace && r.is_ok() {
            emit(
                "mmio",
                &[
                    ("op", "write32".into()),
                    ("offset", Value::Hex(offset.into())),
                    ("value", Value::Hex(value.into())),
                ],
            );
        }
        mmio_result("write32", offset, Some(value), r)
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

    // 3. Instantiate with portals only, under memory and time limits.
    let mut linker: Linker<State> = Linker::new(&engine);
    if let Err(e) = Driver::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s) {
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
    store.set_epoch_deadline(1);
    store.epoch_deadline_trap();
    let ticker = Arc::new(engine.clone());
    let timeout = args.timeout;
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        ticker.increment_epoch();
    });

    let driver = match Driver::instantiate(&mut store, &component, &linker) {
        Ok(d) => d,
        Err(e) => {
            return setup_error(
                "instantiate",
                "the driver instantiates against the portals",
                &format!("{e:#}"),
            )
        }
    };

    // 4. Run.
    emit(
        "start",
        &[
            ("module", Value::Str(&args.module)),
            ("timeout_ms", Value::U64(args.timeout.as_millis() as u64)),
        ],
    );
    let outcome = driver.call_start(&mut store);
    let elapsed_ms = store.data().started.elapsed().as_millis() as u64;
    let violations = store.data().device.window().violations();
    let dma_used = store.data().device.dma().used();
    // Release the driver's resources (DMA buffers are unmapped, then freed)
    // before reporting.
    drop(store);

    let extra = [
        ("violations", Value::U64(violations.into())),
        ("elapsed_ms", Value::U64(elapsed_ms)),
        ("dma_bytes_at_exit", Value::U64(dma_used)),
    ];
    match outcome {
        Err(e) => {
            let timed_out = matches!(e.downcast_ref::<Trap>(), Some(Trap::Interrupt));
            let reason = if timed_out { "timeout" } else { "trap" };
            emit(
                "trap",
                &[
                    ("reason", Value::Str(reason)),
                    (
                        "expected",
                        Value::Str(&format!(
                            "start returns within {} ms",
                            args.timeout.as_millis()
                        )),
                    ),
                    ("got", Value::Str(&format!("{e:?}"))),
                ],
            );
            finish("fail", reason, &extra, 1)
        }
        Ok(Err(msg)) => {
            emit(
                "driver-error",
                &[
                    ("expected", Value::Str("start returns ok")),
                    ("got", Value::Str(&msg)),
                ],
            );
            finish("fail", "driver-error", &extra, 1)
        }
        Ok(Ok(())) if violations > 0 => finish("fail", "deny-list-violation", &extra, 1),
        Ok(Ok(())) => finish("pass", "ok", &extra, 0),
    }
}
