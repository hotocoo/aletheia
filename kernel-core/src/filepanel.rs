//! The file panel — the desktop's view of the namespace, as a bounded model with no device in it.
//!
//! A desktop that cannot show you your own files is not a desktop. But the obvious way to build
//! one is wrong for this kernel: hand the desktop a block device, make `Desktop<H, T>` also
//! generic over `BlockDevice`, and let the pump read the disk. That would put I/O on the
//! compositor's hot path, make a slow device a frozen cursor, and give three kernels a fourth
//! type parameter to thread.
//!
//! So the panel owns no device and performs no I/O. It is a MODEL: a bounded list of rows, a
//! selection, and a scroll window, with a `replace_listing` that the platform calls from wherever
//! it already holds the filesystem — the same shape as [`crate::desktop::Desktop::pump`] being
//! HANDED the frame allocator's reading rather than calling the allocator.
//!
//! Two properties matter more than features here:
//!
//! * **It never allocates after construction.** The row storage is sized once and reused for
//!   every listing. On a heap that never frees (ADR-063) a per-refresh allocation is a leak, and
//!   a file panel refreshes every time anything on the disk changes.
//! * **It is total.** Every operation is defined on an empty listing, on a listing larger than
//!   capacity, and on a name longer than the row. Truncation is COUNTED rather than silent,
//!   because a panel that quietly stops showing your files is worse than one that says it did.

use alloc::vec::Vec;

use crate::fs::Filesystem;
use crate::shell::ServicePhase;
use crate::storage::BlockDevice;
use crate::textgrid::TextGrid;

/// The widest name the panel stores. Longer names are kept truncated and counted — the panel is
/// a view, and a view that allocates to fit its input is a view that can be made to exhaust the
/// heap by creating one long filename.
pub const NAME_CAP: usize = 24;

/// How many rows the panel holds. A listing longer than this is truncated and counted.
pub const ROW_CAP: usize = 64;

/// One row: a name, its length in bytes, and the blocks it occupies. Fixed size by construction,
/// so the row storage can be reused forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileRow {
    name: [u8; NAME_CAP],
    name_len: u8,
    /// Object length in bytes, as the namespace reports it.
    pub len: u64,
    /// Blocks the object occupies.
    pub blocks: u32,
    /// Whether the stored name is shorter than the one the namespace gave.
    pub name_truncated: bool,
}

impl FileRow {
    /// Build a row from a namespace entry, truncating the name to [`NAME_CAP`] and SAYING so.
    pub fn new(name: &[u8], len: u64, blocks: u32) -> Self {
        let keep = name.len().min(NAME_CAP);
        let mut buf = [0u8; NAME_CAP];
        buf[..keep].copy_from_slice(&name[..keep]);
        FileRow {
            name: buf,
            name_len: keep as u8,
            len,
            blocks,
            name_truncated: name.len() > NAME_CAP,
        }
    }

    /// The stored name.
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

/// What a press or key did. Reported rather than performed: the panel decides WHICH row, the
/// desktop decides what opening a row means, and neither can act as the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelAction {
    /// The selection moved to this row index.
    Selected(usize),
    /// This row index was activated (double click, Enter).
    Activated(usize),
    /// Nothing happened, and this is why.
    Refused(PanelRefusal),
}

/// Why the panel did nothing. Named, so a refusal can be counted and tested rather than inferred
/// from an absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelRefusal {
    /// There is nothing to select or activate.
    Empty,
    /// The coordinate or index lies outside the listing.
    OutOfRange,
}

/// The desktop's view of the namespace.
pub struct FilePanel {
    rows: Vec<FileRow>,
    selected: usize,
    scroll: usize,
    visible_rows: usize,
    free_blocks: u32,
    total_blocks: u32,
    /// Listings that did not fit in [`ROW_CAP`], counted. A panel that silently stops showing
    /// files is worse than one that admits it.
    pub listings_truncated: u64,
    /// Entries dropped across every truncated listing, counted.
    pub entries_dropped: u64,
    /// Refusals, counted, for the same reason every other refusal in this tree is counted.
    pub refusals: u64,
    /// Listings taken, so a boot log can show the panel is being fed at all.
    pub listings: u64,
}

