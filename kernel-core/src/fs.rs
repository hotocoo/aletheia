//! A named-object filesystem namespace over the journaled block store (REQ-FS-001, ADR-035).
//!
//! [`crate::storage`] gives an all-or-nothing multi-block write; [`crate::device`] gives capability
//! gating over a device. What was missing between them and anything user-visible is a **namespace**:
//! stable names for durable objects, and a mutation model where *creating or deleting a name is as
//! crash-atomic as writing one block*. That is this module.
//!
//! ## Layout (all durable state lives on the device)
//!
//! ```text
//! block 0                       journal commit record  (crate::storage)
//! blocks 1..=MAX_ENTRIES        journal slots           (crate::storage)
//! DATA_START                    directory block         (this module)
//! DATA_START + 1                allocation bitmap       (this module)
//! DATA_START + 2 ..             file data blocks        (this module)
//! ```
//!
//! The directory is one block of fixed 64-byte slots: slot 0 is the header (magic + version), slots
//! `1..` are entries (`name` NUL-padded, first data block, byte length, flags). The bitmap is one
//! block, one bit per data block. Both are ordinary home blocks — so a `Filesystem` holds **no**
//! durable state of its own beyond the journal's sequence, and any mount can read any device.
//!
//! ## Every mutation is one transaction
//!
//! [`Filesystem::create`] and [`Filesystem::remove`] each build a *single* journal transaction
//! containing every block they change — the file's data blocks, the bitmap, and the directory — and
//! commit it. Therefore a crash at any point leaves either the whole name (with its bytes, and its
//! blocks marked allocated) or none of it: there is no state where a directory entry points at blocks
//! the bitmap calls free, or where a name exists with unwritten contents. Because the journal bounds a
//! transaction at [`crate::storage::MAX_ENTRIES`] blocks and two of those slots are always the
//! directory and the bitmap, a file is bounded at [`MAX_FILE_BLOCKS`] blocks — the bound is *refused*,
//! never silently truncated.
//!
//! [`Filesystem::remove`] also **erases**: the file's data blocks are written back as zeros inside the
//! same transaction, the storage-layer twin of erase-on-free (ADR-033). A block returned to the free
//! map carries no bytes of the object that used to live there.
//!
//! ## Not claimed (explicit scope, ADR-035)
//!
//! One flat namespace (no directories), one bitmap block (so at most [`MAX_DATA_BLOCKS`] data blocks),
//! contiguous extents only (a create can fail with [`FsError::NoSpace`] on fragmentation even when the
//! total free count would fit), no per-object integrity beyond the journal's commit checksum, and no
//! capability gating *inside* this module — authority is applied by wrapping the device in
//! [`crate::device::DeviceGuard`], so the namespace is a mechanism and the policy stays in the
//! capability engine.
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::storage::{BlockDevice, Journal, StorageError, BLOCK_SIZE, DATA_START, MAX_ENTRIES};

/// Directory-block magic ("AlFs\0\0\0\1").
const FS_MAGIC: u64 = 0x416C_4673_0000_0001;
/// On-disk format version. A mount refuses anything else rather than guessing.
const FS_VERSION: u64 = 1;

/// Bytes per directory slot. Slot 0 is the header; the rest are entries.
const SLOT: usize = 64;
/// Longest name in bytes (the entry's name field, NUL-padded).
pub const MAX_NAME: usize = 40;
/// Entry field offsets within a slot.
const E_NAME: usize = 0;
const E_START: usize = 40;
const E_LEN: usize = 48;
const E_FLAGS: usize = 56;
/// Entry flag: this slot names a live object.
const F_USED: u64 = 1;

/// The directory block, then the bitmap block, then file data.
pub const DIR_BLOCK: usize = DATA_START;
pub const BITMAP_BLOCK: usize = DATA_START + 1;
pub const FILE_DATA_START: usize = DATA_START + 2;

/// Number of nameable objects (one directory block of 64-byte slots, minus the header slot).
pub const MAX_FILES: usize = BLOCK_SIZE / SLOT - 1;
/// Data blocks a single transaction can carry: the journal's bound minus the directory and bitmap.
pub const MAX_FILE_BLOCKS: usize = MAX_ENTRIES - 2;
/// Largest object this namespace accepts (refused above, never truncated).
pub const MAX_FILE_BYTES: usize = MAX_FILE_BLOCKS * BLOCK_SIZE;
/// Data blocks one bitmap block can track.
pub const MAX_DATA_BLOCKS: usize = BLOCK_SIZE * 8;

