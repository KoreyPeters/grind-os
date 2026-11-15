// SPDX-License-Identifier: AGPL-3.0-or-later

//! grind-host: run one Wasm driver against one device, through portals only.
//!
//! ```text
//! grind-host --device <pci-address> --module <driver.wasm>
//!            [--timeout-ms N] [--dma-limit-mib N] [--trace]
//!            [--allow-unreviewed-deny-list]
//!            [--serve-ping <ipv4> [--serve-ms N] [--expect-link-flaps N]]
//!            [--fault-seed N] [--fail-allocation N] [--fault-identify] [--fault-mqes N] [--block-quick]
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

mod args;
mod block;
mod faults;
mod net;
mod portals;
mod serve;

use args::{parse_args, Args};
use portals::State;
use serve::{serve, ServeOptions};

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
    // 1. The module, all before the device is touched.
    let (bytes, engine, component) = match load_module(&args) {
        Ok(m) => m,
        Err(code) => return code,
    };
    // 2. The device.
    let device = match Device::open(&args.device, args.allow_unreviewed, args.dma_limit) {
        Ok(d) => d,
        Err(e) => return setup_error(&format!("device-{}", e.step), &e.expected, &e.got),
    };
    // 3. Which world. Setup errors from here on close the device first, so
    //    the result line stays last.
    let world = match choose_world(&args, &device, &bytes) {
        Ok(w) => w,
        Err(code) => {
            drop(device);
            return code();
        }
    };
    // 4. Instantiate with portals only, under memory and time limits.
    let (mut store, instance) = match instantiate(&args, &engine, &component, device, world) {
        Ok(r) => r,
        Err(code) => return code,
    };
    // 5. Run: start, then serve (a NIC) or exercise (a block driver).
    let failure = run(&args, &mut store, &instance);
    // 6. Stop the device, release the driver's resources, report.
    report(store, failure)
}

/// Step 1: load the module, check its imports, wrap it as a component and
/// compile it.
fn load_module(args: &Args) -> Result<(Vec<u8>, Engine, Component), ExitCode> {
    let bytes = match std::fs::read(&args.module) {
        Ok(b) => b,
        Err(e) => {
            return Err(setup_error(
                "module-read",
                &format!("{} is readable", args.module),
                &e.to_string(),
            ))
        }
    };
    if let Err(e) = check_module(&bytes) {
        return Err(setup_error(
            "module-imports",
            "a core module importing only portals",
            &e,
        ));
    }
    let component_bytes = match wit_component::ComponentEncoder::default()
        .module(&bytes)
        .and_then(|enc| enc.validate(true).encode())
    {
        Ok(c) => c,
        Err(e) => {
            return Err(setup_error(
                "module-component",
                "a module built with wit-bindgen for world grind:portals/driver",
                &format!("{e:#}"),
            ))
        }
    };

    let mut config = Config::new();
    config.epoch_interruption(true);
    let engine = match Engine::new(&config) {
        Ok(e) => e,
        Err(e) => {
            return Err(setup_error(
                "engine",
                "Wasmtime engine starts",
                &format!("{e:#}"),
            ))
        }
    };
    let component = match Component::new(&engine, &component_bytes) {
        Ok(c) => c,
        Err(e) => {
            return Err(setup_error(
                "module-compile",
                "the component compiles",
                &format!("{e:#}"),
            ))
        }
    };
    Ok((bytes, engine, component))
}

/// Step 3: the world the module must target. The `nvme` portal exists only
/// for a device that takes commands. On error, the setup error to report
/// once the caller has closed the device.
fn choose_world(
    args: &Args,
    device: &Device,
    bytes: &[u8],
) -> Result<World, Box<dyn FnOnce() -> ExitCode>> {
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
            let got = format!(
                "{} {}",
                args.device,
                if commands {
                    "takes NVMe commands"
                } else {
                    "takes no commands"
                }
            );
            return Err(Box::new(move || {
                setup_error(
                    "world",
                    "--serve-ping only with a network device; --exercise-block only with a device that takes NVMe commands",
                    &got,
                )
            }));
        }
    };
    if imports_nvme(bytes) && !matches!(world, World::NvmeDriver | World::BlockDriver) {
        let got = format!(
            "{} imports it; {} takes no commands",
            args.module, args.device
        );
        return Err(Box::new(move || {
            setup_error(
                "module-imports",
                &format!("{NVME_IMPORT} only for a device that takes NVMe commands"),
                &got,
            )
        }));
    }
    Ok(world)
}

