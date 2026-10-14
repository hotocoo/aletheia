//! Intel High Definition Audio driver (REQ-DRV-017, ADR-258): the first sound driver. One
//! controller, the first codec that answers, one output path (converter to pin), one output stream
//! playing a generated tone from a two-entry buffer descriptor list. Polled, like every other
//! driver here.
//!
//! Register offsets and bits follow the Intel High Definition Audio Specification rev 1.0a
//! (section 3.3, controller registers; section 7, codec verbs and parameters) and were checked
//! against QEMU's `hw/audio/intel-hda.c` and `hda-codec.c`, the model the VM gates drive. Nothing
//! about the codec is assumed: the function group, converter and pin are found by walking the
//! codec's own parameters, and its vendor id is reported, never pinned.
//!
//! Fail-closed rules: a controller that never completes the reset handshake, has no output stream
//! descriptor, or whose registers fall outside the mapped window is refused. Every codec response
//! must be solicited and come from the codec address asked; a response from another codec, an
//! unsolicited one, or none within the budget is an error. A codec with no output path is a named
//! skip, not a failure (ADR-228). Every address handed to the controller is a registered DMA region.

use core::marker::PhantomData;
use core::ptr::{read_volatile, write_bytes, write_volatile};

use crate::dma::DmaRegistry;
use crate::virtioblk::VirtioHal;

/// PCI class code: multimedia (04h), HD audio (03h).
pub const CLASS_HDA: u32 = 0x04_03_00;
/// Bytes of BAR0 the caller maps.
pub const MAP_LEN: usize = 0x4000;

// --- Controller registers (spec §3.3), with their access widths -----------------------------------
pub const GCAP: usize = 0x00; // 16
pub const GCTL: usize = 0x08; // 32
pub const STATESTS: usize = 0x0E; // 16
pub const INTCTL: usize = 0x20; // 32
pub const CORBLBASE: usize = 0x40; // 32
pub const CORBUBASE: usize = 0x44; // 32
pub const CORBWP: usize = 0x48; // 16
pub const CORBRP: usize = 0x4A; // 16
pub const CORBCTL: usize = 0x4C; // 8
pub const CORBSIZE: usize = 0x4E; // 8
pub const RIRBLBASE: usize = 0x50; // 32
pub const RIRBUBASE: usize = 0x54; // 32
pub const RIRBWP: usize = 0x58; // 16
pub const RINTCNT: usize = 0x5A; // 16
pub const RIRBCTL: usize = 0x5C; // 8
pub const RIRBSTS: usize = 0x5D; // 8
pub const RIRBSIZE: usize = 0x5E; // 8
/// Stream descriptor `n` at `SD_BASE + SD_STRIDE * n`: input descriptors first, then output.
pub const SD_BASE: usize = 0x80;
pub const SD_STRIDE: usize = 0x20;
pub const SD_CTL: usize = 0x00; // 32 (CTL in 23:0, STS in 31:24)
pub const SD_LPIB: usize = 0x04; // 32
pub const SD_CBL: usize = 0x08; // 32
pub const SD_LVI: usize = 0x0C; // 16
pub const SD_FMT: usize = 0x12; // 16
pub const SD_BDLPL: usize = 0x18; // 32
pub const SD_BDLPU: usize = 0x1C; // 32