/// Why a namespace operation failed. Every failure is a refusal with a reason — never a partial write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    /// The device is too small to hold a directory, a bitmap and any data.
    DeviceTooSmall,
    /// The directory block carries no/unknown magic or version — refuse rather than guess.
    NotFormatted,
    /// The name is empty, too long, or contains a reserved byte (NUL or `/`).
    BadName,
    /// A live object already owns this name.
    Exists,
    /// No object owns this name.
    NotFound,
    /// No free directory slot, or no contiguous free extent large enough.
    NoSpace,
    /// The object exceeds [`MAX_FILE_BYTES`] — one mutation must fit in one transaction.
    TooLarge,
    /// The directory or bitmap describes something impossible (e.g. an extent off the device).
    Corrupt,
    /// The underlying journal/device reported a failure. Surfaced, never swallowed.
    Storage(StorageError),
}

impl From<StorageError> for FsError {
    fn from(e: StorageError) -> Self {
        FsError::Storage(e)
    }
}

fn le64(buf: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(a)
}

fn put64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// A name is 1..=[`MAX_NAME`] bytes and may not contain NUL (the padding sentinel) or `/` (reserved
/// for a future hierarchy, refused now so today's names stay valid when one exists).
fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= MAX_NAME && !b.contains(&0) && !b.contains(&b'/')
}

/// One live directory entry, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    /// First data block (absolute device block index).
    pub start: usize,
    /// Object length in bytes.
    pub len: usize,
}

impl DirEntry {
    /// Blocks the object occupies (a zero-length object occupies none).
    pub fn blocks(&self) -> usize {
        self.len.div_ceil(BLOCK_SIZE)
    }
}

/// The namespace. Holds only the journal (whose durable state is on the device), so a fresh `mount`
/// of the same device sees exactly what the last committed transaction left.
pub struct Filesystem {
    journal: Journal,
}

impl Filesystem {
    /// How many data blocks this device can track (bounded by the bitmap and by the device itself).
    fn data_capacity<D: BlockDevice>(dev: &D) -> usize {
        let n = dev.num_blocks();
        if n <= FILE_DATA_START {
            return 0;
        }
        core::cmp::min(n - FILE_DATA_START, MAX_DATA_BLOCKS)
    }

    /// Write an empty namespace: a directory with only its header and an all-free bitmap, committed as
    /// one transaction so a crash mid-format leaves an unformatted device rather than half a namespace.
    pub fn format<D: BlockDevice>(dev: &mut D) -> Result<(), FsError> {
        if Self::data_capacity(dev) == 0 {
            return Err(FsError::DeviceTooSmall);
        }
        let mut dir = [0u8; BLOCK_SIZE];
        put64(&mut dir, 0, FS_MAGIC);
        put64(&mut dir, 8, FS_VERSION);
        let bitmap = [0u8; BLOCK_SIZE];
        let mut journal = Journal::new();
        journal.commit(dev, &[(DIR_BLOCK, dir), (BITMAP_BLOCK, bitmap)])?;
        Ok(())
    }

    /// Recover the device (replaying a committed transaction if one is pending), then verify the
    /// directory header. Refuses an unformatted or unknown-version device instead of interpreting it.
    pub fn mount<D: BlockDevice>(dev: &mut D) -> Result<Self, FsError> {
        if Self::data_capacity(dev) == 0 {
            return Err(FsError::DeviceTooSmall);
        }
        let mut journal = Journal::new();
        journal.recover(dev)?;
        let dir = journal.read(dev, DIR_BLOCK)?;
        if le64(&dir, 0) != FS_MAGIC || le64(&dir, 8) != FS_VERSION {
            return Err(FsError::NotFormatted);
        }
        Ok(Filesystem { journal })
    }

    fn dir<D: BlockDevice>(&self, dev: &D) -> Result<[u8; BLOCK_SIZE], FsError> {
        Ok(self.journal.read(dev, DIR_BLOCK)?)
    }

