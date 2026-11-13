// SPDX-License-Identifier: AGPL-3.0-or-later

//! Imports a WASI function besides the portals. Expected: the host rejects
//! the module before it touches the device (rule 2).

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::mmio;

#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn proc_exit(code: i32);
}

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        // Conditional so the import is kept; the host must refuse the module
        // whether or not the call would run.
        if mmio::size() == 0 {
            // SAFETY: test fixture; this module must never be instantiated.
            unsafe { proc_exit(1) };
        }
        Ok(())
    }
}

export!(Module);