const GCTL_CRST: u32 = 1 << 0;
const CORBRP_RST: u16 = 1 << 15;
const CORBCTL_RUN: u8 = 1 << 1;
const RIRBWP_RST: u16 = 1 << 15;
const RIRBCTL_DMAEN: u8 = 1 << 1;
/// Response-interrupt STATUS reporting (RINTCTL). With INTCTL's global enable off no interrupt is
/// delivered, but the status bit must be raised for its write-1-to-clear to release the next verb:
/// QEMU's controller holds CORB after RINTCNT responses until that bit is cleared.
const RIRBCTL_RINTCTL: u8 = 1 << 0;
const RIRBSTS_CLEAR: u8 = 0x05; // response interrupt + overrun, write-1-to-clear
const RING_256: u8 = 0x02; // CORBSIZE/RIRBSIZE: 256 entries
const RIRB_UNSOL: u32 = 1 << 4;
pub const SD_CTL_SRST: u32 = 1 << 0;
pub const SD_CTL_RUN: u32 = 1 << 1;
const SD_CTL_STRM_SHIFT: u32 = 20;
pub const SD_STS_BCIS: u32 = 1 << 26;
/// The stream status byte (CTL bits 31:24), written alone to clear: write-1-to-clear by the spec,
/// and QEMU's 32-bit CTL write stores those bits as given, so clearing through CTL would SET them.
pub const SD_STS: usize = 0x03;
const STS_BCIS: u8 = 1 << 2;

// --- Codec verbs and parameters (spec §7.3) ----------------------------------------------------
const V_GET_PARAMETER: u32 = 0xF00;
const V_GET_CONN_LIST: u32 = 0xF02;
const V_SET_POWER_STATE: u32 = 0x705;
const V_SET_STREAM_ID: u32 = 0x706;
const V_SET_PIN_CTL: u32 = 0x707;
const V_SET_EAPD: u32 = 0x70C;
/// 4-bit verbs carry a 16-bit payload.
const V4_SET_FORMAT: u32 = 0x2;
const V4_SET_AMP: u32 = 0x3;

pub const P_VENDOR_ID: u32 = 0x00;
pub const P_NODE_COUNT: u32 = 0x04;
pub const P_FG_TYPE: u32 = 0x05;
pub const P_WIDGET_CAPS: u32 = 0x09;
pub const P_PIN_CAPS: u32 = 0x0C;
pub const P_CONN_LEN: u32 = 0x0E;

const FG_AUDIO: u32 = 0x01;
const WIDGET_OUTPUT: u32 = 0x0;
const WIDGET_PIN: u32 = 0x4;
const PIN_CAP_OUTPUT: u32 = 1 << 4;
const PIN_CTL_OUT: u32 = 0x40;
/// 48 kHz, 16-bit, two channels (spec §3.7.1 stream format).
pub const FORMAT_48K_16_STEREO: u16 = 0x0011;
/// Output amp, both channels, unmuted, gain 0x3F (spec §7.3.3.7).
const AMP_OUT_BOTH: u32 = 0xB000 | 0x3F;

/// The stream tag the output stream and the converter agree on.
const STREAM_TAG: u32 = 1;
/// Frames per BDL entry buffer; two entries, each one frame.
const BUF_BYTES: usize = crate::dma::PAGE;
/// Samples per tone period at 48 kHz: 64 gives 750 Hz, and a 1024-sample buffer holds exactly 16
/// periods, so the looping BDL plays a seamless tone.
pub const TONE_PERIOD: usize = 64;
/// Peak amplitude of the triangle tone.
const TONE_PEAK: i32 = 8000;

/// Time budget for a codec response or a register handshake.
pub const BUDGET_NS: u64 = 1_000_000_000;

/// Register access at the widths the HDA register map defines. Writing a byte register with a
/// 32-bit store would also write its neighbours on silicon, so widths are part of the seam.
pub trait HdaRegs {
    fn r8(&self, off: usize) -> u8;
    fn r16(&self, off: usize) -> u16;
    fn r32(&self, off: usize) -> u32;
    fn w8(&self, off: usize, v: u8);
    fn w16(&self, off: usize, v: u16);
    fn w32(&self, off: usize, v: u32);
}

/// BAR0 through an identity-mapped device-memory window.
pub struct MmioRegs {
    base: usize,
}

impl MmioRegs {
    /// # Safety
    /// `base` must be the mapped BAR0 of an HDA controller, at least [`MAP_LEN`] bytes.
    pub unsafe fn new(base: usize) -> Self {
        MmioRegs { base }
    }
}

