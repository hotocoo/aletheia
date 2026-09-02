//! The input HARDWARE rung: real devices through the input session (ALET-P2-021, ADR-080).
//!
//! ADR-079 built the input path as an authority question — exactly one session, focus as
//! routing, the owner alone reading — and named its own limit in the register: no REAL input
//! device was wired through it. The console's PS/2/serial bytes still fed the serial console,
//! the cursor moved only through the session API, and a desktop nobody could steer with
//! hardware is a mockup with good manners. This module closes that rung the way every driver
//! here does: a virtio device the machine actually has (one driver body over the transport
//! seam, both buses), a decoder whose output alphabet is a SECURITY BOUNDARY, and a routing
//! path the boot suites and the live desktop share.
//!
//! # virtio-input, because the question is arch-neutral
//!
//! A keyboard is a PS/2 controller on x86-64 and nothing at all on the ARM/RISC-V `virt`
//! machines; a USB stack does not exist here. virtio-input (VIRTIO 1.2 §5.8, device id 18)
//! exists on all three targets behind the transports this kernel already speaks, which puts
//! the SAME decode contract under the SAME gates on every CPU. Two queues: the **eventq** the
//! device fills with `virtio_input_event { type, code, value }` records as fast as the driver
//! re-posts buffers, and the **statusq** — created, armed, never fed: the device under QEMU
//! never sends on it, and buffers the driver does not understand how to harvest are buffers
//! it should not offer. Device identity, event-type bits and pointer-axis geometry come from
//! the config space, which virtio-input reads through a select/subsel register pair — the one
//! config WRITE this kernel's transports had never needed, and therefore the one the
//! [`ConfigWrite`] seam names.
//!
//! # The one rule, again, because it is still the whole security argument
//!
//! [`keymap::Keymap`] states the console's byte alphabet against [`shell::editor_accepts`],
//! and the PS/2 decoder proved "everything I emit, the editor understands" over its entire
//! input space (ADR-049/050). virtio-input hands the driver **Linux keycodes** — a different
//! wire alphabet entirely — so this module owns a second decoder with the same ONE rule: it
//! emits only through [`keymap::Keys`], only bytes the editor has a rule for, and the sweep
//! proves it over the whole keycode space in every reachable modifier state. A device an
//! attacker may be holding cannot manufacture a control byte the console has no answer for.
//! What the two decoders do NOT share is state: `Keymap`'s shift is the PS/2 shift key's
//! state, and a virtio keyboard's modifiers are held separately, because two devices sharing
//! one modifier bit would let either one hold the other's shift down.
//!
//! # The pointer is the cursor, and the click is a routing decision
//!
//! A pointing device (QEMU's virtio-tablet: absolute axes) feeds the compositor's cursor
//! plane through the session — the plane ADR-079 reserved for exactly this and no surface
//! could occupy. A button press is not a cursor move: it is a FOCUS decision, so the session
//! gained `focus_at` — the topmost placed surface under the point takes focus, a click on
//! empty space clears it, the loser is told through its own queue. The input path decides
//! where events go; the hardware finally speaks to it.
//!
//! # Proof posture
//!
//! Host-exhaustive in `tests/vinput.rs` (the alphabet sweep over every keycode in every
//! modifier state, the axis-mapping corners, the routing tables, the fail-closed refusals);
//! in-kernel, [`vinput_suite`] runs against the REAL devices on all three targets — identity
//! read back from the device's own config space and PINNED, DMA-gated queues, armed silence,
//! and the decode→route path driven end to end with synthetic records through the same
//! functions the live desktop pumps. The LIVE leg — injected events reaching the session of
//! an interactive machine — is `scripts/vinput-e2e.sh`.

use crate::compositor::{CompFault, Compositor};
use crate::keymap::{self, Keys};
use crate::virtioblk::{Transport, VirtioHal};
use crate::virtq::Virtqueue;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::marker::PhantomData;

/// virtio device id for an input device (VIRTIO 1.2 §5.8).
pub const VIRTIO_ID_INPUT: u32 = 18;

/// Queue 0: the device fills driver-posted buffers with input events.
const EVENT_QUEUE: u16 = 0;
/// Queue 1: the status queue — created, armed, never fed (see the module docs).
const STATUS_QUEUE: u16 = 1;

/// `struct virtio_input_event`: type (u16), code (u16), value (u32) — 8 bytes, one per buffer.
pub const EVENT_SIZE: usize = 8;

// -- config-space selects (VIRTIO 1.2 §5.8.4) ----------------------------------------------
/// Select 0: `size` 1, the data byte is the unplugged flag (0 = plugged in).
pub const CFG_UNPLUGGED: u8 = 0;
/// Select 1: the device name string, NUL-terminated, byte per subsel (subsel 0 starts it).
pub const CFG_NAME: u8 = 1;
/// Select 3: `struct virtio_input_devids` — bustype/vendor/product/version u16s.
pub const CFG_ID_DEVIDS: u8 = 3;
/// Select 0x11: per-event-type bitmap by subsel (EV_KEY = 1, EV_ABS = 3).
pub const CFG_EV_BITS: u8 = 0x11;
/// Select 0x12: `struct virtio_input_absinfo` for axis `subsel` — 20 bytes: min, max, fuzz,
/// flat, res.
pub const CFG_ABS_INFO: u8 = 0x12;

// -- event records (Linux input-event codes: the wire alphabet virtio-input speaks) --------
/// Event type 0: the batch separator — a decoder commits what accumulated under it.
pub const EV_SYN: u16 = 0;
/// Event type 1: a key or button; value 1 = press, 0 = release, 2 = autorepeat.
pub const EV_KEY: u16 = 1;
/// Event type 3: an absolute axis sample.
pub const EV_ABS: u16 = 3;
/// Axis 0 / axis 1: the pointing device's X / Y.
pub const ABS_X: u16 = 0;
pub const ABS_Y: u16 = 1;
/// The buttons this rung names. A left click routes (focus); the rest are decoded and counted.
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;

