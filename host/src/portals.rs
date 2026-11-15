// SPDX-License-Identifier: AGPL-3.0-or-later

//! The portals, as the host implements them for a driver: the per-run
//! [`State`] and its `mmio`, `dma`, `clock`, `log` and `nvme` host traits.

use super::*;

/// Per-run host state: the device and the driver's resources.
///
/// Teardown order matters: bus mastering must be off before DMA buffers
/// (in `table`) are unmapped. `main` calls `Device::quiesce` explicitly
/// before dropping the store; `Device`'s own drop is only a backstop.
pub(crate) struct State {
    pub(crate) device: Device,
    pub(crate) table: ResourceTable,
    pub(crate) limits: StoreLimits,
    pub(crate) trace: bool,
    pub(crate) started: Instant,
    /// Injected device faults (docs/design/robust-nic.md).
    pub(crate) faults: faults::Faults,
    /// When the current call into the driver runs out of time (set with
    /// its epoch deadline). A sleep stops here, so it cannot carry the
    /// call past its limit (issue #10).
    pub(crate) deadline: Option<Instant>,
}

impl mmio::Host for State {
    fn size(&mut self) -> wasmtime::Result<u32> {
        Ok(self.device.window().size())
    }

    fn read8(&mut self, offset: u32) -> wasmtime::Result<Result<u8, mmio::Error>> {
        let mut r = self.device.window().read8(offset);
        if r.is_ok() {
            if let Some(v) = self.faults.register_read("read8", offset, 1) {
                r = Ok(v as u8);
            } else if let Ok(v) = r {
                let v = self.faults.cap_read("read8", offset, 1, v.into());
                r = Ok(v as u8);
            }
        }
        self.trace_read("read8", offset, r.as_ref().ok().map(|v| u64::from(*v)));
        mmio_result("read8", offset, None, r)
    }

    fn read16(&mut self, offset: u32) -> wasmtime::Result<Result<u16, mmio::Error>> {
        let mut r = self.device.window().read16(offset);
        if r.is_ok() {
            if let Some(v) = self.faults.register_read("read16", offset, 2) {
                r = Ok(v as u16);
            } else if let Ok(v) = r {
                let v = self.faults.cap_read("read16", offset, 2, v.into());
                r = Ok(v as u16);
            }
        }
        self.trace_read("read16", offset, r.as_ref().ok().map(|v| u64::from(*v)));
        mmio_result("read16", offset, None, r)
    }

    fn read32(&mut self, offset: u32) -> wasmtime::Result<Result<u32, mmio::Error>> {
        let mut r = self.device.window().read32(offset);
        if r.is_ok() {
            if let Some(v) = self.faults.register_read("read32", offset, 4) {
                r = Ok(v);
            } else if let Ok(v) = r {
                let v = self.faults.cap_read("read32", offset, 4, v);
                r = Ok(v);
            }
        }
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
        if self.faults.allocation("allocate", size) {
            return Ok(Err(dma_error("allocate", DmaError::OutOfMemory)?));
        }
        let r = self.device.dma().allocate(size);
        self.dma_buffer("allocate", r)
    }

    fn allocate32(
        &mut self,
        size: u32,
    ) -> wasmtime::Result<Result<Resource<DmaBuffer>, dma::Error>> {
        if self.faults.allocation("allocate32", size) {
            return Ok(Err(dma_error("allocate32", DmaError::OutOfMemory)?));
        }
        let r = self.device.dma().allocate32(size);
        self.dma_buffer("allocate32", r)
    }
}

impl State {
    /// Hand a new buffer to the driver, or map the shim's error.
    fn dma_buffer(
        &mut self,
        op: &str,
        r: Result<DmaBuffer, DmaError>,
    ) -> wasmtime::Result<Result<Resource<DmaBuffer>, dma::Error>> {
        match r {
            Ok(buf) => {
                if self.trace {
                    emit(
                        "dma-allocate",
                        &[
                            ("op", Value::Str(op)),
                            ("size", Value::Hex(buf.size().into())),
                            ("iova", Value::Hex(buf.iova())),
                        ],
                    );
                }
                Ok(Ok(self.table.push(buf)?))
            }
            Err(e) => Ok(Err(dma_error(op, e)?)),
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
        let b = self.table.get(&buf)?;
        let iova = b.iova();
        match b.read(offset, len) {
            Ok(mut v) => {
                self.faults.device_data(iova, offset, &mut v);
                self.faults.identify_data(iova, offset, &mut v);
                Ok(Ok(v))
            }
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
        let mut sleep = Duration::from_micros(us.min(MAX_SLEEP_US).into());
        // The epoch deadline interrupts only Wasm code, not this host
        // thread: without the cut, a call could overrun its limit by up to
        // MAX_SLEEP_US. Cut short, the call is interrupted at its next
        // epoch check, within a tick or so of its deadline.
        if let Some(deadline) = self.deadline {
            sleep = sleep.min(deadline.saturating_duration_since(Instant::now()));
        }
        std::thread::sleep(sleep);
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
        // t_ms: the same clock as injected faults, so the two line up.
        let mut out = vec![
            (
                "t_ms",
                Value::U64(self.started.elapsed().as_millis() as u64),
            ),
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
        if self.faults.gone("admin") {
            return Ok(Err(nvme::Error::Failed));
        }
        self.faults.admin_command(&command);
        if let Some(c) = self.faults.create_queue(&command) {
            return Ok(Ok(c));
        }
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
        Ok(r.map(|c| {
            let mut c = c.to_vec();
            self.faults.completion("admin", &mut c);
            c
        })
        .map_err(nvme_error))
    }

    fn submit(&mut self, sqid: u16, command: Vec<u8>) -> wasmtime::Result<Result<(), nvme::Error>> {
        if self.faults.gone("submit") {
            return Ok(Err(nvme::Error::Failed));
        }
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
        if self.faults.gone("complete") {
            return Ok(Err(nvme::Error::Failed));
        }
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
        Ok(r.map(|c| {
            c.iter()
                .map(|e| {
                    let mut e = e.to_vec();
                    self.faults.completion("complete", &mut e);
                    e
                })
                .collect()
        })
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