impl HdaRegs for MmioRegs {
    fn r8(&self, off: usize) -> u8 {
        // SAFETY: `new` requires a mapped BAR0; every offset used is inside MAP_LEN.
        unsafe { read_volatile((self.base + off) as *const u8) }
    }
    fn r16(&self, off: usize) -> u16 {
        // SAFETY: as above.
        unsafe { read_volatile((self.base + off) as *const u16) }
    }
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: as above.
        unsafe { read_volatile((self.base + off) as *const u32) }
    }
    fn w8(&self, off: usize, v: u8) {
        // SAFETY: as above.
        unsafe { write_volatile((self.base + off) as *mut u8, v) }
    }
    fn w16(&self, off: usize, v: u16) {
        // SAFETY: as above.
        unsafe { write_volatile((self.base + off) as *mut u16, v) }
    }
    fn w32(&self, off: usize, v: u32) {
        // SAFETY: as above.
        unsafe { write_volatile((self.base + off) as *mut u32, v) }
    }
}

/// Why the controller or codec was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdaError {
    /// A handshake or response did not arrive within the budget.
    Timeout,
    /// No output stream descriptor, or registers outside the mapped window.
    Capabilities,
    /// No frame, or a frame the DMA gate refused.
    Memory,
    /// A response was unsolicited where a solicited one was owed.
    Unsolicited,
    /// A response came from another codec address than the one asked.
    WrongCodec,
    /// The codec has no audio function group, converter, or output pin connected to it.
    NoOutputPath,
    /// The stream ran but its position never advanced, or no buffer completed.
    StreamStalled,
}

/// What init observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdaReport {
    pub gcap: u16,
    pub input_streams: u8,
    pub output_streams: u8,
    pub codecs: u16,
}

/// The output path found in a codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputPath {
    pub cad: u8,
    pub vendor: u32,
    pub afg: u8,
    pub dac: u8,
    pub pin: u8,
}

/// A live HDA controller.
pub struct Hda<H: VirtioHal, R: HdaRegs> {
    regs: R,
    rings: usize,
    corb_wp: u16,
    rirb_rp: u16,
    sd: usize,
    dma: DmaRegistry,
    budget_ns: u64,
    _hal: PhantomData<H>,
}

fn frame<H: VirtioHal>(dma: &mut DmaRegistry, owner: &'static str) -> Result<usize, HdaError> {
    let f = H::alloc_frame().ok_or(HdaError::Memory)?;
    // SAFETY: a frame the HAL handed us exclusively, identity-mapped.
    unsafe { write_bytes(f as *mut u8, 0, crate::dma::PAGE) };
    dma.register(f, crate::dma::PAGE, owner)
        .map_err(|_| HdaError::Memory)?;
    Ok(f)
}

/// One sample of the triangle tone, at sample index `i`.
pub fn tone_sample(i: usize) -> i16 {
    let p = (i % TONE_PERIOD) as i32;
    let q = TONE_PERIOD as i32 / 4;
    let v = if p < q {
        p
    } else if p < 3 * q {
        2 * q - p
    } else {
        p - 4 * q
    };
    (v * TONE_PEAK / q) as i16
}