/// Step 4: link the portals for the world, build the store (memory limit,
/// epoch deadlines, fault state) and instantiate the driver.
fn instantiate(
    args: &Args,
    engine: &Engine,
    component: &Component,
    device: Device,
    world: World,
) -> Result<(Store<State>, Instance), ExitCode> {
    let mut linker: Linker<State> = Linker::new(engine);
    let linked = match world {
        World::NvmeDriver => NvmeDriver::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s),
        World::BlockDriver => {
            BlockDriver::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)
        }
        _ => Driver::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s),
    };
    if let Err(e) = linked {
        drop(device);
        return Err(setup_error(
            "linker",
            "portals added to the linker",
            &format!("{e:#}"),
        ));
    }
    let started = Instant::now();
    let mut state = State {
        device,
        table: ResourceTable::new(),
        limits: StoreLimitsBuilder::new().memory_size(MAX_MEMORY).build(),
        trace: args.trace,
        started,
        faults: faults::Faults::new(
            args.fault_seed,
            args.fail_allocation,
            args.fault_identify,
            started,
        ),
        deadline: None,
    };
    if let Some(entries) = args.fault_mqes {
        state.faults.limit_queues(entries);
    }
    let mut store = Store::new(engine, state);
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
        World::Driver => Driver::instantiate(&mut store, component, &linker).map(Instance::Driver),
        World::Nic => Nic::instantiate(&mut store, component, &linker).map(Instance::Nic),
        World::NvmeDriver => {
            NvmeDriver::instantiate(&mut store, component, &linker).map(Instance::NvmeDriver)
        }
        World::BlockDriver => {
            BlockDriver::instantiate(&mut store, component, &linker).map(Instance::Block)
        }
    };
    let instance = match instance {
        Ok(i) => i,
        Err(e) => {
            drop(store);
            return Err(setup_error(
                "instantiate",
                &format!(
                    "the driver instantiates as world {} against the portals",
                    world.name()
                ),
                &format!("{e:#}"),
            ));
        }
    };
    Ok((store, instance))
}

/// Step 5: call `start`, then serve (`--serve-ping`) or exercise
/// (`--exercise-block`). The failure reason, if any.
fn run(args: &Args, store: &mut Store<State>, instance: &Instance) -> Option<&'static str> {
    emit(
        "start",
        &[
            ("module", Value::Str(&args.module)),
            ("timeout_ms", Value::U64(args.timeout.as_millis() as u64)),
        ],
    );
    set_deadline(store, args.timeout);
    let started = match instance {
        Instance::Driver(d) => d.call_start(&mut *store),
        Instance::Nic(n) => n.call_start(&mut *store),
        Instance::NvmeDriver(n) => n.call_start(&mut *store),
        Instance::Block(b) => b.call_start(&mut *store),
    };
    match started {
        Err(e) => Some(call_failed("start", args.timeout, e)),
        Ok(Err(msg)) if msg.trim().is_empty() => {
            emit(
                "driver-error-unreadable",
                &[
                    ("call", Value::Str("start")),
                    (
                        "expected",
                        Value::Str("an error saying what was attempted, expected and got (rule 3)"),
                    ),
                    ("got", Value::Str("an empty error")),
                ],
            );
            Some("driver-error-unreadable")
        }
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
        Ok(Ok(())) => match (instance, args.serve_ping) {
            (Instance::Nic(nic), Some(ip)) => serve(
                store,
                nic,
                &ServeOptions {
                    ip,
                    duration: args.serve,
                    expect_link_flaps: args.expect_link_flaps,
                },
            )
            .err(),
            (Instance::Block(b), _) => {
                let expect = args.exercise_block.as_ref().expect("world BlockDriver");
                block::exercise(store, b.grind_portals_block(), expect).err()
            }
            _ => None,
        },
    }
}

/// Step 6: stop DMA, release the driver's resources, then report the
/// result line and exit status.
fn report(store: Store<State>, failure: Option<&'static str>) -> ExitCode {
    let elapsed_ms = store.data().started.elapsed().as_millis() as u64;
    let violations = store.data().device.window().violations();
    let refused = store.data().device.refused_commands();
    let dma_used = store.data().device.dma().used();
    // Stop DMA before the driver's buffers are unmapped (logged either way).
    let quiesced = store.data().device.quiesce();
    // Release the driver's resources before reporting: they live in the
    // store, so dropping it unmaps the DMA buffers, then frees them. (The
    // instance holds only handles into the store; it needs no drop.)
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
    store.data_mut().deadline = Some(Instant::now() + limit);
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
