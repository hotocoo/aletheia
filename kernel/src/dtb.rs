//! The flattened device tree: the platform's DECLARATION of what exists and where (ADR-074).
//!
//! QEMU generates this tree for the configured MACHINE but, on direct -kernel ELF boots,
//! delivers it through the firmware configuration channel rather than any register: the gate
//! dumps the generated blob (-machine dumpdtb), trims it to its declared size, and republishes
//! it as an opt/ fw_cfg item - the same declared door the custody anchor arrives through
//! (ADR-072). The SMMUv3 rung reads the machine's own claims out of that tree instead of
//! poking fixed addresses:
//!
//! * the arm,smmu-v3 node names the unit's register base, size and phandle;
//! * the PCI host bridge's iommu-map binds Requester IDs to that phandle (stream id = RID
//!   under the identity map the virt machine emits);
//! * the host bridge's own reg (in ROOT address cells) is the ECAM window enumeration walks;
//! * every virtio_mmio node carries NO iommus property here - platform devices are NOT behind
//!   the unit, a fact the gate asserts rather than assumes.
//!
//! Structures are walked by DECLARED length (token stream, property sizes, cell counts), so a
//! malformed tree ends the walk with a named refusal instead of steering it.
use crate::fwcfg::FwCfgMmio;
use kernel_core::fwcfg;

/// The firmware-configuration name under which the GATE publishes the machine's own tree
/// (the -machine dumpdtb blob, trimmed to its declared total size). Same delivery door as the
/// custody anchor (ADR-072): an opt/ fw_cfg item nothing else in the guest may write.
pub const DTB_FWCFG_NAME: &[u8] = b"opt/org.aletheia/dtb";

/// FDT magic. A blob whose first word is not this is not a tree we can read.
const MAGIC: u32 = 0xD00D_FEED;

/// One node's interesting facts.
pub struct NodeInfo {
    pub name: alloc::string::String,
    pub props: alloc::vec::Vec<(alloc::string::String, alloc::vec::Vec<u8>)>,
}

impl NodeInfo {
    pub fn prop(&self, name: &str) -> Option<&[u8]> {
        self.props
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    }
    pub fn compatible_contains(&self, needle: &str) -> bool {
        self.prop("compatible")
            .map(|v| {
                // The value is NUL-terminated strings concatenated; a plain byte find is exact
                // enough for "arm,smmu-v3" and cannot match across a longer name's tail.
                let needle = needle.as_bytes();
                v.windows(needle.len()).any(|w| w == needle)
            })
            .unwrap_or(false)
    }
    pub fn u32_prop(&self, name: &str) -> Option<u32> {
        be32(self.prop(name)?, 0)
    }
}

/// The parsed tree: only what this kernel needs, nothing more.
pub struct Dtb {
    pub nodes: alloc::vec::Vec<NodeInfo>,
    /// The ROOT's address/size cell counts, which every reg decode below needs.
    pub root_cells: (usize, usize),
}

impl Dtb {
    /// Read the tree over the firmware configuration channel and parse it. None = the
    /// platform published no tree (a fact about the machine, like VirtualBox's missing DMAR).
    pub fn load() -> Option<Dtb> {
        let mut bus = FwCfgMmio::new();
        if !fwcfg::signature_matches(&mut bus) {
            return None;
        }
        let entry = match fwcfg::find_file(&mut bus, DTB_FWCFG_NAME) {
            Some(e) => {
                crate::kprintln!(
                    "[dtb] item found: selector {:#x} size {}",
                    e.selector,
                    e.size
                );
                e
            }
            None => {
                crate::kprintln!("[dtb] item NOT in firmware directory");
                return None;
            }
        };
        let mut buf = alloc::vec![0u8; entry.size as usize];
        let got = fwcfg::read_entry(&mut bus, &entry, &mut buf);
        if got != buf.len() || buf.len() < 40 || be32(&buf, 0)? != MAGIC {
            return None;
        }
        Some(Dtb::parse(buf))
    }

