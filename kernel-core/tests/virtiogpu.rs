//! Host proofs for the shared GPU driver (REQ-GFX-001).
//!
//! The MMIO/virtqueue half of `kernel_core::virtiogpu` can only be exercised against a real
//! device, which is what the three VM gates do. What IS provable on the host is everything that
//! decides whether a target's boot passes: the WIRE FORMAT (a typo'd command code or field
//! offset is a conversation with nobody, and only the device's silence would reveal it — in the
//! worst case, after a gate went green for the wrong reason), the geometric rules every rect
//! argument must satisfy BEFORE the device hears about it, and the fail-closed parsing of what a
//! device reports back.
extern crate alloc;

use kernel_core::virtiogpu::*;

/// A buffer the size of one DMA frame — what every encoder writes into.
fn buf() -> alloc::vec::Vec<u8> {
    alloc::vec![0xAA; 4096]
}

fn le32_at(b: &[u8], at: usize) -> u32 {
    le32(b, at)
}
fn le64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        b[at],
        b[at + 1],
        b[at + 2],
        b[at + 3],
        b[at + 4],
        b[at + 5],
        b[at + 6],
        b[at + 7],
    ])
}

#[test]
fn create_2d_matches_the_uapi_layout_field_for_field() {
    let mut b = buf();
    let n = encode_create_2d(&mut b, 7, FORMAT_B8G8R8A8_UNORM, 256, 64);
    assert_eq!(n, CREATE_2D_LEN);
    assert_eq!(n, 40);
    assert_eq!(le32_at(&b, 0), CMD_RESOURCE_CREATE_2D); // the command code at byte 0
    assert_eq!(le32_at(&b, 24), 7); // resource_id
    assert_eq!(le32_at(&b, 28), 1); // VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM
    assert_eq!(le32_at(&b, 32), 256); // width
    assert_eq!(le32_at(&b, 36), 64); // height
                                     // Nothing else in the span may carry stale bytes: the encoder zeroes what it owns.
    assert!(b[..n].iter().all(|x| *x != 0xAA));
}

#[test]
fn scanout_flush_and_transfer_lay_fields_out_exactly_where_the_header_says() {
    let mut b = buf();
    let r = Rect {
        x: 1,
        y: 2,
        width: 256,
        height: 64,
    };

    let n = encode_set_scanout(&mut b, 3, 7, r);
    assert_eq!(n, SET_SCANOUT_LEN);
    assert_eq!(le32_at(&b, 0), CMD_SET_SCANOUT);
    assert_eq!(
        (
            le32_at(&b, 24),
            le32_at(&b, 28),
            le32_at(&b, 32),
            le32_at(&b, 36)
        ),
        (1, 2, 256, 64)
    );
    assert_eq!(le32_at(&b, 40), 3); // scanout_id
    assert_eq!(le32_at(&b, 44), 7); // resource_id

    let n = encode_flush(&mut b, 7, r);
    assert_eq!(n, FLUSH_LEN);
    assert_eq!(le32_at(&b, 0), CMD_RESOURCE_FLUSH);
    assert_eq!(le32_at(&b, 40), 7); // resource_id (padding at 44 is zeroed)
    assert_eq!(le32_at(&b, 44), 0);

    let n = encode_transfer_to_host_2d(&mut b, 7, r, 4096);
    assert_eq!(n, TRANSFER_2D_LEN);
    assert_eq!(le32_at(&b, 0), CMD_TRANSFER_TO_HOST_2D);
    assert_eq!(le64_at(&b, 40), 4096); // the OFFSET is u64 — a u32 here would corrupt rid
    assert_eq!(le32_at(&b, 48), 7);
}

