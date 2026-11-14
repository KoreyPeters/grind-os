// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control: a "NIC driver" that touches no hardware, claims the
//! link is up, never receives and silently drops what it is given to send.
//! The NIC test must fail on it (nic-peer gets no ARP reply), which shows
//! the test cannot pass without a working driver.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "nic" });

use exports::grind::portals::ethernet;

struct Silent;

impl Guest for Silent {
    fn start() -> Result<(), String> {
        Ok(())
    }
}

impl ethernet::Guest for Silent {
    fn mac_address() -> (u8, u8, u8, u8, u8, u8) {
        (0x52, 0x54, 0x00, 0x12, 0x34, 0x56)
    }
    fn link_up() -> bool {
        true
    }
    fn transmit(_frame: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
    fn receive() -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }
}

export!(Silent);