    fn parse(buf: alloc::vec::Vec<u8>) -> Dtb {
        let mut root_cells = (2usize, 2usize);
        const BEGIN_NODE: u32 = 1;
        const END_NODE: u32 = 2;
        const PROP: u32 = 3;
        const NOP: u32 = 4;
        const END: u32 = 9;

        // FDT header: magic @0, totalsize @4, off_struct @8, off_strings @12.
        let struct_base = be32(&buf, 8).unwrap_or(0) as usize;
        let strings_base = be32(&buf, 12).unwrap_or(0) as usize;
        let mut i = struct_base;
        struct Frame {
            name: alloc::string::String,
            props: alloc::vec::Vec<(alloc::string::String, alloc::vec::Vec<u8>)>,
        }
        let mut stack: alloc::vec::Vec<Frame> = alloc::vec::Vec::new();
        let mut done: alloc::vec::Vec<NodeInfo> = alloc::vec::Vec::new();

        while let Some(tok) = be32(&buf, i) {
            i += 4;
            match tok {
                NOP => continue,
                END => break,
                BEGIN_NODE => {
                    let Some(end) = buf[i..].iter().position(|&b| b == 0).map(|p| i + p) else {
                        break;
                    };
                    let name = alloc::string::String::from(
                        core::str::from_utf8(&buf[i..end]).unwrap_or("?"),
                    );
                    i = (end + 4) & !3;
                    stack.push(Frame {
                        name,
                        props: alloc::vec::Vec::new(),
                    });
                }
                END_NODE => {
                    let Some(frame) = stack.pop() else { break };
                    let mut path = alloc::string::String::new();
                    for a in &stack {
                        if !a.name.is_empty() {
                            path.push('/');
                            path.push_str(&a.name);
                        }
                    }
                    if !frame.name.is_empty() {
                        path.push('/');
                        path.push_str(&frame.name);
                    }
                    let node = NodeInfo {
                        name: path,
                        props: frame.props,
                    };
                    if node.name.is_empty() {
                        // Root: remember the cell counts every reg decode needs.
                        root_cells = (
                            node.u32_prop("#address-cells").unwrap_or(2) as usize,
                            node.u32_prop("#size-cells").unwrap_or(2) as usize,
                        );
                    }
                    let interesting = node.prop("compatible").is_some()
                        && (node.compatible_contains("arm,smmu-v3")
                            || node.compatible_contains("pci-host-ecam-generic")
                            || node.compatible_contains("virtio,mmio"));
                    if interesting {
                        done.push(node);
                    }
                }
                PROP => {
                    let Some(len) = be32(&buf, i) else { break };
                    let Some(nameoff) = be32(&buf, i + 4) else {
                        break;
                    };
                    i += 8;
                    let val_end = (i + len as usize).min(buf.len());
                    let value = buf[i..val_end].to_vec();
                    i = (i + len as usize + 3) & !3;
                    let e = (strings_base + nameoff as usize).min(buf.len());
                    let nul = buf[e..]
                        .iter()
                        .position(|&b| b == 0)
                        .map(|p| e + p)
                        .unwrap_or(e);
                    let name = alloc::string::String::from(
                        core::str::from_utf8(&buf[e..nul]).unwrap_or("?"),
                    );
                    if let Some(frame) = stack.last_mut() {
                        frame.props.push((name, value));
                    }
                }
                _ => break, // malformed token stream: refuse the walk rather than guess
            }
        }
        Dtb {
            nodes: done,
            root_cells,
        }
    }

    /// The unit + how PCIe binds to it, or which named piece is missing.
    pub fn discover_smmu(&self) -> Result<(SmmuDt, PcieDt), &'static str> {
        let (ac, sc) = self.root_cells;
        let smmu = self
            .nodes
            .iter()
            .find(|n| n.compatible_contains("arm,smmu-v3"))
            .ok_or("no arm,smmu-v3 node in the device tree")?;
        let pcie = self
            .nodes
            .iter()
            .find(|n| n.compatible_contains("pci-host-ecam-generic"))
            .ok_or("no pci-host-ecam-generic node in the device tree")?;