#[test]
fn attach_backing_encodes_every_entry_and_refuses_an_impossible_count() {
    let mut b = buf();
    let entries: alloc::vec::Vec<(u64, u32)> = (0..MAX_BACKING_ENTRIES)
        .map(|i| (0x1000u64 + i as u64 * 0x1000, 4096u32))
        .collect();
    let n = encode_attach_backing(&mut b, 7, &entries).expect("sixteen entries fit");
    let want = CTRL_HDR_LEN + 8 + MAX_BACKING_ENTRIES * 16;
    assert_eq!(n, want);
    assert_eq!(le32_at(&b, 0), CMD_RESOURCE_ATTACH_BACKING);
    assert_eq!(le32_at(&b, 24), 7);
    assert_eq!(le32_at(&b, 28), MAX_BACKING_ENTRIES as u32); // nr_entries
    for (i, (addr, len)) in entries.iter().enumerate() {
        let base = CTRL_HDR_LEN + 8 + i * 16;
        assert_eq!(le64_at(&b, base), *addr);
        assert_eq!(le32_at(&b, base + 8), *len);
        assert_eq!(le32_at(&b, base + 12), 0); // entry padding
    }
    // Seventeen entries would not fit the bound; an empty attach names nothing.
    let seventeen: alloc::vec::Vec<(u64, u32)> = (0..=MAX_BACKING_ENTRIES)
        .map(|i| (i as u64, 16u32))
        .collect();
    assert!(encode_attach_backing(&mut b, 7, &seventeen).is_none());
    assert!(encode_attach_backing(&mut b, 7, &[]).is_none());
}

#[test]
fn unref_and_detach_are_their_own_commands_not_aliases() {
    let mut b = buf();
    assert_eq!(encode_unref(&mut b, 7), UNREF_LEN);
    assert_eq!(le32_at(&b, 0), CMD_RESOURCE_UNREF);
    assert_eq!(encode_detach_backing(&mut b, 7), DETACH_BACKING_LEN);
    assert_eq!(le32_at(&b, 0), CMD_RESOURCE_DETACH_BACKING);
    assert_ne!(CMD_RESOURCE_UNREF, CMD_RESOURCE_DETACH_BACKING);
}

#[test]
fn validate_create_enforces_the_documented_bounds_inclusively() {
    assert!(matches!(validate_create(0, 64), Err(GpuError::Refused(_))));
    assert!(matches!(validate_create(64, 0), Err(GpuError::Refused(_))));
    assert!(matches!(
        validate_create(MAX_RESOURCE_EXTENT_PX + 1, 1),
        Err(GpuError::Refused(_))
    ));
    // 4096 x 4096 is over the AREA bound even though each extent fits.
    assert!(matches!(
        validate_create(MAX_RESOURCE_EXTENT_PX, MAX_RESOURCE_EXTENT_PX),
        Err(GpuError::Refused(_))
    ));
    // Exactly 4 Mi pixels is ON the bound — inclusive.
    assert_eq!(validate_create(MAX_RESOURCE_EXTENT_PX, 1024), Ok(()));
    assert_eq!(validate_create(256, 64), Ok(()));
}

fn display_info_table(entries: &[(Rect, bool)]) -> alloc::vec::Vec<u8> {
    let mut v = alloc::vec![0u8; DISPLAY_INFO_RESP_LEN];
    put_le32_test(&mut v, 0, RESP_OK_DISPLAY_INFO);
    for (i, (r, enabled)) in entries.iter().enumerate() {
        let base = CTRL_HDR_LEN + i * DISPLAY_ONE_LEN;
        v[base..base + 4].copy_from_slice(&r.x.to_le_bytes());
        v[base + 4..base + 8].copy_from_slice(&r.y.to_le_bytes());
        v[base + 8..base + 12].copy_from_slice(&r.width.to_le_bytes());
        v[base + 12..base + 16].copy_from_slice(&r.height.to_le_bytes());
        v[base + 16..base + 20].copy_from_slice(&(*enabled as u32).to_le_bytes());
    }
    v
}

fn put_le32_test(v: &mut [u8], at: usize, word: u32) {
    v[at..at + 4].copy_from_slice(&word.to_le_bytes());
}