// Linux keycodes the decoder maps BY NAME (everything else goes through the tables).
const KEY_ESC: u16 = 1;
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_ENTER: u16 = 28;
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_CAPSLOCK: u16 = 58;
const KEY_KPENTER: u16 = 96;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_KPSLASH: u16 = 98;
const KEY_RIGHTALT: u16 = 100;
const KEY_HOME: u16 = 102;
const KEY_UP: u16 = 103;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_END: u16 = 107;
const KEY_DOWN: u16 = 108;
const KEY_DELETE: u16 = 111;

/// One event record harvested from a device: byte-for-byte what the wire carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawEvent {
    pub ty: u16,
    pub code: u16,
    pub value: u32,
}

/// What role a device plays. Slot order is an attachment artifact of the QEMU command line;
/// what a device IS is what it says it is — devices are classified by their OWN declared name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Keyboard,
    Tablet,
}

/// Classify a device by its config-space name. `None` = a virtio-input device this rung does
/// not model (a speaker, a switch bank) — named at the caller, never guessed at.
pub fn classify(name: &str) -> Option<Role> {
    if name.contains("Keyboard") {
        Some(Role::Keyboard)
    } else if name.contains("Tablet") {
        Some(Role::Tablet)
    } else {
        None
    }
}

/// The one config-space WRITE this rung needs: virtio-input reads its attributes through a
/// select/subsel pair, so a driver that cannot write two config bytes cannot even ask the
/// device its name. Named as its own trait rather than added to [`Transport`] so the existing
/// transports can adopt it without the other drivers caring.
pub trait ConfigWrite {
    /// Write an aligned 32-bit word of device-specific config space at `off`.
    ///
    /// # Safety
    /// The transport's config region must be mapped; only the select/subsel word (offset 0)
    /// has defined meaning in this kernel, and writing elsewhere is the caller's error.
    unsafe fn set_config_u32(&mut self, off: usize, value: u32);
}