        let smmu_phandle = smmu.u32_prop("phandle").ok_or("smmu node has no phandle")?;
        let (base, size) = decode_reg(smmu.prop("reg").ok_or("smmu node has no reg")?, ac, sc)
            .ok_or("smmu reg malformed")?;

        // iommu-map entries are four single cells: rid-base, phandle, sid-base, sid-len.
        let map_raw = pcie.prop("iommu-map").ok_or("pcie node has no iommu-map")?;
        if map_raw.len() % 16 != 0 {
            return Err("iommu-map is not a whole number of entries");
        }
        let mut map = alloc::vec::Vec::new();
        let mut bound_to_us = false;
        for e in 0..map_raw.len() / 16 {
            let rid_base = be32(map_raw, e * 16).unwrap_or(0);
            let ph = be32(map_raw, e * 16 + 4).unwrap_or(0);
            let sid_base = be32(map_raw, e * 16 + 8).unwrap_or(0);
            let sid_len = be32(map_raw, e * 16 + 12).unwrap_or(0);
            if ph == smmu_phandle {
                bound_to_us = true;
                map.push((rid_base, sid_base, sid_len));
            }
        }
        if !bound_to_us {
            return Err(
                "the PCIe hierarchy is not behind this smmu (iommu-map misses its phandle)",
            );
        }
        let (ecam_base, ecam_size) =
            decode_reg(pcie.prop("reg").ok_or("pcie node has no reg")?, ac, sc)
                .ok_or("pcie reg malformed")?;

        Ok((
            SmmuDt {
                base,
                size,
                phandle: smmu_phandle,
            },
            PcieDt {
                ecam_base,
                ecam_size,
                map,
            },
        ))
    }

    /// Platform devices behind the unit? On this machine the answer must be NO for every
    /// virtio_mmio slot - asserted, because an attached UART or NIC would silently change
    /// what enforcement covers.
    pub fn virtio_mmio_attached_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.compatible_contains("virtio,mmio") && n.prop("iommus").is_some())
            .count()
    }
}

/// The unit as the platform declares it.
pub struct SmmuDt {
    pub base: usize,
    pub size: usize,
    /// Kept for future per-unit DT cross-checks (iommu-map parents beyond the first bridge).
    #[allow(dead_code)]
    pub phandle: u32,
}

/// How the host bridge binds requesters to streams, plus the ECAM window.
pub struct PcieDt {
    pub ecam_base: usize,
    /// Declared window length; kept for future bus walks beyond bus 0.
    #[allow(dead_code)]
    pub ecam_size: usize,
    /// (rid_base, sid_base, sid_len) triples naming THIS unit.
    pub map: alloc::vec::Vec<(u32, u32, u32)>,
}

impl PcieDt {
    /// Stream id for one requester under the declared map (identity on this machine).
    pub fn sid_for_rid(&self, rid: u32) -> Option<u32> {
        self.map.iter().find_map(|&(base, sid_base, len)| {
            (rid >= base && rid - base < len).then_some(sid_base + (rid - base))
        })
    }
}

/// Decode one reg property (single tuple) given the PARENT's cell counts.
fn decode_reg(raw: &[u8], addr_cells: usize, size_cells: usize) -> Option<(usize, usize)> {
    let total = (addr_cells + size_cells) * 4;
    if raw.len() != total {
        return None;
    }
    let mut addr = 0usize;
    for c in 0..addr_cells {
        addr = (addr << 32) | be32(raw, c * 4)? as usize;
    }
    let mut size = 0usize;
    for c in 0..size_cells {
        size = (size << 32) | be32(raw, (addr_cells + c) * 4)? as usize;
    }
    Some((addr, size))
}

#[inline]
fn be32(buf: &[u8], off: usize) -> Option<u32> {
    if off + 4 > buf.len() {
        return None;
    }
    Some(u32::from_be_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
    ]))
}