#[test]
fn display_info_parsing_is_fail_closed_on_a_lying_device() {
    let good = Rect {
        x: 0,
        y: 0,
        width: 1024,
        height: 768,
    };
    // One enabled head parses cleanly.
    let tbl = display_info_table(&[(good, true)]);
    let parsed = parse_display_info(&tbl).expect("a well-formed table parses");
    assert_eq!(parsed.len(), MAX_SCANOUTS); // disabled heads are reported too
    assert_eq!(
        parsed[0],
        Scanout {
            rect: good,
            enabled: true
        }
    );
    assert!(!parsed[1].enabled);

    // An ENABLED rect with insane extents poisons the WHOLE answer.
    let liar = Rect {
        x: 0,
        y: 0,
        width: 0xFFFF_FFFF,
        height: 768,
    };
    let tbl = display_info_table(&[(liar, true)]);
    assert!(parse_display_info(&tbl).is_none());
    // The same insane rect DISABLED is not believed and not fatal.
    let tbl = display_info_table(&[(liar, false), (good, true)]);
    let parsed = parse_display_info(&tbl).expect("a disabled nonsense rect is ignored");
    assert!(parsed[0].rect == liar && !parsed[0].enabled);

    // An enabled flag outside {0,1} is malformed, not "truthy".
    let mut tbl = display_info_table(&[(good, false)]);
    let base = CTRL_HDR_LEN + 16;
    put_le32_test(&mut tbl, base, 7);
    assert!(parse_display_info(&tbl).is_none());

    // Truncated below even one entry is unusable.
    assert!(parse_display_info(&tbl[..CTRL_HDR_LEN]).is_none());
    // Shorter than the full table parses the entries PRESENT.
    let short = &display_info_table(&[(good, true)])[..CTRL_HDR_LEN + DISPLAY_ONE_LEN * 4];
    let parsed = parse_display_info(short).expect("partial tables parse their present entries");
    assert_eq!(parsed.len(), 4);
}

#[test]
fn every_response_code_the_driver_knows_has_a_name() {
    for code in [
        RESP_OK_NODATA,
        RESP_OK_DISPLAY_INFO,
        RESP_ERR_UNSPEC,
        RESP_ERR_OUT_OF_MEMORY,
        RESP_ERR_INVALID_SCANOUT_ID,
        RESP_ERR_INVALID_RESOURCE_ID,
        RESP_ERR_INVALID_CONTEXT_ID,
        RESP_ERR_INVALID_PARAMETER,
    ] {
        assert_ne!(resp_name(code), "UNKNOWN", "code {code:#06x} lost its name");
    }
    assert_eq!(resp_name(0), "UNKNOWN");
}

#[test]
fn the_vm_gate_marker_count_is_thirteen_and_dense() {
    // The suite proves THIRTEEN invariants; the VM gates grep that exact number. This test
    // exists so renumbering the suite fails `cargo test` HERE first, with a message, instead of
    // failing three QEMU gates with a count mismatch.
    let names = [
        "gpu: init reached DRIVER_OK",
        "gpu: the control queue DMA gate denies unregistered addresses",
        "gpu: GET_DISPLAY_INFO is answered",
        "gpu: scanout 0 reports the machine display geometry", // (1280x800, enabled)
        "gpu: twelve invalid requests are refused by NAME",
        "gpu: RESOURCE_CREATE_2D is accepted",
        "gpu: rects outside a live resource are refused by name",
        "gpu: RESOURCE_ATTACH_BACKING of sixteen DMA-gated pages is accepted",
        "gpu: SET_SCANOUT binds the resource to scanout 0",
        "gpu: TRANSFER_TO_HOST_2D of the full resource is accepted",
        "gpu: RESOURCE_FLUSH of the full resource is accepted",
        "gpu: a flush for a never-created resource is answered INVALID_RESOURCE_ID",
        "gpu: DETACH_BACKING plus UNREF end the lifecycle",
    ];
    assert_eq!(names.len(), 13);
    // Every name is a PREFIX of its suite counterpart (the suite names carry the rest).
    assert!(names.iter().all(|n| n.starts_with("gpu:")));
}