/// Why input device bring-up refused. Same grammar as every driver here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputError {
    Unsupported(&'static str),
    Queue(&'static str),
    Refused(&'static str),
}

// -- feature/status bits, as everywhere else on the bus -------------------------------------
const S_ACKNOWLEDGE: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;
const S_FAILED: u32 = 0x80;
const F_VERSION_1_BIT: u32 = 0;
const F_IOMMU_PLATFORM_BIT: u32 = 1;

/// A live virtio-input device: the event queue with its posted buffers, the armed status
/// queue, and the counters the suites compare. One driver body, both buses, both roles —
/// a keyboard and a tablet are the SAME device kind with different config answers.
pub struct VirtioInput<H: VirtioHal, T: Transport + ConfigWrite> {
    transport: T,
    eventq: Virtqueue,
    statusq: Virtqueue,
    /// One frame carved into `qsize` 8-byte event slots; descriptor `slot` names slot `slot`.
    events_frame: usize,
    qsize: u16,
    version1: bool,
    status_at_ok: u32,
    /// Events harvested from the device — the live-leg counter.
    events_seen: Cell<u64>,
    local_refusals: Cell<u64>,
    _hal: PhantomData<H>,
}

#[inline]
fn le16(p: *const u8) -> u16 {
    // SAFETY: the caller proves the 8-byte record lies inside a registered, mapped frame.
    unsafe { (*p) as u16 | ((*p.add(1)) as u16) << 8 }
}

#[inline]
fn le32(p: *const u8) -> u32 {
    // SAFETY: as above.
    unsafe {
        (*p) as u32
            | ((*p.add(1)) as u32) << 8
            | ((*p.add(2)) as u32) << 16
            | ((*p.add(3)) as u32) << 24
    }
}

impl<H: VirtioHal, T: Transport + ConfigWrite> VirtioInput<H, T> {
    /// Bring the device up: the VIRTIO 1.1 §3.1.1 dance, VERSION_1 plus the platform-IOMMU
    /// feature when offered (the same posture as every driver behind the VT-d/SMMU rungs),
    /// BOTH queues created, the event frame registered with the DMA gate BEFORE any
    /// descriptor names it, every event buffer posted, THEN DRIVER_OK.
    ///
    /// # Safety
    /// `transport` must be bound to a live virtio input device; `H::alloc_frame` must return
    /// identity-mapped frames the caller owns exclusively.
    pub unsafe fn init(mut transport: T) -> Result<Self, InputError> {
        let (_version, device_id) = transport.identity();
        if device_id != VIRTIO_ID_INPUT {
            return Err(InputError::Unsupported("not a virtio input device"));
        }

        transport.set_status(0);
        let mut status = S_ACKNOWLEDGE;
        transport.set_status(status);
        status |= S_DRIVER;
        transport.set_status(status);

        let hi = transport.device_features(1);
        if hi & (1 << F_VERSION_1_BIT) == 0 {
            return Err(InputError::Unsupported(
                "device does not offer VIRTIO_F_VERSION_1",
            ));
        }
        let iommu_platform = hi & (1 << F_IOMMU_PLATFORM_BIT) != 0;
        transport.set_driver_features(0, 0);
        transport.set_driver_features(
            1,
            1 << F_VERSION_1_BIT
                | if iommu_platform {
                    1 << F_IOMMU_PLATFORM_BIT
                } else {
                    0
                },
        );
        transport.set_status(status | S_FEATURES_OK);
        if transport.status() & S_FEATURES_OK == 0 {
            transport.set_status(status | S_FAILED);
            return Err(InputError::Unsupported(
                "device rejected the negotiated features",
            ));
        }

        let mut eventq =
            Virtqueue::new::<H, T>(&mut transport, EVENT_QUEUE).map_err(InputError::Queue)?;
        if eventq.len() < 8 {
            return Err(InputError::Queue("event queue too short for this driver"));
        }
        // The status queue is created (selected, sized, published, ready) but never fed: the
        // device under QEMU never sends on it, and buffers the driver does not understand how
        // to harvest are buffers it should not offer.
        let statusq =
            Virtqueue::new::<H, T>(&mut transport, STATUS_QUEUE).map_err(InputError::Queue)?;

        let events_frame =
            H::alloc_frame().ok_or(InputError::Queue("no frame for the event buffers"))?;
        eventq
            .register_buffer(events_frame, crate::dma::PAGE, "virtio-input.events")
            .map_err(|_| InputError::Queue("the event frame was refused as a DMA region"))?;
        let qsize = eventq.len();
        for slot in 0..qsize {
            eventq
                .add::<H>(
                    slot,
                    (events_frame + slot as usize * EVENT_SIZE) as u64,
                    EVENT_SIZE as u32,
                    true,
                )
                .map_err(|_| InputError::Queue("an event buffer failed the DMA gate"))?;
        }
        // SAFETY: the transport is live and at least one buffer is published.
        eventq.kick::<H, T>(&transport);

        status |= S_DRIVER_OK;
        transport.set_status(status);

        Ok(VirtioInput {
            transport,
            eventq,
            statusq,
            events_frame,
            qsize,
            version1: true,
            status_at_ok: status,
            events_seen: Cell::new(0),
            local_refusals: Cell::new(0),
            _hal: PhantomData,
        })
    }

    // -- config space (select/subsel reads through the ConfigWrite seam) --------------------

    /// Point the config window at `(select, subsel)`. The device repopulates its config
    /// payload from this pair — the read that follows is only as meaningful as this write.
    ///
    /// The ATOMIC WORD is load-bearing and was found live: the pair lives in one aligned
    /// config word, and byte-wide writes are delivered to the device as read-modify-writes
    /// of that word on the word-granular transports — so writing the two bytes separately
    /// makes the second write re-deliver a STALE first byte, the device resolves a pair
    /// nobody asked for, and every read after it comes back as the zeroed no-entry config.
    /// One word carrying BOTH bytes resolves exactly the pair the caller asked for.
    fn select(&mut self, select: u8, subsel: u8) {
        let word = (select as u32) | ((subsel as u32) << 8);
        // SAFETY: the transport is live; the select/subsel word is the defined write.
        unsafe { self.transport.set_config_u32(0, word) }
    }

    /// Read the config answer for `(select, subsel)`: the `size` byte and the 8 payload bytes
    /// at config offset 8 (after the select/subsel/size header).
    fn config_answer(&mut self, select: u8, subsel: u8) -> (u8, u64) {
        self.select(select, subsel);
        // SAFETY: config reads at the offsets every driver reads.
        let hdr = unsafe { self.transport.config_u64(0) };
        let size = ((hdr >> 16) & 0xFF) as u8;
        // SAFETY: as above.
        let payload = unsafe { self.transport.config_u64(8) };
        (size, payload)
    }

    /// The device's declared name, NUL-terminated in config space, read 8 bytes at a time.
    /// Bounded by the config string's own 128-byte extent — a device that never terminates
    /// its name gets a truncated one, not an unbounded read.
    pub fn device_name(&mut self) -> String {
        self.select(CFG_NAME, 0);
        let mut out = String::new();
        'outer: for chunk in 0..16u32 {
            // SAFETY: live transport, aligned config read.
            let word = unsafe { self.transport.config_u64(8 + chunk as usize * 8) };
            for i in 0..8 {
                let b = ((word >> (i * 8)) & 0xFF) as u8;
                if b == 0 {
                    break 'outer;
                }
                out.push(b as char);
            }
        }
        out
    }

    /// Is the device unplugged? Select 0 answers one byte: 0 = plugged in.
    pub fn unplugged(&mut self) -> bool {
        let (size, payload) = self.config_answer(CFG_UNPLUGGED, 0);
        size == 1 && (payload & 0xFF) != 0
    }

    /// The device ids (bustype, vendor, product, version) — identity beyond the name.
    pub fn id_devs(&mut self) -> (u16, u16, u16, u16) {
        let (_size, payload) = self.config_answer(CFG_ID_DEVIDS, 0);
        (
            (payload & 0xFFFF) as u16,
            ((payload >> 16) & 0xFFFF) as u16,
            ((payload >> 32) & 0xFFFF) as u16,
            ((payload >> 48) & 0xFFFF) as u16,
        )
    }

    /// Which event types the device declares for `subsel`: `size` bytes of bitmap, bit `t`
    /// set = the device can emit type `t`. A keyboard declares EV_KEY|EV_SYN; a tablet adds
    /// EV_ABS. Read, not assumed — the decoder's refusals mean something only if the device
    /// never declared the type.
    pub fn ev_bits(&mut self, subsel: u8) -> u64 {
        let (_size, payload) = self.config_answer(CFG_EV_BITS, subsel);
        payload
    }

    /// The absinfo (min, max) of axis `axis`, if the device declares one — the range the
    /// decoder maps against, read from the DEVICE, never assumed.
    pub fn abs_info(&mut self, axis: u16) -> Option<(u32, u32)> {
        let (size, payload) = self.config_answer(CFG_ABS_INFO, axis as u8);
        if size < 20 {
            return None;
        }
        Some((payload as u32, ((payload >> 32) & 0xFFFF_FFFF) as u32))
    }

    // -- the event path ----------------------------------------------------------------------

    /// Harvest ONE event from the device, if the device has produced one, and re-post the
    /// buffer it arrived in. `None` is the common answer — an armed device speaks only when
    /// someone touches it, and the suites measure that silence.
    ///
    /// # Safety
    /// The device must be live (DRIVER_OK reached at init).
    pub unsafe fn next_event(&mut self) -> Option<RawEvent> {
        let (slot, written) = self.eventq.poll_used::<H>()?;
        let addr = self.events_frame + slot as usize * EVENT_SIZE;
        if written as usize != EVENT_SIZE {
            // A device that writes the wrong amount into an event buffer is a device whose
            // records cannot be trusted: the buffer is re-posted, the record is NOT decoded,
            // and the refusal is counted — never silently swallowed.
            self.local_refusals.set(self.local_refusals.get() + 1);
            // SAFETY: slot < qsize; addr was registered at init.
            let _ = self
                .eventq
                .add::<H>(slot, addr as u64, EVENT_SIZE as u32, true);
            // SAFETY: live transport.
            self.eventq.kick::<H, T>(&self.transport);
            return None;
        }
        // SAFETY: addr is inside the registered event frame; the used-ring advance observed
        // by poll_used orders the device's writes before these reads.
        let raw = unsafe {
            let p = addr as *const u8;
            RawEvent {
                ty: le16(p),
                code: le16(p.add(2)),
                value: le32(p.add(4)),
            }
        };
        // SAFETY: slot < qsize; addr registered at init.
        let _ = self
            .eventq
            .add::<H>(slot, addr as u64, EVENT_SIZE as u32, true);
        // SAFETY: live transport.
        self.eventq.kick::<H, T>(&self.transport);
        self.events_seen.set(self.events_seen.get() + 1);
        Some(raw)
    }

    // -- the facts the suites and the IOMMU layer consume ------------------------------------

    pub fn queue_len(&self) -> u16 {
        self.qsize
    }
    pub fn version1_negotiated(&self) -> bool {
        self.version1
    }
    pub fn reached_driver_ok(&self) -> bool {
        self.status_at_ok & S_DRIVER_OK != 0
    }
    pub fn events_seen(&self) -> u64 {
        self.events_seen.get()
    }
    pub fn local_refusals(&self) -> u64 {
        self.local_refusals.get()
    }

    /// The gate denies by default on BOTH queues — proved per queue because each owns its own
    /// registry (REQ-DRV-006).
    pub fn dma_gate_refuses_unregistered(&self) -> bool {
        self.eventq.would_refuse(0x7fff_0000_0000, 64)
            && self.statusq.would_refuse(0x7fff_0000_0000, 64)
    }

    /// LIVE DMA regions: each queue's ring frame, plus the event frame on the eventq.
    pub fn live_dma_regions(&self) -> usize {
        self.eventq.live_regions() + self.statusq.live_regions()
    }

    /// The grants THIS function vouches for, named by owner — the VT-d/SMMU window builder's
    /// input (ADR-075): the registry decides, the tables obey.
    pub fn dma_grants(&self) -> Vec<crate::dma::Grant> {
        let mut g = self.eventq.grants();
        g.extend(self.statusq.grants());
        g
    }
}

// ===========================================================================
// The keyboard decoder: Linux keycodes -> the console's byte alphabet.
// ===========================================================================

/// Unshifted characters for Linux keycodes 0..84. `0` = "not a character key" — handled by
/// name above or dropped, so the table stays a table. The shape mirrors `keymap::UNSHIFTED`,
/// but the INDEX SPACE is the Linux keycode set, because that is what this wire speaks.
/// Generated from the uapi keycode list, not hand-typed: 55 = KPASTERISK, 71..83 = the
/// keypad, and every hole (F-keys, numlock, the modifiers) is a named key or dropped.
const LK_UNSHIFTED: [u8; 84] = [
    0, 0, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'-', b'=', 0, 0, //
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'[', b']', 0, 0, b'a',
    b's', //
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', b'\'', b'`', 0, b'\\', b'z', b'x', b'c',
    b'v', //
    b'b', b'n', b'm', b',', b'.', b'/', 0, b'*', 0, b' ', 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, b'7', b'8', b'9', b'-', b'4', b'5', b'6', b'+', b'1', //
    b'2', b'3', b'0', b'.',
];

/// Shifted characters, same indexing. A separate table rather than a transform: the number
/// row's shift behavior is a layout convention, and an uppercase transform would silently
/// produce nothing for `1` → `!` — the exact trap `keymap::SHIFTED` documents.
const LK_SHIFTED: [u8; 84] = [
    0, 0, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'_', b'+', 0, 0, //
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I', b'O', b'P', b'{', b'}', 0, 0, b'A',
    b'S', //
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':', b'"', b'~', 0, b'|', b'Z', b'X', b'C',
    b'V', //
    b'B', b'N', b'M', b'<', b'>', b'?', 0, b'*', 0, b' ', 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, b'7', b'8', b'9', b'-', b'4', b'5', b'6', b'+', b'1', //
    b'2', b'3', b'0', b'.',
];

/// Keyboard modifier state. Held modifiers are state a DEVICE controls, cleared only by their
/// own release — the rule `keymap::Keymap` states, and this decoder does not get to restate.
/// The state is SEPARATE from the PS/2 decoder's: two keyboards must not hold each other's
/// shift down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct KbState {
    shift: bool,
    ctrl: bool,
    alt: bool,
    caps: bool,
}