impl<H: VirtioHal, R: HdaRegs> Hda<H, R> {
    /// Reset the controller (CRST low then high), bring up CORB and RIRB, interrupts off.
    ///
    /// # Safety
    /// `regs` must reach an HDA controller's BAR0, mapped for `window` bytes, that this kernel
    /// owns; `H::alloc_frame` must return identity-mapped frames the caller owns exclusively.
    pub unsafe fn init(regs: R, window: usize) -> Result<(Self, HdaReport), HdaError> {
        let gcap = regs.r16(GCAP);
        let iss = ((gcap >> 8) & 0xF) as u8;
        let oss = ((gcap >> 12) & 0xF) as u8;
        let bss = ((gcap >> 3) & 0x1F) as usize;
        let total = iss as usize + oss as usize + bss;
        if oss == 0 || SD_BASE + SD_STRIDE * total > window {
            return Err(HdaError::Capabilities);
        }
        let mut h = Hda {
            regs,
            rings: 0,
            corb_wp: 0,
            rirb_rp: 0,
            sd: SD_BASE + SD_STRIDE * iss as usize,
            dma: DmaRegistry::new(),
            budget_ns: BUDGET_NS,
            _hal: PhantomData,
        };
        // Stop the rings before reset (spec §4.2): a controller firmware left running must not
        // DMA into frames this kernel is about to reuse.
        h.regs.w8(CORBCTL, 0);
        h.regs.w8(RIRBCTL, 0);
        h.regs.w32(GCTL, h.regs.r32(GCTL) & !GCTL_CRST);
        h.wait(|r| r.r32(GCTL) & GCTL_CRST == 0)?;
        h.regs.w32(GCTL, h.regs.r32(GCTL) | GCTL_CRST);
        h.wait(|r| r.r32(GCTL) & GCTL_CRST != 0)?;
        // Codecs announce themselves in STATESTS within 521 us of reset leaving (spec §4.3); give
        // them a millisecond of the platform's clock.
        let t0 = H::now_ns();
        while H::now_ns().wrapping_sub(t0) < 1_000_000 {
            core::hint::spin_loop();
        }
        let codecs = h.regs.r16(STATESTS) & 0x7FFF;
        h.regs.w32(INTCTL, 0);

        h.rings = frame::<H>(&mut h.dma, "hda.corb-rirb")?;
        let corb = h.rings as u64;
        let rirb = (h.rings + 2048) as u64;
        h.regs.w32(CORBLBASE, corb as u32);
        h.regs.w32(CORBUBASE, (corb >> 32) as u32);
        h.regs.w8(CORBSIZE, RING_256);
        h.regs.w16(CORBWP, 0);
        // CORBRP reset handshake (spec §4.4.1.3): set the bit, see it set, clear it, see it clear.
        // Some controllers never show the bit set; only the clearing is required.
        h.regs.w16(CORBRP, CORBRP_RST);
        let _ = h.wait(|r| r.r16(CORBRP) & CORBRP_RST != 0);
        h.regs.w16(CORBRP, 0);
        h.wait(|r| r.r16(CORBRP) & CORBRP_RST == 0)?;
        h.regs.w32(RIRBLBASE, rirb as u32);
        h.regs.w32(RIRBUBASE, (rirb >> 32) as u32);
        h.regs.w8(RIRBSIZE, RING_256);
        h.regs.w16(RIRBWP, RIRBWP_RST);
        h.regs.w16(RINTCNT, 1);
        h.regs.w8(RIRBSTS, RIRBSTS_CLEAR);
        h.regs.w8(RIRBCTL, RIRBCTL_DMAEN | RIRBCTL_RINTCTL);
        h.regs.w8(CORBCTL, CORBCTL_RUN);
        let report = HdaReport {
            gcap,
            input_streams: iss,
            output_streams: oss,
            codecs,
        };
        Ok((h, report))
    }

