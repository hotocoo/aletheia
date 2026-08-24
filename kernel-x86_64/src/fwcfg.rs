//! The fw_cfg ioport transport (ISA default since forever: selector word at 0x510, data byte
//! at 0x511). Port I/O needs no page mapping, and OVMF does not hide the ports — the same
//! delivery channel works under UEFI boot as under direct kernel load.
use kernel_core::fwcfg::FwCfgBus;
use x86_64::instructions::port::Port;

/// The ISA-assigned fw_cfg ports.
const SELECTOR_PORT: u16 = 0x510;
const DATA_PORT: u16 = 0x511;

/// The platform bus handle. One instance; nothing here is shared across cores.
pub struct FwCfgIoports {
    selector: Port<u16>,
    data: Port<u8>,
}

impl FwCfgIoports {
    pub const fn new() -> Self {
        FwCfgIoports {
            selector: Port::new(SELECTOR_PORT),
            data: Port::new(DATA_PORT),
        }
    }
}

impl FwCfgBus for FwCfgIoports {
    fn select(&mut self, selector: u16) {
        // SAFETY: 0x510/0x511 are the firmware configuration ports; an absent device reads all
        // ones, which the protocol layer above names as "no platform channel".
        unsafe { self.selector.write(selector) };
    }
    fn read_byte(&mut self) -> u8 {
        // SAFETY: see above — dead ports read as 0xFF, which is exactly the dead-bus rule.
        unsafe { self.data.read() }
    }
}