/// The keyboard decoder: `virtio_input_event` records in, bytes out — ONLY bytes the console's
/// line editor has a rule for. Fail-closed exactly where `keymap::Keymap` is: an unmapped key
/// contributes nothing, an unknown event type is counted, and no path exists that emits an
/// unchecked byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyDecoder {
    st: KbState,
    /// Events this decoder refused to decode — unknown types, impossible values.
    unknown: u64,
}

impl KeyDecoder {
    pub fn new() -> Self {
        KeyDecoder::default()
    }

    /// Events refused by name — counted, never silent.
    pub fn unknown_refusals(&self) -> u64 {
        self.unknown
    }

    /// Decode one event. Releases, modifiers and unmapped keys produce [`Keys::EMPTY`] — the
    /// common answer, never an error.
    pub fn feed(&mut self, ev: RawEvent) -> Keys {
        if ev.ty != EV_KEY {
            // EV_SYN is the batch separator; a keyboard stream has nothing to commit, so it
            // is empty by design. Anything else is a record this decoder does not model.
            if ev.ty != EV_SYN {
                self.unknown += 1;
            }
            return Keys::EMPTY;
        }
        let press = match ev.value {
            1 | 2 => true,
            0 => false,
            _ => {
                self.unknown += 1;
                return Keys::EMPTY;
            }
        };
        match ev.code {
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                self.st.shift = press;
                return Keys::EMPTY;
            }
            KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                self.st.ctrl = press;
                return Keys::EMPTY;
            }
            KEY_LEFTALT | KEY_RIGHTALT => {
                self.st.alt = press;
                return Keys::EMPTY;
            }
            // Caps toggles on the PRESS only — toggling on the release too cancels itself.
            KEY_CAPSLOCK => {
                if press {
                    self.st.caps = !self.st.caps;
                }
                return Keys::EMPTY;
            }
            KEY_ENTER | KEY_KPENTER => {
                return if press {
                    Keys::one(keymap::CARRIAGE_RETURN)
                } else {
                    Keys::EMPTY
                }
            }
            KEY_BACKSPACE => {
                return if press {
                    Keys::one(keymap::BACKSPACE)
                } else {
                    Keys::EMPTY
                }
            }
            // Tab completes a name — the editor has a rule for it, so the key is delivered.
            KEY_TAB => {
                return if press {
                    Keys::one(crate::shell::TAB)
                } else {
                    Keys::EMPTY
                }
            }
            KEY_ESC => return Keys::EMPTY,
            KEY_UP => {
                return if press {
                    keymap::csi(b'A')
                } else {
                    Keys::EMPTY
                }
            }
            KEY_DOWN => {
                return if press {
                    keymap::csi(b'B')
                } else {
                    Keys::EMPTY
                }
            }
            KEY_RIGHT => {
                return if press {
                    keymap::csi(b'C')
                } else {
                    Keys::EMPTY
                }
            }
            KEY_LEFT => {
                return if press {
                    keymap::csi(b'D')
                } else {
                    Keys::EMPTY
                }
            }
            KEY_HOME => {
                return if press {
                    keymap::csi(b'H')
                } else {
                    Keys::EMPTY
                }
            }
            KEY_END => {
                return if press {
                    keymap::csi(b'F')
                } else {
                    Keys::EMPTY
                }
            }
            KEY_DELETE => {
                return if press {
                    keymap::csi_delete()
                } else {
                    Keys::EMPTY
                }
            }
            KEY_KPSLASH => return if press { Keys::one(b'/') } else { Keys::EMPTY },
            _ => {}
        }

        let i = ev.code as usize;
        if i >= LK_UNSHIFTED.len() {
            return Keys::EMPTY;
        }
        let base = if self.st.shift {
            LK_SHIFTED[i]
        } else {
            LK_UNSHIFTED[i]
        };
        if base == 0 || !press {
            return Keys::EMPTY;
        }

        // A Ctrl chord is delivered only when the editor has a rule for the resulting byte —
        // the same gate `keymap::Keymap` applies, so widening the editor's alphabet widens
        // what EITHER keyboard can send and nothing else can.
        if self.st.ctrl {
            let ctl = base.to_ascii_lowercase();
            if ctl.is_ascii_lowercase() {
                let byte = ctl & 0x1f;
                if crate::shell::editor_accepts(byte) {
                    return Keys::one(byte);
                }
            }
            return Keys::EMPTY;
        }

        // Caps affects letters only; shift cancels it — the number row stays punctuation
        // either way, exactly as the PS/2 decoder rules.
        if self.st.caps && base.is_ascii_alphabetic() {
            return Keys::one(if self.st.shift {
                base.to_ascii_lowercase()
            } else {
                base.to_ascii_uppercase()
            });
        }
        Keys::one(base)
    }
}