    fn wait(&self, done: impl Fn(&R) -> bool) -> Result<(), HdaError> {
        let t0 = H::now_ns();
        while !done(&self.regs) {
            if H::now_ns().wrapping_sub(t0) > self.budget_ns {
                return Err(HdaError::Timeout);
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    pub fn dma_gate_refuses_unregistered(&self) -> bool {
        !self.dma.visible(0x7fff_0000_0000, 64) && !self.dma.visible(self.rings, 2 * 4096)
    }

    pub fn dma_regions(&self) -> usize {
        self.dma.live_regions()
    }

    pub fn dma_grants(&self) -> alloc::vec::Vec<crate::dma::Grant> {
        self.dma.grants()
    }

    /// Send one verb through CORB and wait for its solicited response from the same codec.
    pub fn verb(&mut self, cad: u8, nid: u8, payload: u32) -> Result<u32, HdaError> {
        let v = (cad as u32 & 0xF) << 28 | (nid as u32 & 0x7F) << 20 | (payload & 0xF_FFFF);
        let wp = (self.corb_wp + 1) & 0xFF;
        // SAFETY: CORB entry `wp` lies in the rings frame this controller owns.
        unsafe { write_volatile((self.rings + 4 * wp as usize) as *mut u32, v) };
        H::barrier();
        self.corb_wp = wp;
        self.regs.w16(CORBWP, wp);
        let want = (self.rirb_rp + 1) & 0xFF;
        self.wait(|r| r.r16(RIRBWP) & 0xFF == want)?;
        H::barrier();
        let e = self.rings + 2048 + 8 * want as usize;
        // SAFETY: RIRB entry `want` lies in the rings frame this controller owns.
        let (resp, ex) = unsafe {
            (
                read_volatile(e as *const u32),
                read_volatile((e + 4) as *const u32),
            )
        };
        self.rirb_rp = want;
        // Clearing the response status lets the controller take the next verb (RINTCNT = 1).
        self.regs.w8(RIRBSTS, RIRBSTS_CLEAR);
        if ex & RIRB_UNSOL != 0 {
            return Err(HdaError::Unsolicited);
        }
        if ex & 0xF != cad as u32 & 0xF {
            return Err(HdaError::WrongCodec);
        }
        Ok(resp)
    }

    fn param(&mut self, cad: u8, nid: u8, p: u32) -> Result<u32, HdaError> {
        self.verb(cad, nid, V_GET_PARAMETER << 8 | p)
    }

    /// The nodes `nid` lists as its connections (short form, up to eight).
    fn connections(&mut self, cad: u8, nid: u8) -> Result<alloc::vec::Vec<u8>, HdaError> {
        let len = (self.param(cad, nid, P_CONN_LEN)? & 0x7F) as usize;
        let mut out = alloc::vec::Vec::new();
        for i in (0..len.min(8)).step_by(4) {
            let r = self.verb(cad, nid, V_GET_CONN_LIST << 8 | i as u32)?;
            for k in 0..4.min(len - i) {
                out.push((r >> (8 * k)) as u8 & 0x7F);
            }
        }
        Ok(out)
    }

    /// Walk codec `cad` for an audio function group, an output converter in it, and an
    /// output-capable pin whose connection list reaches that converter directly or through one
    /// intermediate widget.
    pub fn find_output(&mut self, cad: u8) -> Result<OutputPath, HdaError> {
        let vendor = self.param(cad, 0, P_VENDOR_ID)?;
        let root = self.param(cad, 0, P_NODE_COUNT)?;
        let (fg0, fgn) = ((root >> 16) as u8, (root & 0xFF) as u8);
        for afg in fg0..fg0.saturating_add(fgn) {
            if self.param(cad, afg, P_FG_TYPE)? & 0xFF != FG_AUDIO {
                continue;
            }
            let sub = self.param(cad, afg, P_NODE_COUNT)?;
            let (w0, wn) = ((sub >> 16) as u8, (sub & 0xFF) as u8);
            let mut dacs = alloc::vec::Vec::new();
            let mut pins = alloc::vec::Vec::new();
            for nid in w0..w0.saturating_add(wn) {
                let caps = self.param(cad, nid, P_WIDGET_CAPS)?;
                match (caps >> 20) & 0xF {
                    WIDGET_OUTPUT => dacs.push(nid),
                    WIDGET_PIN if self.param(cad, nid, P_PIN_CAPS)? & PIN_CAP_OUTPUT != 0 => {
                        pins.push(nid)
                    }
                    _ => {}
                }
            }
            for &pin in &pins {
                for c in self.connections(cad, pin)? {
                    if dacs.contains(&c) {
                        return Ok(OutputPath {
                            cad,
                            vendor,
                            afg,
                            dac: c,
                            pin,
                        });
                    }
                    if let Some(&dac) = self.connections(cad, c)?.iter().find(|d| dacs.contains(d))
                    {
                        return Ok(OutputPath {
                            cad,
                            vendor,
                            afg,
                            dac,
                            pin,
                        });
                    }
                }
            }
        }
        Err(HdaError::NoOutputPath)
    }

    /// Power the path, set the converter's format and stream tag, enable the pin's output and
    /// unmute the amps on the way.
    pub fn configure(&mut self, p: &OutputPath) -> Result<(), HdaError> {
        for nid in [p.afg, p.dac, p.pin] {
            self.verb(p.cad, nid, V_SET_POWER_STATE << 8)?;
        }
        self.verb(
            p.cad,
            p.dac,
            V4_SET_FORMAT << 16 | FORMAT_48K_16_STEREO as u32,
        )?;
        self.verb(p.cad, p.dac, V_SET_STREAM_ID << 8 | STREAM_TAG << 4)?;
        self.verb(p.cad, p.dac, V4_SET_AMP << 16 | AMP_OUT_BOTH)?;
        self.verb(p.cad, p.pin, V_SET_PIN_CTL << 8 | PIN_CTL_OUT)?;
        self.verb(p.cad, p.pin, V4_SET_AMP << 16 | AMP_OUT_BOTH)?;
        self.verb(p.cad, p.pin, V_SET_EAPD << 8 | 0x2)?;
        Ok(())
    }

    fn sdr(&self, off: usize) -> u32 {
        self.regs.r32(self.sd + off)
    }

    /// Play the generated tone on the first output stream for `ms` milliseconds of the platform's
    /// clock: a two-entry BDL, each entry one frame of tone with IOC set, looping. Returns the
    /// buffers completed (BCIS observations). The stream is stopped and reset before returning,
    /// whatever happened.
    pub fn play_tone(&mut self, ms: u64) -> Result<u32, HdaError> {
        let bdl = frame::<H>(&mut self.dma, "hda.bdl")?;
        let bufs = [
            frame::<H>(&mut self.dma, "hda.pcm")?,
            frame::<H>(&mut self.dma, "hda.pcm")?,
        ];
        let frames_per_buf = BUF_BYTES / 4;
        for (b, &f) in bufs.iter().enumerate() {
            for i in 0..frames_per_buf {
                let s = tone_sample(b * frames_per_buf + i);
                // SAFETY: inside the PCM frame this controller owns; left then right channel.
                unsafe {
                    write_volatile((f + 4 * i) as *mut i16, s);
                    write_volatile((f + 4 * i + 2) as *mut i16, s);
                }
            }
            // SAFETY: BDL entry `b` (16 bytes) inside the BDL frame.
            unsafe {
                let e = bdl + 16 * b;
                write_volatile(e as *mut u64, f as u64);
                write_volatile((e + 8) as *mut u32, BUF_BYTES as u32);
                write_volatile((e + 12) as *mut u32, 1); // IOC
            }
        }
        H::barrier();
        let sd = self.sd;
        // Reset the stream (SRST set, seen set, cleared, seen clear), then program it.
        self.regs.w32(sd + SD_CTL, SD_CTL_SRST);
        let _ = self.wait(|r| r.r32(sd + SD_CTL) & SD_CTL_SRST != 0);
        self.regs.w32(sd + SD_CTL, 0);
        self.wait(|r| r.r32(sd + SD_CTL) & SD_CTL_SRST == 0)?;
        self.regs.w32(sd + SD_BDLPL, bdl as u32);
        self.regs.w32(sd + SD_BDLPU, (bdl as u64 >> 32) as u32);
        self.regs.w32(sd + SD_CBL, 2 * BUF_BYTES as u32);
        self.regs.w16(sd + SD_LVI, 1);
        self.regs.w16(sd + SD_FMT, FORMAT_48K_16_STEREO);
        let ctl = STREAM_TAG << SD_CTL_STRM_SHIFT;
        self.regs.w8(sd + SD_STS, STS_BCIS);
        self.regs.w32(sd + SD_CTL, ctl);
        self.regs.w32(sd + SD_CTL, ctl | SD_CTL_RUN);

        let mut completed = 0u32;
        let mut moved = false;
        let t0 = H::now_ns();
        while H::now_ns().wrapping_sub(t0) < ms * 1_000_000 {
            let v = self.sdr(SD_CTL);
            moved |= self.sdr(SD_LPIB) != 0;
            if v & SD_STS_BCIS != 0 {
                completed += 1;
                self.regs.w8(sd + SD_STS, STS_BCIS);
            }
            core::hint::spin_loop();
        }
        self.stop_stream();
        if !moved || completed == 0 {
            return Err(HdaError::StreamStalled);
        }
        Ok(completed)
    }

    fn stop_stream(&mut self) {
        let sd = self.sd;
        self.regs.w32(sd + SD_CTL, 0);
        let _ = self.wait(|r| r.r32(sd + SD_CTL) & SD_CTL_RUN == 0);
        self.regs.w32(sd + SD_CTL, SD_CTL_SRST);
        self.regs.w32(sd + SD_CTL, 0);
    }

    /// Stop the rings and hold the controller in reset, so nothing this kernel told it about is
    /// touched afterwards (ADR-228). Returns whether the reset was seen.
    pub fn halt(&mut self) -> bool {
        self.stop_stream();
        self.regs.w8(CORBCTL, 0);
        self.regs.w8(RIRBCTL, 0);
        self.regs.w32(GCTL, self.regs.r32(GCTL) & !GCTL_CRST);
        self.wait(|r| r.r32(GCTL) & GCTL_CRST == 0).is_ok()
    }
}

impl<H: VirtioHal, R: HdaRegs> Drop for Hda<H, R> {
    fn drop(&mut self) {
        if self.regs.r32(GCTL) & GCTL_CRST != 0 {
            self.halt();
        }
    }
}

/// Milliseconds of tone the boot suite plays.
pub const SUITE_PLAY_MS: u64 = 250;

/// The boot suite over a real controller (REQ-DRV-017). `Ok(0)` when no codec answers or no codec
/// has an output path: a named skip after bring-up, not a failure (ADR-228).
///
/// # Safety
/// As [`Hda::init`].
pub unsafe fn device_suite<H: VirtioHal, R: HdaRegs, F: FnMut(usize, bool, &str)>(
    regs: R,
    window: usize,
    log: &mut F,
    report: &mut dyn FnMut(&HdaReport, Option<&OutputPath>, u32),
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
    let init = Hda::<H, R>::init(regs, window);
    check!(
        "hda: controller reset handshake completes, CORB and RIRB running, an output stream exists",
        init.is_ok()
    );
    let Ok((mut h, rep)) = init else {
        unreachable!()
    };
    check!(
        "hda: DMA gate refuses an address no ring or buffer registered",
        h.dma_gate_refuses_unregistered()
    );
    let Some(cad) = (0..15u8).find(|c| rep.codecs & (1 << c) != 0) else {
        report(&rep, None, 0);
        return Ok(0);
    };
    let path = h.find_output(cad);
    if path == Err(HdaError::NoOutputPath) {
        report(&rep, None, 0);
        return Ok(0);
    }
    check!(
        "hda: the codec answers solicited from the address asked; an output converter and pin found",
        path.is_ok()
    );
    let Ok(path) = path else { unreachable!() };
    check!(
        "hda: path powered, 48 kHz 16-bit stereo set, stream tagged, pin output enabled, amps unmuted",
        h.configure(&path).is_ok()
    );
    let played = h.play_tone(SUITE_PLAY_MS);
    report(&rep, Some(&path), *played.as_ref().unwrap_or(&0));
    check!(
        "hda: the output stream runs, its position advances and buffers complete (BCIS)",
        played.is_ok()
    );
    check!(
        "hda: the stream stops and the controller is held in reset, nothing in flight",
        h.halt()
    );
    Ok(n)
}