impl FilePanel {
    /// Build a panel that shows `visible_rows` rows at a time. This is the ONLY allocation the
    /// panel ever performs.
    pub fn new(visible_rows: usize) -> Self {
        FilePanel {
            rows: Vec::with_capacity(ROW_CAP),
            selected: 0,
            scroll: 0,
            visible_rows: visible_rows.max(1),
            free_blocks: 0,
            total_blocks: 0,
            listings_truncated: 0,
            entries_dropped: 0,
            refusals: 0,
            listings: 0,
        }
    }

    /// How many rows the panel is holding.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the panel is holding nothing.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The selected row index, if there is a listing at all.
    pub fn selected(&self) -> Option<usize> {
        if self.rows.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    /// The first row of the visible window.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// The rows currently on screen, in order.
    pub fn visible(&self) -> &[FileRow] {
        let end = (self.scroll + self.visible_rows).min(self.rows.len());
        &self.rows[self.scroll.min(self.rows.len())..end]
    }

    /// The selected row, if any.
    pub fn selected_row(&self) -> Option<&FileRow> {
        self.rows.get(self.selected)
    }

    /// Free and total blocks, as the platform last reported them.
    pub fn space(&self) -> (u32, u32) {
        (self.free_blocks, self.total_blocks)
    }

    /// Take a new listing, REUSING the row storage.
    ///
    /// The selection is preserved BY NAME when the same name is still present, because a refresh
    /// that silently moves the selection onto a different file is how a user deletes the wrong
    /// thing. When the name is gone the selection clamps into range instead.
    pub fn replace_listing<I: Iterator<Item = FileRow>>(
        &mut self,
        entries: I,
        free_blocks: u32,
        total_blocks: u32,
    ) {
        let previous = self.selected_row().map(|r| (r.name, r.name_len));
        self.rows.clear();
        let mut dropped = 0u64;
        for row in entries {
            if self.rows.len() == ROW_CAP {
                dropped += 1;
                continue;
            }
            self.rows.push(row);
        }
        if dropped > 0 {
            self.listings_truncated += 1;
            self.entries_dropped += dropped;
        }
        self.listings += 1;
        self.free_blocks = free_blocks;
        self.total_blocks = total_blocks;
        self.selected = match previous {
            Some((name, len)) => self
                .rows
                .iter()
                .position(|r| r.name_len == len && r.name == name)
                .unwrap_or_else(|| self.selected.min(self.rows.len().saturating_sub(1))),
            None => 0,
        };
        self.clamp_scroll();
    }

    /// Move the selection by one row, reporting what happened.
    pub fn step(&mut self, down: bool) -> PanelAction {
        if self.rows.is_empty() {
            self.refusals += 1;
            return PanelAction::Refused(PanelRefusal::Empty);
        }
        let last = self.rows.len() - 1;
        self.selected = if down {
            if self.selected == last {
                0
            } else {
                self.selected + 1
            }
        } else if self.selected == 0 {
            last
        } else {
            self.selected - 1
        };
        self.clamp_scroll();
        PanelAction::Selected(self.selected)
    }

    /// Jump to the first or last row.
    pub fn jump(&mut self, end: bool) -> PanelAction {
        if self.rows.is_empty() {
            self.refusals += 1;
            return PanelAction::Refused(PanelRefusal::Empty);
        }
        self.selected = if end { self.rows.len() - 1 } else { 0 };
        self.clamp_scroll();
        PanelAction::Selected(self.selected)
    }

    /// Select the row at a panel-relative row coordinate, where row 0 is the first VISIBLE row.
    pub fn select_at_row(&mut self, row: usize) -> PanelAction {
        if self.rows.is_empty() {
            self.refusals += 1;
            return PanelAction::Refused(PanelRefusal::Empty);
        }
        if row >= self.visible_rows {
            self.refusals += 1;
            return PanelAction::Refused(PanelRefusal::OutOfRange);
        }
        let index = self.scroll + row;
        if index >= self.rows.len() {
            self.refusals += 1;
            return PanelAction::Refused(PanelRefusal::OutOfRange);
        }
        self.selected = index;
        PanelAction::Selected(index)
    }

    /// Activate the selection. The panel does not know what opening a file means; it says WHICH.
    pub fn activate(&mut self) -> PanelAction {
        if self.rows.is_empty() {
            self.refusals += 1;
            return PanelAction::Refused(PanelRefusal::Empty);
        }
        PanelAction::Activated(self.selected)
    }

    /// Keep the visible window over the selection, moving it by the least that achieves that.
    fn clamp_scroll(&mut self) {
        if self.rows.is_empty() {
            self.scroll = 0;
            return;
        }
        let max_scroll = self.rows.len().saturating_sub(self.visible_rows);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + self.visible_rows {
            self.scroll = self.selected + 1 - self.visible_rows;
        }
        self.scroll = self.scroll.min(max_scroll);
    }

    /// Paint the panel into a grid: one row per visible entry, the selection marked, then a
    /// footer naming the count and the free space. Writes nothing but cells — the grid decides
    /// what a cell looks like.
    pub fn render(&self, grid: &mut TextGrid) {
        grid.clear();
        if self.rows.is_empty() {
            grid.write(b"  (the namespace is empty)");
            grid.put(b'\n');
        }
        for (n, row) in self.visible().iter().enumerate() {
            let index = self.scroll + n;
            grid.write(if index == self.selected { b"> " } else { b"  " });
            let cols = grid.cols() as usize;
            // A name is clipped to what the row can hold, and the clip is VISIBLE (an ellipsis)
            // rather than a name that silently reads as a different, shorter file.
            let budget = cols.saturating_sub(12);
            let name = row.name();
            if name.len() > budget && budget > 1 {
                grid.write(&name[..budget - 1]);
                grid.put(b'~');
            } else {
                grid.write(name);
            }
            if row.name_truncated {
                grid.put(b'~');
            }
            grid.put(b' ');
            write_u64(grid, row.len);
            grid.put(b'\n');
        }
        grid.write(b"-- ");
        write_u64(grid, self.rows.len() as u64);
        grid.write(b" objects, ");
        write_u64(grid, self.free_blocks as u64);
        grid.put(b'/');
        write_u64(grid, self.total_blocks as u64);
        grid.write(b" blocks free");
        if self.listings_truncated > 0 {
            grid.write(b" (truncated)");
        }
    }
}

/// Write a decimal number into a grid without allocating or formatting machinery.
fn write_u64(grid: &mut TextGrid, mut v: u64) {
    let mut buf = [0u8; 20];
    let mut n = 0;
    if v == 0 {
        grid.put(b'0');
        return;
    }
    while v > 0 && n < buf.len() {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        grid.put(buf[n]);
    }
}

// ---------------------------------------------------------------------------------------------
// The crossing: the console owns the namespace, the desktop owns the view (ADR-137).
// ---------------------------------------------------------------------------------------------

/// How much of an opened object the console prints. A panel that dumps a whole file into a
/// 40-column terminal has destroyed the session the operator was in the middle of, so the preview
/// is bounded here rather than by the operator's patience.
pub const PREVIEW_BYTES: usize = 512;

/// Serve the desktop's file panel from the console's namespace.
///
/// This is the ONLY place the two meet, and it meets them on the console's thread: the shell's
/// loop hands the filesystem out between keystrokes ([`crate::shell::run_loop_serviced`]), so a
/// slow device delays a typist rather than the compositor.
///
/// Returns whether anything was printed, so the caller can re-issue its prompt.
///
/// The phases differ in cost on purpose. [`ServicePhase::Idle`] runs a thousand times a second
/// and does nothing at all unless the operator activated a row, which is a latched value and not
/// a device read. [`ServicePhase::Settled`] runs once per command line, where one directory read
/// is what a human has already paid for by pressing return.
pub fn service_panel<D: BlockDevice>(
    phase: ServicePhase,
    fs: &Filesystem,
    dev: &mut D,
    out: &mut dyn FnMut(&str),
    publish: &mut dyn FnMut(&[FileRow], u32, u32),
    take_activation: &mut dyn FnMut() -> Option<[u8; NAME_CAP]>,
) -> bool {
    let opened = take_activation();
    if phase == ServicePhase::Idle && opened.is_none() {
        return false;
    }
    let printed = match opened {
        Some(name) => open_name(&name, fs, dev, out),
        None => false,
    };
    publish_listing(fs, dev, publish);
    printed
}

/// Read the namespace once and hand the rows over. A device that refuses to be listed leaves the
/// panel showing the LAST listing rather than an empty one: "I cannot read the disk right now" and
/// "you have no files" are different statements and the panel must not confuse them.
fn publish_listing<D: BlockDevice>(
    fs: &Filesystem,
    dev: &D,
    publish: &mut dyn FnMut(&[FileRow], u32, u32),
) {
    let Ok(entries) = fs.list(dev) else {
        return;
    };
    let mut used = 0u32;
    let mut rows: Vec<FileRow> = Vec::with_capacity(entries.len());
    for e in &entries {
        let blocks = e.blocks() as u32;
        used = used.saturating_add(blocks);
        rows.push(FileRow::new(e.name.as_bytes(), e.len as u64, blocks));
    }
    let free = fs.free_blocks(dev).unwrap_or(0) as u32;
    publish(&rows, free, free.saturating_add(used));
}

/// Print the object the operator opened, the way `cat` would, into whatever surfaces the console
/// writes to. Activation is performed HERE and nowhere else: the panel reported a name, the shell
/// owns the namespace, and the compositor never learns that a file exists.
fn open_name<D: BlockDevice>(
    name: &[u8; NAME_CAP],
    fs: &Filesystem,
    dev: &D,
    out: &mut dyn FnMut(&str),
) -> bool {
    let end = name.iter().position(|&b| b == 0).unwrap_or(NAME_CAP);
    let Ok(text) = core::str::from_utf8(&name[..end]) else {
        out("\r\nfiles: that name is not text\r\n");
        return true;
    };
    out("\r\nfiles: ");
    out(text);
    match fs.read(dev, text) {
        Ok(bytes) => {
            out("\r\n");
            print_preview(&bytes, out);
        }
        // The listing the operator clicked can be older than the namespace: a name removed
        // between the click and this read is reported, not invented.
        Err(_) => out(": no longer in the namespace\r\n"),
    }
    true
}

/// Print at most [`PREVIEW_BYTES`] of an object, with anything unprintable shown as a dot. A file
/// panel must be safe to point at a binary: bytes that would move the cursor, change the colour,
/// or reprogram the terminal are shown rather than executed.
fn print_preview(bytes: &[u8], out: &mut dyn FnMut(&str)) {
    let shown = bytes.len().min(PREVIEW_BYTES);
    let mut buf = [0u8; 64];
    let mut n = 0usize;
    let flush = |buf: &[u8], out: &mut dyn FnMut(&str)| {
        if let Ok(s) = core::str::from_utf8(buf) {
            out(s);
        }
    };
    for &b in &bytes[..shown] {
        if n + 2 > buf.len() {
            flush(&buf[..n], out);
            n = 0;
        }
        match b {
            b'\n' => {
                buf[n] = b'\r';
                buf[n + 1] = b'\n';
                n += 2;
            }
            b'\t' | 0x20..=0x7e => {
                buf[n] = b;
                n += 1;
            }
            _ => {
                buf[n] = b'.';
                n += 1;
            }
        }
    }
    flush(&buf[..n], out);
    out("\r\n");
    if shown < bytes.len() {
        out("files: the rest is not shown\r\n");
    }
}

/// The file-panel contract, proved on every CPU at boot.
///
/// Takes the caller's scratch device rather than making its own: the crossing's checks need a real
/// namespace, and on a heap that never frees (ADR-063) a suite that allocates half a megabyte of
/// RAM disk for itself is half a megabyte the machine never gets back.
pub fn filepanel_suite<D: BlockDevice>(
    dev: &mut D,
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }
    let row = |name: &[u8], len: u64| FileRow::new(name, len, len.div_ceil(512) as u32);