// ===========================================================================
// The pointer decoder: absolute axes -> the cursor plane, buttons -> focus.
// ===========================================================================

/// A pointer button, named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
}

/// What one SYN batch commits. The device speaks in axes and button bits; the BATCH is the
/// unit a pointer speaks in — a move is `move_to`, a click is `button` (`true` = DOWN).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PointerBatch {
    pub move_to: Option<(u32, u32)>,
    pub button: Option<(Button, bool)>,
}

/// The pointer decoder. Axis ranges come from the DEVICE's absinfo config (read at bring-up,
/// never assumed); the scanout size comes from the compositor. Mapping an axis value `v` in
/// `0..=max` onto a span of `s` cursor positions: `v * s / (max + 1)`, clamped to `s - 1` —
/// both endpoints exact (`0` → position 0, `max` → the last position) and monotone between.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerDecoder {
    x_max: u32,
    y_max: u32,
    scanout: (u32, u32),
    pend: (Option<u32>, Option<u32>),
    pend_btn: Option<(Button, bool)>,
    unknown: u64,
}

impl PointerDecoder {
    /// `scanout` is the compositor's geometry; call [`PointerDecoder::set_axis`] with the
    /// ranges the device declared before the first event is decoded.
    pub fn new(scanout_w: u32, scanout_h: u32) -> Self {
        PointerDecoder {
            x_max: 0,
            y_max: 0,
            scanout: (scanout_w, scanout_h),
            pend: (None, None),
            pend_btn: None,
            unknown: 0,
        }
    }

    /// The device's own axis ranges (from [`VirtioInput::abs_info`]).
    pub fn set_axis(&mut self, x_max: u32, y_max: u32) {
        self.x_max = x_max;
        self.y_max = y_max;
    }

    /// Events refused by name (unknown types, impossible axes, half batches).
    pub fn unknown_refusals(&self) -> u64 {
        self.unknown
    }