    fn bitmap<D: BlockDevice>(&self, dev: &D) -> Result<[u8; BLOCK_SIZE], FsError> {
        Ok(self.journal.read(dev, BITMAP_BLOCK)?)
    }

    fn decode(dir: &[u8; BLOCK_SIZE], slot: usize) -> Option<DirEntry> {
        let off = slot * SLOT;
        if le64(dir, off + E_FLAGS) & F_USED == 0 {
            return None;
        }
        let raw = &dir[off + E_NAME..off + E_NAME + MAX_NAME];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(MAX_NAME);
        let name = core::str::from_utf8(&raw[..end]).ok()?;
        Some(DirEntry {
            name: String::from(name),
            start: le64(dir, off + E_START) as usize,
            len: le64(dir, off + E_LEN) as usize,
        })
    }

    /// Every live name, in slot order.
    pub fn list<D: BlockDevice>(&self, dev: &D) -> Result<Vec<DirEntry>, FsError> {
        let dir = self.dir(dev)?;
        let mut out = Vec::new();
        for slot in 1..=MAX_FILES {
            if let Some(e) = Self::decode(&dir, slot) {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// Look one name up without reading its contents.
    pub fn stat<D: BlockDevice>(&self, dev: &D, name: &str) -> Result<DirEntry, FsError> {
        if !valid_name(name) {
            return Err(FsError::BadName);
        }
        let dir = self.dir(dev)?;
        for slot in 1..=MAX_FILES {
            if let Some(e) = Self::decode(&dir, slot) {
                if e.name == name {
                    return Ok(e);
                }
            }
        }
        Err(FsError::NotFound)
    }

    fn find_slot(dir: &[u8; BLOCK_SIZE], name: &str) -> Option<usize> {
        (1..=MAX_FILES).find(|&slot| Self::decode(dir, slot).is_some_and(|e| e.name == name))
    }

    fn free_slot(dir: &[u8; BLOCK_SIZE]) -> Option<usize> {
        (1..=MAX_FILES).find(|&slot| Self::decode(dir, slot).is_none())
    }

    fn bit(bitmap: &[u8; BLOCK_SIZE], b: usize) -> bool {
        bitmap[b / 8] & (1 << (b % 8)) != 0
    }

    fn set_bit(bitmap: &mut [u8; BLOCK_SIZE], b: usize, on: bool) {
        let mask = 1u8 << (b % 8);
        if on {
            bitmap[b / 8] |= mask;
        } else {
            bitmap[b / 8] &= !mask;
        }
    }

    /// First-fit contiguous run of `count` free data blocks, as a bitmap-relative index.
    fn find_extent(bitmap: &[u8; BLOCK_SIZE], capacity: usize, count: usize) -> Option<usize> {
        if count == 0 {
            return Some(0);
        }
        let mut run = 0usize;
        for b in 0..capacity {
            if Self::bit(bitmap, b) {
                run = 0;
                continue;
            }
            run += 1;
            if run == count {
                return Some(b + 1 - count);
            }
        }
        None
    }

    /// Free data blocks on the device (bitmap-derived, so it reflects the last committed transaction).
    pub fn free_blocks<D: BlockDevice>(&self, dev: &D) -> Result<usize, FsError> {
        let bitmap = self.bitmap(dev)?;
        let cap = Self::data_capacity(dev);
        Ok((0..cap).filter(|&b| !Self::bit(&bitmap, b)).count())
    }

    /// Create `name` holding `data`, as ONE transaction: the data blocks, the bitmap and the directory
    /// all land together or not at all. Refuses a duplicate name, a bad name, an object above
    /// [`MAX_FILE_BYTES`], a full directory, and a device with no contiguous extent large enough.
    pub fn create<D: BlockDevice>(
        &mut self,
        dev: &mut D,
        name: &str,
        data: &[u8],
    ) -> Result<(), FsError> {
        if !valid_name(name) {
            return Err(FsError::BadName);
        }
        if data.len() > MAX_FILE_BYTES {
            return Err(FsError::TooLarge);
        }
        let mut dir = self.dir(dev)?;
        if Self::find_slot(&dir, name).is_some() {
            return Err(FsError::Exists);
        }
        let slot = Self::free_slot(&dir).ok_or(FsError::NoSpace)?;
        let mut bitmap = self.bitmap(dev)?;
        let cap = Self::data_capacity(dev);
        let nblocks = data.len().div_ceil(BLOCK_SIZE);
        let first = Self::find_extent(&bitmap, cap, nblocks).ok_or(FsError::NoSpace)?;

        // The data blocks (zero-padded to the block size — a short tail never leaks adjacent bytes).
        let mut updates: Vec<(usize, [u8; BLOCK_SIZE])> = Vec::with_capacity(nblocks + 2);
        for i in 0..nblocks {
            let mut blk = [0u8; BLOCK_SIZE];
            let off = i * BLOCK_SIZE;
            let end = core::cmp::min(off + BLOCK_SIZE, data.len());
            blk[..end - off].copy_from_slice(&data[off..end]);
            updates.push((FILE_DATA_START + first + i, blk));
            Self::set_bit(&mut bitmap, first + i, true);
        }
        // The directory entry.
        let off = slot * SLOT;
        dir[off..off + SLOT].fill(0);
        dir[off + E_NAME..off + E_NAME + name.len()].copy_from_slice(name.as_bytes());
        put64(&mut dir, off + E_START, (FILE_DATA_START + first) as u64);
        put64(&mut dir, off + E_LEN, data.len() as u64);
        put64(&mut dir, off + E_FLAGS, F_USED);

        updates.push((BITMAP_BLOCK, bitmap));
        updates.push((DIR_BLOCK, dir));
        self.journal.commit(dev, &updates)?;
        Ok(())
    }

    /// Read `name`'s contents. Refuses an entry whose extent does not lie on the device (`Corrupt`)
    /// rather than reading whatever is at that offset.
    pub fn read<D: BlockDevice>(&self, dev: &D, name: &str) -> Result<Vec<u8>, FsError> {
        let e = self.stat(dev, name)?;
        let nblocks = e.blocks();
        if e.start < FILE_DATA_START || e.start + nblocks > dev.num_blocks() {
            return Err(FsError::Corrupt);
        }
        let mut out = Vec::with_capacity(e.len);
        for i in 0..nblocks {
            let blk = self.journal.read(dev, e.start + i)?;
            let take = core::cmp::min(BLOCK_SIZE, e.len - out.len());
            out.extend_from_slice(&blk[..take]);
        }
        Ok(out)
    }

    /// Remove `name`, as ONE transaction: its data blocks are overwritten with zeros (erase on
    /// delete — the storage twin of ADR-033), its bitmap bits are cleared, and its directory slot is
    /// freed. A crash leaves either the whole object or none of it.
    pub fn remove<D: BlockDevice>(&mut self, dev: &mut D, name: &str) -> Result<(), FsError> {
        if !valid_name(name) {
            return Err(FsError::BadName);
        }
        let mut dir = self.dir(dev)?;
        let slot = Self::find_slot(&dir, name).ok_or(FsError::NotFound)?;
        let e = Self::decode(&dir, slot).ok_or(FsError::Corrupt)?;
        let nblocks = e.blocks();
        if e.start < FILE_DATA_START || e.start + nblocks > dev.num_blocks() {
            return Err(FsError::Corrupt);
        }
        let mut bitmap = self.bitmap(dev)?;
        let first = e.start - FILE_DATA_START;

        let mut updates: Vec<(usize, [u8; BLOCK_SIZE])> = Vec::with_capacity(nblocks + 2);
        for i in 0..nblocks {
            updates.push((e.start + i, [0u8; BLOCK_SIZE])); // erase on delete
            Self::set_bit(&mut bitmap, first + i, false);
        }
        let off = slot * SLOT;
        dir[off..off + SLOT].fill(0);

        updates.push((BITMAP_BLOCK, bitmap));
        updates.push((DIR_BLOCK, dir));
        self.journal.commit(dev, &updates)?;
        Ok(())
    }
}

/// A device wrapper that fails after `allow` successful mutations — the crash the journal exists to
/// survive, expressed as a device error so the same proof runs on an emulated RAM disk and on a real
/// driver, in-kernel, with no host support. Reads always succeed (a crash does not corrupt reads).
pub struct FaultDevice<'a, D: BlockDevice> {
    dev: &'a mut D,
    allow: usize,
    used: usize,
}

impl<'a, D: BlockDevice> FaultDevice<'a, D> {
    /// Wrap `dev`, letting the first `allow` write/flush operations through and failing every one
    /// after that.
    pub fn new(dev: &'a mut D, allow: usize) -> Self {
        FaultDevice {
            dev,
            allow,
            used: 0,
        }
    }

    /// Mutations attempted so far (including the one that failed).
    pub fn used(&self) -> usize {
        self.used
    }

    fn tick(&mut self) -> Result<(), StorageError> {
        self.used += 1;
        if self.used > self.allow {
            return Err(StorageError::Device);
        }
        Ok(())
    }
}

impl<D: BlockDevice> BlockDevice for FaultDevice<'_, D> {
    fn num_blocks(&self) -> usize {
        self.dev.num_blocks()
    }
    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        self.dev.read_block(idx, buf)
    }
    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        self.tick()?;
        self.dev.write_block(idx, buf)
    }
    fn flush(&mut self) -> Result<(), StorageError> {
        self.tick()?;
        self.dev.flush()
    }
}