    // 1 — an empty panel has no selection, and every action on it is refused BY NAME and counted.
    {
        let mut p = FilePanel::new(4);
        let a = p.step(true);
        let b = p.activate();
        let c = p.jump(true);
        let d = p.select_at_row(0);
        check!(
            p.selected().is_none()
                && a == PanelAction::Refused(PanelRefusal::Empty)
                && b == PanelAction::Refused(PanelRefusal::Empty)
                && c == PanelAction::Refused(PanelRefusal::Empty)
                && d == PanelAction::Refused(PanelRefusal::Empty)
                && p.refusals == 4,
            "filepanel: an empty listing has no selection and refuses every action by name"
        );
    }
    // 2 — the selection never leaves the listing, however far it is stepped.
    {
        let mut p = FilePanel::new(2);
        p.replace_listing(
            [row(b"a", 1), row(b"b", 2), row(b"c", 3)].into_iter(),
            10,
            20,
        );
        let mut ok = true;
        for _ in 0..17 {
            p.step(true);
            ok &= p.selected().is_some_and(|s| s < 3);
        }
        for _ in 0..17 {
            p.step(false);
            ok &= p.selected().is_some_and(|s| s < 3);
        }
        check!(
            ok,
            "filepanel: the selection stays inside the listing however far it is stepped"
        );
    }
    // 3 — the visible window always contains the selection, exactly.
    {
        let mut p = FilePanel::new(3);
        let rows: Vec<FileRow> = (0..9).map(|i| row(&[b'a' + i as u8], i as u64)).collect();
        p.replace_listing(rows.into_iter(), 4, 9);
        let mut ok = true;
        for _ in 0..20 {
            p.step(true);
            let s = p.selected().unwrap();
            ok &= s >= p.scroll() && s < p.scroll() + 3;
        }
        check!(
            ok,
            "filepanel: the visible window always contains the selection"
        );
    }
    // 4 — a listing longer than capacity is truncated and COUNTED, never allocated for.
    {
        let mut p = FilePanel::new(4);
        let rows: Vec<FileRow> = (0..ROW_CAP + 7).map(|i| row(b"x", i as u64)).collect();
        p.replace_listing(rows.into_iter(), 0, 1);
        check!(
            p.len() == ROW_CAP && p.listings_truncated == 1 && p.entries_dropped == 7,
            "filepanel: a listing longer than capacity is truncated and counted"
        );
    }
    // 5 — a name longer than the row is stored truncated and SAYS it was truncated.
    {
        let long = [b'z'; NAME_CAP + 5];
        let r = FileRow::new(&long, 1, 1);
        check!(
            r.name().len() == NAME_CAP && r.name_truncated,
            "filepanel: a name longer than the row is truncated and says so"
        );
    }
    // 6 — refreshing the listing REUSES its storage: the row capacity never moves, so a panel
    //     that refreshes on every disk change cannot grow the never-freeing heap.
    {
        let mut p = FilePanel::new(4);
        p.replace_listing([row(b"a", 1)].into_iter(), 1, 2);
        let cap = p.rows.capacity();
        let ptr = p.rows.as_ptr();
        for i in 0..64u64 {
            let rows: Vec<FileRow> = (0..8).map(|k| row(b"f", i + k)).collect();
            p.replace_listing(rows.into_iter(), 1, 2);
        }
        check!(
            p.rows.capacity() == cap && p.rows.as_ptr() == ptr && p.listings == 65,
            "filepanel: refreshing the listing reuses its storage and never reallocates"
        );
    }
    // 7 — a refresh keeps the selection on the SAME NAME when that name survives. A refresh that
    //     moves the selection onto a different file is how a user deletes the wrong thing.
    {
        let mut p = FilePanel::new(4);
        p.replace_listing(
            [row(b"one", 1), row(b"two", 2), row(b"three", 3)].into_iter(),
            1,
            2,
        );
        p.step(true);
        p.step(true);
        let before = p.selected_row().map(|r| r.name().to_vec());
        p.replace_listing(
            [row(b"zero", 9), row(b"one", 1), row(b"three", 3)].into_iter(),
            1,
            2,
        );
        let after = p.selected_row().map(|r| r.name().to_vec());
        check!(
            before.as_deref() == Some(b"three".as_slice()) && after == before,
            "filepanel: a refresh keeps the selection on the same name when it survives"
        );
    }
    // 8 — a pointer press outside the listing is refused by name rather than selecting the
    //     nearest row: a click on empty space below the last file must not select a file.
    {
        let mut p = FilePanel::new(6);
        p.replace_listing([row(b"a", 1), row(b"b", 2)].into_iter(), 1, 2);
        let before = p.refusals;
        let a = p.select_at_row(4);
        let b = p.select_at_row(99);
        check!(
            a == PanelAction::Refused(PanelRefusal::OutOfRange)
                && b == PanelAction::Refused(PanelRefusal::OutOfRange)
                && p.refusals == before + 2
                && p.selected() == Some(0),
            "filepanel: a press below the last row is refused by name and selects nothing"
        );
    }
    // 9 — the same listing rendered twice is byte-identical, and the render is bounded by the
    //     grid rather than by the listing.
    {
        let mut p = FilePanel::new(3);
        let rows: Vec<FileRow> = (0..20).map(|i| row(b"file", i as u64)).collect();
        p.replace_listing(rows.into_iter(), 5, 40);
        let mut g1 = TextGrid::new(32, 6);
        let mut g2 = TextGrid::new(32, 6);
        p.render(&mut g1);
        p.render(&mut g2);
        let mut same = true;
        for r in 0..6 {
            same &= g1.line(r) == g2.line(r);
        }
        check!(
            same && g1.refused() == 0,
            "filepanel: the same listing renders byte-identically and refuses nothing"
        );
    }
    // 10 — the rendered rows ARE the visible window, in order: what the panel says is on screen
    //      is what the grid shows, or the pointer's row arithmetic is a lie.
    {
        let mut p = FilePanel::new(3);
        let rows: Vec<FileRow> = (0..9)
            .map(|i| row(&[b'a' + i as u8, b'a' + i as u8], i as u64))
            .collect();
        p.replace_listing(rows.into_iter(), 1, 9);
        p.jump(true);
        let mut g = TextGrid::new(32, 5);
        p.render(&mut g);
        let window = p.visible();
        let mut ok = window.len() == 3;
        for (n, r) in window.iter().enumerate() {
            let line = g.line(n as u32);
            ok &= line.len() > 2 && &line[2..4] == r.name();
        }
        check!(
            ok,
            "filepanel: the rendered rows are exactly the visible window, in order"
        );
    }
    // 11 — an idle turn with nothing activated does NOTHING. This runs a thousand times a second
    //      on a live machine: a device read here would put the disk on the display's cadence.
    {
        if Filesystem::format(dev).is_err() {
            return Err((n + 1, "filepanel: format the crossing's device"));
        }
        let Ok(fs) = Filesystem::mount(&mut *dev) else {
            return Err((n + 1, "filepanel: mount the crossing's device"));
        };
        let mut published = 0u32;
        let mut printed_len = 0usize;
        let said = service_panel(
            ServicePhase::Idle,
            &fs,
            &mut *dev,
            &mut |s: &str| printed_len += s.len(),
            &mut |_, _, _| published += 1,
            &mut || None,
        );
        check!(
            !said && published == 0 && printed_len == 0,
            "filepanel: an idle turn with nothing activated neither reads the disk nor prints"
        );
    }
    // 12 — a settle publishes the namespace AS IT IS, including the space accounting the footer
    //      shows. The free/total pair comes from the device, not from a count of what was written.
    {
        if Filesystem::format(dev).is_err() {
            return Err((n + 1, "filepanel: format the crossing's device"));
        }
        let Ok(mut fs) = Filesystem::mount(&mut *dev) else {
            return Err((n + 1, "filepanel: mount the crossing's device"));
        };
        if fs.create(&mut *dev, "notes", b"hello").is_err()
            || fs.create(&mut *dev, "log", b"two blocks").is_err()
        {
            return Err((n + 1, "filepanel: create the crossing's objects"));
        }
        let mut names: Vec<[u8; NAME_CAP]> = Vec::new();
        let mut space = (0u32, 0u32);
        let said = service_panel(
            ServicePhase::Settled,
            &fs,
            &mut *dev,
            &mut |_: &str| {},
            &mut |rows, free, total| {
                names = rows.iter().map(|r| r.name).collect();
                space = (free, total);
            },
            &mut || None,
        );
        let listed: Vec<&[u8]> = names
            .iter()
            .map(|nm| &nm[..nm.iter().position(|&b| b == 0).unwrap_or(NAME_CAP)])
            .collect();
        check!(
            !said
                && listed.len() == 2
                && listed.contains(&&b"notes"[..])
                && listed.contains(&&b"log"[..])
                && space.1 > space.0,
            "filepanel: a settle publishes every live name with the device's own free space"
        );
    }
    // 13 — opening a name PRINTS it and then republishes, and a name that left the namespace
    //      between the click and the read is refused by name rather than printed as garbage.
    {
        if Filesystem::format(dev).is_err() {
            return Err((n + 1, "filepanel: format the crossing's device"));
        }
        let Ok(mut fs) = Filesystem::mount(&mut *dev) else {
            return Err((n + 1, "filepanel: mount the crossing's device"));
        };
        if fs.create(&mut *dev, "notes", b"hello\x1b[2J").is_err() {
            return Err((n + 1, "filepanel: create the crossing's object"));
        }
        let latched = |text: &[u8]| {
            let mut nm = [0u8; NAME_CAP];
            nm[..text.len()].copy_from_slice(text);
            nm
        };
        let mut log = alloc::string::String::new();
        let mut published = 0u32;
        let mut one = Some(latched(b"notes"));
        let said = service_panel(
            ServicePhase::Idle,
            &fs,
            &mut *dev,
            &mut |s: &str| log.push_str(s),
            &mut |_, _, _| published += 1,
            &mut || one.take(),
        );
        // The escape sequence in the object is SHOWN, not executed: a file panel that can be
        // pointed at a binary must not let that binary drive the terminal.
        let opened = said && published == 1 && log.contains("hello") && !log.contains("\x1b");
        let mut gone = Some(latched(b"vanished"));
        let mut log2 = alloc::string::String::new();
        let said2 = service_panel(
            ServicePhase::Idle,
            &fs,
            &mut *dev,
            &mut |s: &str| log2.push_str(s),
            &mut |_, _, _| {},
            &mut || gone.take(),
        );
        check!(
            opened && said2 && log2.contains("no longer in the namespace"),
            "filepanel: opening a row prints its bytes safely and a vanished name is refused"
        );
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_file_panel_invariant() {
        let mut seen = 0;
        let mut dev = crate::storage::MemBlockDevice::new(crate::fs::FILE_DATA_START + 32);
        let n = filepanel_suite(&mut dev, |_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the file-panel suite should hold");
        assert_eq!(n, 13);
        assert_eq!(seen, 13);
    }

    #[test]
    fn a_fresh_panel_holds_nothing_and_selects_nothing() {
        let p = FilePanel::new(4);
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        assert_eq!(p.selected(), None);
        assert_eq!(p.space(), (0, 0));
    }

    #[test]
    fn stepping_wraps_at_both_ends_of_the_listing() {
        let mut p = FilePanel::new(4);
        p.replace_listing(
            [
                FileRow::new(b"a", 1, 1),
                FileRow::new(b"b", 2, 1),
                FileRow::new(b"c", 3, 1),
            ]
            .into_iter(),
            1,
            2,
        );
        assert_eq!(p.step(false), PanelAction::Selected(2));
        assert_eq!(p.step(true), PanelAction::Selected(0));
    }

    #[test]
    fn activating_reports_the_selected_row_without_performing_anything() {
        let mut p = FilePanel::new(4);
        p.replace_listing(
            [FileRow::new(b"one", 5, 1), FileRow::new(b"two", 6, 1)].into_iter(),
            1,
            2,
        );
        p.step(true);
        assert_eq!(p.activate(), PanelAction::Activated(1));
        assert_eq!(p.selected_row().map(|r| r.name()), Some(b"two".as_slice()));
    }

    #[test]
    fn a_press_on_a_visible_row_selects_that_row_and_not_its_neighbour() {
        let mut p = FilePanel::new(2);
        p.replace_listing(
            (0..6)
                .map(|i| FileRow::new(&[b'a' + i as u8], i as u64, 1))
                .collect::<Vec<_>>()
                .into_iter(),
            1,
            2,
        );
        p.jump(true);
        let top = p.scroll();
        assert_eq!(p.select_at_row(0), PanelAction::Selected(top));
        assert_eq!(p.select_at_row(1), PanelAction::Selected(top + 1));
    }

    #[test]
    fn the_footer_reports_the_count_and_the_free_space() {
        let mut p = FilePanel::new(4);
        p.replace_listing(
            [FileRow::new(b"a", 1, 1), FileRow::new(b"b", 2, 1)].into_iter(),
            7,
            19,
        );
        let mut g = TextGrid::new(40, 6);
        p.render(&mut g);
        let footer = g.line(2);
        assert_eq!(footer, b"-- 2 objects, 7/19 blocks free");
    }

    #[test]
    fn an_empty_namespace_says_so_rather_than_rendering_a_blank_window() {
        let p = FilePanel::new(4);
        let mut g = TextGrid::new(40, 4);
        p.render(&mut g);
        assert_eq!(g.line(0), b"  (the namespace is empty)");
    }

    #[test]
    fn a_name_wider_than_the_row_is_clipped_visibly_rather_than_silently() {
        let mut p = FilePanel::new(2);
        p.replace_listing([FileRow::new(b"averylongfilename", 1, 1)].into_iter(), 1, 2);
        let mut g = TextGrid::new(20, 3);
        p.render(&mut g);
        let line = g.line(0);
        assert!(line.ends_with(b"~ 1"), "clip should be visible: {line:?}");
    }
}