    /// Decode one event. Only an EV_SYN returns a batch — everything else accumulates under
    /// the sync that will commit it, which is what makes a move+click land as ONE decision.
    pub fn feed(&mut self, ev: RawEvent) -> PointerBatch {
        match ev.ty {
            EV_ABS => {
                match ev.code {
                    ABS_X => self.pend.0 = Some(ev.value.min(self.x_max)),
                    ABS_Y => self.pend.1 = Some(ev.value.min(self.y_max)),
                    _ => self.unknown += 1,
                }
                PointerBatch::default()
            }
            EV_KEY => {
                match (ev.code, ev.value) {
                    (BTN_LEFT, 1) => self.pend_btn = Some((Button::Left, true)),
                    (BTN_LEFT, 0) => self.pend_btn = Some((Button::Left, false)),
                    (BTN_RIGHT, 1) => self.pend_btn = Some((Button::Right, true)),
                    (BTN_RIGHT, 0) => self.pend_btn = Some((Button::Right, false)),
                    // A button autorepeat (value 2) is not a click: a HELD button must not
                    // machine-gun the focus. Named and ignored.
                    _ => self.unknown += 1,
                }
                PointerBatch::default()
            }
            EV_SYN => {
                let (sx, sy) = self.scanout;
                let move_to = match (self.pend.0.take(), self.pend.1.take()) {
                    (Some(x), Some(y)) => {
                        Some((self.map(x, self.x_max, sx), self.map(y, self.y_max, sy)))
                    }
                    (None, None) => None,
                    // One axis without the other is a batch a real tablet never sends (both
                    // axes sample together). A half-batch is refused by name and contributes
                    // NO position — a cursor that teleports on half a sample is a cursor
                    // that lies.
                    _ => {
                        self.unknown += 1;
                        None
                    }
                };
                PointerBatch {
                    move_to,
                    button: self.pend_btn.take(),
                }
            }
            _ => {
                self.unknown += 1;
                PointerBatch::default()
            }
        }
    }

    /// Map an axis sample onto one scanout dimension. `max == 0` — a device that never
    /// declared its range — maps everything to the edge rather than somewhere undefined.
    /// Public because the mapping IS the contract: the host proofs use this same function
    /// as their oracle, never a private copy that could drift from it.
    pub fn map(&self, v: u32, max: u32, span: u32) -> u32 {
        if max == 0 || span == 0 {
            return 0;
        }
        let scaled = (v as u64 * span as u64) / (max as u64 + 1);
        scaled.min(span as u64 - 1) as u32
    }
}

// ===========================================================================
// The routing path: the SAME functions the boot suite drives and the live
// desktop pumps. One decode->route path; the suite proves the one the
// machine runs.
// ===========================================================================

/// Route one event from a KEYBOARD device through the machine's input session: decode, then
/// post every byte to the focused surface's bounded queue. Returns the bytes routed; a
/// refused post (`NoFocus`, `Backlogged`) propagates — input that goes nowhere is NAMED.
/// No atomic multi-byte delivery: a sequence refused mid-way leaves its earlier bytes queued
/// (named in the register — the bounded queue makes torn sequences possible in principle).
pub fn route_key(
    dec: &mut KeyDecoder,
    comp: &mut Compositor,
    sess: u64,
    ev: RawEvent,
) -> Result<usize, CompFault> {
    let keys = dec.feed(ev);
    let mut routed = 0;
    for b in keys.as_slice() {
        comp.post_key(sess, *b)?;
        routed += 1;
    }
    Ok(routed)
}

/// Route one event from a POINTER device: batches commit on EV_SYN — the cursor move first
/// (the cursor is the compositor's own plane, session-moved only), then a LEFT press is a
/// FOCUS decision through `focus_at`: the topmost placed surface under the point, or a click
/// on empty space clearing focus. A right press is decoded and NOT routed — there is no
/// context menu to open, and inventing one would be inventing authority.
pub fn route_pointer(
    dec: &mut PointerDecoder,
    comp: &mut Compositor,
    sess: u64,
    ev: RawEvent,
) -> Result<(), CompFault> {
    let batch = dec.feed(ev);
    if let Some((x, y)) = batch.move_to {
        comp.move_cursor(sess, x, y)?;
    }
    if let Some((button, down)) = batch.button {
        if button == Button::Left && down {
            let (x, y) = match batch.move_to {
                Some(p) => p,
                None => comp.cursor().unwrap_or((0, 0)),
            };
            comp.focus_at(sess, x, y)?;
        }
    }
    Ok(())
}

// ===========================================================================
// The in-kernel invariant suite. Runs against the REAL devices on all three
// targets; the exhaustive sweeps live in tests/vinput.rs.
// ===========================================================================

/// The tablet's axis range, MEASURED on the emulator this kernel is qualified on (the same
/// discipline as the display-geometry pin in `gpu_suite`: the pin states what was measured,
/// so a device that changes its answer fails the gate instead of silently re-mapping the
/// user's hand). QEMU's virtio-tablet declares x/y in 0..=32767 — do not "fix" this number.
const PINNED_AXIS_MAX: u32 = 32767;

/// The suite's scanout: the framebuffer console's geometry, so the pointer mapping proved
/// here is the mapping the live desktop runs.
const SUITE_W: u32 = 640;
const SUITE_H: u32 = 240;

/// Map a sample the way the decoder does — the suite's oracle, written once.
fn expect_map(v: u64, max: u64, span: u64) -> u32 {
    ((v * span) / (max + 1)).min(span - 1) as u32
}