/// Where the commit pivot falls, in device mutations, for a `create` of `nblocks` data blocks.
///
/// `create` performs `u = nblocks + 2` journal writes (data + bitmap + directory), a flush, the
/// commit-record write, a flush, then `u` home writes and a flush. Returns
/// `(before_pivot, after_pivot)`: allowing `before_pivot` mutations kills the create just before its
/// commit record exists (so nothing is committed), and allowing `after_pivot` kills it just after the
/// record is durable but before any home block is updated (so recovery must replay it).
pub fn create_ops(nblocks: usize) -> (usize, usize) {
    let u = nblocks + 2;
    (u + 1, u + 3)
}

/// The arch-independent filesystem invariant suite (REQ-FS-001), reported through a caller-supplied
/// logger exactly like [`crate::selftest::run`]. It runs against ANY [`BlockDevice`] — the emulated
/// RAM disk every target has, and the real virtio-blk device on a target that has one — so the same
/// named behaviors are proved on every CPU and, where a disk exists, over a real driver.
///
/// The device is REFORMATTED: this is a destructive suite, meant for a scratch device.
/// Returns the number of invariants proved, or `(index, name)` of the first that failed.
pub fn selftest_on<D: BlockDevice, F: FnMut(usize, bool, &str)>(
    dev: &mut D,
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
    const FIRST: &str = "fs: a formatted device mounts and is empty";

    // 1. A formatted device mounts, and names nothing.
    if Filesystem::format(dev).is_err() {
        log(1, false, FIRST);
        return Err((1, FIRST));
    }
    let mut fs = match Filesystem::mount(dev) {
        Ok(fs) => fs,
        Err(_) => {
            log(1, false, FIRST);
            return Err((1, FIRST));
        }
    };
    check!(FIRST, fs.list(dev).map(|l| l.is_empty()) == Ok(true));

    // 2. A created object reads back byte for byte (including a partial tail block).
    let body: Vec<u8> = (0..(BLOCK_SIZE + 7)).map(|i| (i % 251) as u8).collect();
    check!(
        "fs: a created object reads back byte for byte",
        fs.create(dev, "alpha", &body).is_ok() && fs.read(dev, "alpha").as_deref() == Ok(&body[..])
    );

    // 3. Names are unique.
    check!(
        "fs: creating a duplicate name is refused (names are unique)",
        fs.create(dev, "alpha", b"other") == Err(FsError::Exists)
    );

    // 4. A malformed name never reaches the device.
    check!(
        "fs: a malformed name is refused (empty, over-long, or reserved byte)",
        fs.create(dev, "", b"x") == Err(FsError::BadName)
            && fs.create(dev, "a/b", b"x") == Err(FsError::BadName)
            && fs.stat(dev, "").is_err()
    );

    // 5. An absent name is a refusal, not an empty read.
    check!(
        "fs: reading an absent name is refused",
        fs.read(dev, "missing") == Err(FsError::NotFound)
    );

    // 6. Two objects never share a data block.
    let beta: Vec<u8> = (0..(2 * BLOCK_SIZE)).map(|i| (i % 97) as u8).collect();
    let mut disjoint = fs.create(dev, "beta", &beta).is_ok();
    if disjoint {
        match (fs.stat(dev, "alpha"), fs.stat(dev, "beta")) {
            (Ok(a), Ok(b)) => {
                disjoint = a.start + a.blocks() <= b.start || b.start + b.blocks() <= a.start;
            }
            _ => disjoint = false,
        }
    }
    check!("fs: two objects never share a data block", disjoint);

    // 7. Removal returns the blocks to the free map — exactly the ones the object held.
    let free_before = fs.free_blocks(dev).unwrap_or(0);
    let (beta_start, beta_blocks) = match fs.stat(dev, "beta") {
        Ok(e) => (e.start, e.blocks()),
        Err(_) => (0, 0),
    };
    let removed = beta_blocks > 0 && fs.remove(dev, "beta").is_ok();
    check!(
        "fs: removing an object returns exactly its blocks to the free map",
        removed
            && fs.free_blocks(dev) == Ok(free_before + beta_blocks)
            && fs.read(dev, "beta") == Err(FsError::NotFound)
    );

    // 8. Erase on delete: the freed blocks carry none of the object's bytes.
    let mut erased = beta_blocks > 0;
    for i in 0..beta_blocks {
        let mut blk = [0u8; BLOCK_SIZE];
        if dev.read_block(beta_start + i, &mut blk).is_err() || blk.iter().any(|&b| b != 0) {
            erased = false;
            break;
        }
    }
    check!(
        "fs: a deleted object's blocks carry no bytes of it (erased on delete)",
        erased
    );

    // 9. An object too large for one transaction is refused, not truncated.
    let big = vec![0xABu8; MAX_FILE_BYTES + 1];
    check!(
        "fs: an object too large for one transaction is refused",
        fs.create(dev, "huge", &big) == Err(FsError::TooLarge)
            && fs.read(dev, "huge") == Err(FsError::NotFound)
    );

    // 10. All durable state is on the device: a fresh mount sees the same namespace.
    let listed = fs.list(dev).unwrap_or_default();
    let remounted = match Filesystem::mount(dev) {
        Ok(fs2) => fs2.list(dev).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    check!(
        "fs: the namespace survives a remount (all durable state is on the device)",
        !listed.is_empty() && listed == remounted
    );

    // 11. A create that dies BEFORE its commit record changes nothing.
    let before = fs.list(dev).unwrap_or_default();
    let free_pre = fs.free_blocks(dev).unwrap_or(0);
    let (pre_pivot, post_pivot) = create_ops(1);
    {
        // The already-mounted `fs` drives the faulty device, so the fault budget counts the create's
        // own mutations only — a fresh mount would spend some of it replaying the last transaction.
        let mut faulty = FaultDevice::new(dev, pre_pivot);
        let _ = fs.create(&mut faulty, "torn-early", &[0x5Au8; 16]);
    }
    let (after, free_post) = match Filesystem::mount(dev) {
        Ok(fs2) => (
            fs2.list(dev).unwrap_or_default(),
            fs2.free_blocks(dev).unwrap_or(usize::MAX),
        ),
        Err(_) => (Vec::new(), usize::MAX),
    };
    check!(
        "fs: a create that dies before its commit record leaves the namespace unchanged",
        after == before && free_post == free_pre
    );

    // 12. A create that dies AFTER its commit record is completed by the next mount (replayed).
    {
        let mut faulty = FaultDevice::new(dev, post_pivot);
        let _ = fs.create(&mut faulty, "torn-late", &[0x77u8; 16]);
    }
    let replayed = match Filesystem::mount(dev) {
        Ok(fs2) => fs2.read(dev, "torn-late").as_deref() == Ok(&[0x77u8; 16][..]),
        Err(_) => false,
    };
    check!(
        "fs: a create that dies after its commit record is completed by the next mount",
        replayed
    );

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemBlockDevice;

    #[test]
    fn suite_holds_on_a_ram_disk() {
        let mut dev = MemBlockDevice::new(FILE_DATA_START + 256);
        let n = selftest_on(&mut dev, |_, _, _| {}).expect("every fs invariant holds");
        assert_eq!(n, 12);
    }
}