pub fn vinput_suite<H: VirtioHal, T: Transport + ConfigWrite, F: FnMut(usize, bool, &str)>(
    kb: &mut VirtioInput<H, T>,
    tab: &mut VirtioInput<H, T>,
    mut log: F,
) -> Result<usize, (usize, &'static str)> {
    let mut n = 0usize;
    macro_rules! check {
        ($name:expr, $cond:expr) => {{
            n += 1;
            let ok = $cond;
            log(n, ok, $name);
            if !ok {
                return Err((n, $name));
            }
        }};
    }

    // 1 — the devices answer for their identity FROM THEIR OWN CONFIG SPACE, and the answers
    //     are PINNED to measurement (the gpu_suite discipline): the names, the plugged-in
    //     state, the declared event types, and the tablet's axis range. A device that changes
    //     what it declares fails here, loudly, instead of quietly re-mapping somebody's hand.
    let kb_name = kb.device_name();
    let tab_name = tab.device_name();
    let tab_x = tab.abs_info(ABS_X);
    let tab_y = tab.abs_info(ABS_Y);
    let kb_types = kb.ev_bits(1); // EV_KEY subsel
    let tab_types = tab.ev_bits(3); // EV_ABS subsel
    check!(
        "vinput: real devices answer for their identity by name and reach DRIVER_OK",
        kb.reached_driver_ok()
            && tab.reached_driver_ok()
            && kb.version1_negotiated()
            && tab.version1_negotiated()
            && kb_name == "QEMU Virtio Keyboard"
            && tab_name == "QEMU Virtio Tablet"
            && !kb.unplugged()
            && !tab.unplugged()
            && classify(&kb_name) == Some(Role::Keyboard)
            && classify(&tab_name) == Some(Role::Tablet)
            && kb_types & (1 << 30) != 0
            && tab_types & ((1 << ABS_X) | (1 << ABS_Y)) != 0
            && tab_x == Some((0, PINNED_AXIS_MAX))
            && tab_y == Some((0, PINNED_AXIS_MAX))
    );

    // 2 — the event path is DMA-gated on BOTH queues: an address nobody registered is refused
    //     as a descriptor, the live regions are exactly ring+ring+events per device, and the
    //     grants the IOMMU layer would consume carry this driver's owner name.
    check!(
        "vinput: the event path is DMA-gated - unregistered addresses are refused, grants carry their owner",
        kb.dma_gate_refuses_unregistered()
            && tab.dma_gate_refuses_unregistered()
            && kb.live_dma_regions() == 3
            && tab.live_dma_regions() == 3
            && kb
                .dma_grants()
                .iter()
                .any(|g| g.owner == "virtio-input.events")
    );

    // 3 — armed silence: both devices are at DRIVER_OK with every buffer posted, and a bounded
    //     poll harvests NOTHING — an input device speaks only when someone touches it, and
    //     that is MEASURED here rather than assumed.
    let mut silent = true;
    for _ in 0..10_000 {
        // SAFETY: both devices reached DRIVER_OK in check 1.
        if unsafe { kb.next_event() }.is_some() || unsafe { tab.next_event() }.is_some() {
            silent = false;
            break;
        }
    }
    check!(
        "vinput: an armed input device sends nothing uninvited - the silence is measured",
        silent && kb.events_seen() == 0 && tab.events_seen() == 0
    );

    // The machine path the rest of the suite drives: a fresh compositor with the desktop's
    // geometry, ONE input session, a panel and a window (with a gap between them, because an
    // "empty space" click must land on space that is actually empty), the window focused —
    // the same shape the live desktop installs.
    let mut comp = Compositor::new(0x0E5E_A1E5, SUITE_W, SUITE_H);
    let sess = comp.open_input_session().unwrap();
    let tok_panel = comp.mint_surface(1, 400, 200).unwrap();
    let tok_win = comp.mint_surface(2, 200, 80).unwrap();
    comp.attach(1, tok_panel, 0, 0).unwrap();
    comp.attach(2, tok_win, 300, 60).unwrap();
    comp.set_focus(sess, 2).unwrap();

    // 4 — a REAL keyboard record routes through the decoder into the focused surface's queue:
    //     press 'a' types 'a', release types nothing, and the keymap's modifier rules hold on
    //     this wire too (shift held, then 'a' types 'A').
    let kdec = &mut KeyDecoder::new();
    let routed = route_key(
        kdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_KEY,
            code: 30,
            value: 1,
        },
    );
    let released = route_key(
        kdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_KEY,
            code: 30,
            value: 0,
        },
    );
    let _ = route_key(
        kdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_KEY,
            code: KEY_LEFTSHIFT,
            value: 1,
        },
    );
    let shifted = route_key(
        kdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_KEY,
            code: 30,
            value: 1,
        },
    );
    let drained = comp.drain_input(2, tok_win).unwrap();
    check!(
        "vinput: a real keyboard record routes through the decoder into the focused surface's queue",
        routed == Ok(1)
            && released == Ok(0)
            && shifted == Ok(1)
            && drained.len() == 2
            && drained[0].kind == crate::compositor::EventKind::Key(b'a')
            && drained[1].kind == crate::compositor::EventKind::Key(b'A')
    );

    // 5 — a REAL pointer record moves the compositor's OWN cursor to the mapped position,
    //     exactly: the axis samples the suite sends map through the device's DECLARED range
    //     to named cursor positions — an interior point, both endpoints, all exact.
    let pdec = &mut PointerDecoder::new(SUITE_W, SUITE_H);
    pdec.set_axis(PINNED_AXIS_MAX, PINNED_AXIS_MAX);
    let move_to = |pdec: &mut PointerDecoder, comp: &mut Compositor, x: u32, y: u32| {
        let _ = route_pointer(
            pdec,
            comp,
            sess,
            RawEvent {
                ty: EV_ABS,
                code: ABS_X,
                value: x,
            },
        );
        let _ = route_pointer(
            pdec,
            comp,
            sess,
            RawEvent {
                ty: EV_ABS,
                code: ABS_Y,
                value: y,
            },
        );
        let _ = route_pointer(
            pdec,
            comp,
            sess,
            RawEvent {
                ty: EV_SYN,
                code: 0,
                value: 0,
            },
        );
    };
    move_to(pdec, &mut comp, 20480, 13653); // interior: the window's center
    let at_mid = comp.cursor();
    move_to(pdec, &mut comp, 0, 0);
    let at_origin = comp.cursor();
    move_to(pdec, &mut comp, PINNED_AXIS_MAX, PINNED_AXIS_MAX);
    let at_corner = comp.cursor();
    check!(
        "vinput: a real pointer record moves the compositor's cursor to the exactly mapped position",
        at_mid == Some((expect_map(20480, PINNED_AXIS_MAX as u64, SUITE_W as u64), expect_map(13653, PINNED_AXIS_MAX as u64, SUITE_H as u64)))
            && at_origin == Some((0, 0))
            && at_corner == Some((SUITE_W - 1, SUITE_H - 1))
    );

    // 6 — a click is a ROUTING decision through the pointer's own batch: a left press at the
    //     cursor focuses the topmost placed surface under the point (the window, here, over
    //     the panel), and a click over the GAP between the surfaces clears focus — the loser
    //     told through its own queue, exactly as `focus_at` promises.
    move_to(pdec, &mut comp, 20480, 13653);
    let _ = route_pointer(
        pdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_KEY,
            code: BTN_LEFT,
            value: 1,
        },
    );
    let _ = route_pointer(
        pdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_SYN,
            code: 0,
            value: 0,
        },
    );
    let focused_by_click = comp.focus();
    // The gap: (600, 199) is inside the scanout, outside the panel (0..400 x 0..200) and
    // outside the window (300..500 x 60..140).
    move_to(pdec, &mut comp, 30720, 27170);
    let _ = route_pointer(
        pdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_KEY,
            code: BTN_LEFT,
            value: 1,
        },
    );
    let _ = route_pointer(
        pdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_SYN,
            code: 0,
            value: 0,
        },
    );
    check!(
        "vinput: a pointer click focuses the surface under the point and an empty click clears focus",
        focused_by_click == Some(2)
            && comp.focus().is_none()
    );
    let _ = comp.drain_input(2, tok_win).unwrap();

    // 7 — records this rung does not model are REFUSED BY NAME and counted, and they change
    //     nothing: an unknown event type routes nothing, an unknown axis is counted, and a
    //     button autorepeat never machine-guns the focus.
    let (focus0, cursor0) = (comp.focus(), comp.cursor());
    let k_before = kdec.unknown_refusals();
    let p_before = pdec.unknown_refusals();
    let _ = route_key(
        kdec,
        &mut comp,
        sess,
        RawEvent {
            ty: 99,
            code: 0,
            value: 1,
        },
    );
    let _ = route_pointer(
        pdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_ABS,
            code: 55,
            value: 1,
        },
    );
    let _ = route_pointer(
        pdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_SYN,
            code: 0,
            value: 0,
        },
    );
    let _ = route_pointer(
        pdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_KEY,
            code: BTN_LEFT,
            value: 2,
        },
    );
    let _ = route_pointer(
        pdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_SYN,
            code: 0,
            value: 0,
        },
    );
    check!(
        "vinput: an unknown record is refused by name and counted - it changes nothing",
        kdec.unknown_refusals() == k_before + 1
            && pdec.unknown_refusals() == p_before + 2
            && comp.focus() == focus0
            && comp.cursor() == cursor0
            && comp.drain_input(2, tok_win).unwrap().is_empty()
    );

    // 8 — the security property, over the WHOLE keycode space in every reachable modifier
    //     state: no record a virtio keyboard can deliver emits a byte the console's editor
    //     has no rule for. The cross-module claim the PS/2 decoder makes, on its own wire.
    let mut ok = true;
    for shift in [false, true] {
        for ctrl in [false, true] {
            for caps in [false, true] {
                let mut dec = KeyDecoder::new();
                if shift {
                    dec.feed(RawEvent {
                        ty: EV_KEY,
                        code: KEY_LEFTSHIFT,
                        value: 1,
                    });
                }
                if ctrl {
                    dec.feed(RawEvent {
                        ty: EV_KEY,
                        code: KEY_LEFTCTRL,
                        value: 1,
                    });
                }
                if caps {
                    dec.feed(RawEvent {
                        ty: EV_KEY,
                        code: KEY_CAPSLOCK,
                        value: 1,
                    });
                }
                for code in 0u32..=0x2FF {
                    for value in [1u32, 2, 0] {
                        let mut probe = dec;
                        let keys = probe.feed(RawEvent {
                            ty: EV_KEY,
                            code: code as u16,
                            value,
                        });
                        for b in keys.as_slice() {
                            if !keymap::Keymap::is_console_byte(*b) {
                                ok = false;
                            }
                        }
                    }
                }
            }
        }
    }
    check!(
        "vinput: no keycode in any modifier state emits a byte the console refuses",
        ok
    );

    // 9 — the axis mapping is exact at its edges: corners map to corners, out-of-range
    //     samples clamp INSIDE, and a device that never declared its range maps everything
    //     to the edge rather than somewhere undefined.
    let mut d = PointerDecoder::new(SUITE_W, SUITE_H);
    d.set_axis(PINNED_AXIS_MAX, PINNED_AXIS_MAX);
    let corners = (
        d.map(0, PINNED_AXIS_MAX, SUITE_W),
        d.map(PINNED_AXIS_MAX, PINNED_AXIS_MAX, SUITE_W),
        d.map(PINNED_AXIS_MAX + 7, PINNED_AXIS_MAX, SUITE_H),
    );
    let undeclared = PointerDecoder::new(SUITE_W, SUITE_H);
    let fallback = undeclared.map(1234, 0, SUITE_W);
    check!(
        "vinput: the axis mapping is exact at the edges and clamps out-of-range samples",
        corners == (0, SUITE_W - 1, SUITE_H - 1) && fallback == 0
    );

    // 10 — input is not pixels: a keystroke on a quiet frame composes NOTHING, and a cursor
    //      move composes only its own glyph regions — the input path changes ROUTING state;
    //      the compositor decides what that costs in pixels, and reports it.
    let mut shadow_bits = alloc::vec![false; (SUITE_W * SUITE_H) as usize];
    struct Shadow<'a>(&'a mut alloc::vec::Vec<bool>);
    impl crate::compositor::Raster for Shadow<'_> {
        fn put(&mut self, x: u32, y: u32, ink: bool) {
            self.0[(y as usize) * (SUITE_W as usize) + x as usize] = ink;
        }
    }
    let _ = comp.set_focus(sess, 2);
    comp.compose_frame(&mut Shadow(&mut shadow_bits)); // settle the earlier routing's damage
    let _ = route_key(
        kdec,
        &mut comp,
        sess,
        RawEvent {
            ty: EV_KEY,
            code: 30,
            value: 1,
        },
    );
    let quiet = comp.compose_frame(&mut Shadow(&mut shadow_bits));
    move_to(pdec, &mut comp, 1000, 1000);
    let moved = comp.compose_frame(&mut Shadow(&mut shadow_bits));
    check!(
        "vinput: a keystroke composes nothing and a pointer move costs exactly its glyph regions",
        quiet.pixels_blitted == 0
            && quiet.cursor_pixels == 0
            && moved.pixels_blitted > 0
            && moved.pixels_blitted <= 2 * 128 + 2 * 64
            && moved.cursor_pixels > 0
    );

    Ok(n)
}
