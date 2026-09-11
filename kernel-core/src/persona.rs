//! Desktop shell personas — the arch-independent policy that decides WHERE the desktop's chrome
//! sits, without changing what the chrome IS.
//!
//! Aletheia's window manager (`wm`) owns window authority and the desktop (`desktop`) owns the
//! surfaces. Neither of them should also own an opinion about whether the panel belongs at the top
//! or the bottom of the scanout, whether the launcher sits at the left edge or in the middle, or
//! which side of a title bar carries the window controls. Those are *presentation conventions*, and
//! different users arrive with different ones already in their fingers.
//!
//! This module makes that convention an explicit, pure, allocation-free value:
//!
//! * [`ShellPersona::Aletheia`] — the in-house convention, which is exactly the layout Aletheia
//!   shipped before personas existed, so every pre-existing GUI proof keeps its meaning;
//! * [`ShellPersona::Windows`] — bottom panel, launcher hard left, workspace strip parked on the
//!   right where a Windows user looks for the tray;
//! * [`ShellPersona::Macos`] — top menu bar, application cluster centred like a dock, and window
//!   controls on the LEFT of the title bar;
//! * [`ShellPersona::Gnome`] — top panel, activities-style launcher at the left, workspace strip
//!   next to it, applications centred.
//!
//! Everything here is a total function over [`ChromeMetrics`]: no allocation, no globals, no device
//! access. That is deliberate. The desktop's hit map and its painter must agree exactly, on every
//! target, for every persona; the cheapest way to guarantee that is to derive both from ONE value
//! that can be proved on the host in microseconds and re-proved on real hardware at boot.

/// Which side of a window's title bar carries its minimize/maximize/close controls. Aletheia's
/// painter has always put them on the right; the macOS convention puts them on the left, and a
/// user who has that convention in their fingers reaches left for close no matter what the pixels
/// say. Naming the side makes the painter and the hit map share one answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlSide {
    /// Controls occupy the leading edge of the title bar.
    Left,
    /// Controls occupy the trailing edge of the title bar.
    Right,
}

/// How many personas exist. Cycling is defined over this count so a new persona cannot be added
/// without the traversal, the labels, and the invariant suite all seeing it.
pub const PERSONA_COUNT: usize = 4;

/// Which edge of the scanout the panel occupies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelEdge {
    /// Panel pinned to the top of the scanout (macOS/GNOME convention).
    Top,
    /// Panel pinned to the bottom of the scanout (Aletheia/Windows convention).
    Bottom,
}

/// A desktop shell convention. The value is presentation-only: it never becomes window-manager
/// authority, never gates a capability, and never changes which windows exist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellPersona {
    /// Aletheia's own convention — the pre-persona layout, preserved byte-for-byte. It is the
    /// default because Aletheia should boot as itself, not as an imitation of another system.
    #[default]
    Aletheia,
    /// Familiar to a Windows user: bottom panel, launcher left, workspace strip right.
    Windows,
    /// Familiar to a macOS user: top menu bar, centred dock cluster, left-hand window controls.
    Macos,
    /// Familiar to a Linux/GNOME user: top panel, activities launcher, workspaces beside it.
    Gnome,
}

/// The chrome's fixed measurements, supplied by the desktop that owns the surfaces. Keeping these
/// as an argument rather than a constant lets the same policy serve a different scanout size (or a
/// test) without a second copy of the layout rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChromeMetrics {
    /// Scanout width in pixels.
    pub surface_w: u32,
    /// Scanout height in pixels.
    pub surface_h: u32,
    /// Total panel height in pixels, including its title band.
    pub panel_h: u32,
    /// Width of the launcher affordance in pixels.
    pub launcher_w: u32,
    /// Width of ONE application button in pixels.
    pub button_w: u32,
    /// How many application buttons the panel carries.
    pub button_count: u32,
    /// Width of ONE workspace button in pixels.
    pub workspace_w: u32,
    /// How many workspace buttons the panel carries.
    pub workspace_count: u32,
}

impl ChromeMetrics {
    /// Total width of the application cluster.
    pub const fn buttons_w(&self) -> u32 {
        self.button_w * self.button_count
    }

    /// Total width of the workspace cluster.
    pub const fn workspaces_w(&self) -> u32 {
        self.workspace_w * self.workspace_count
    }

    /// Total width every cluster needs if they were packed end to end with no gap. A persona whose
    /// placement cannot fit this into `surface_w` has to fall back to packed order.
    pub const fn packed_w(&self) -> u32 {
        self.launcher_w + self.buttons_w() + self.workspaces_w()
    }
}

/// The resolved geometry: where each cluster starts, where the panel sits, and which side of a
/// title bar carries the window controls. Every field is absolute, in scanout pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChromeLayout {
    /// Which edge the panel occupies.
    pub edge: PanelEdge,
    /// Panel origin on the y axis, in scanout pixels.
    pub panel_y: i32,
    /// Launcher origin on the x axis, relative to the panel's left edge.
    pub launcher_x: u32,
    /// Application cluster origin on the x axis.
    pub buttons_x: u32,
    /// Workspace cluster origin on the x axis.
    pub workspaces_x: u32,
    /// Which side of a window's title bar carries its controls.
    pub controls: ControlSide,
}

/// Place `width` in the middle of `space`, clamped to zero when it cannot fit. Centring is a
/// presentation decision that must never produce a negative origin, because the hit map is
/// unsigned: an unfittable cluster degrades to the left edge rather than to an unreachable one.
const fn centred(space: u32, width: u32) -> u32 {
    if width >= space {
        0
    } else {
        (space - width) / 2
    }
}

/// Centre `width` inside the half-open span `[from, to)`, degrading to `from` when the span is
/// too tight. Centring a cluster against the whole scanout is wrong once another cluster is
/// already parked: the visual centre the user reads is the centre of the space that is left.
const fn centred_in_span(from: u32, to: u32, width: u32) -> u32 {
    if to <= from {
        return from;
    }
    from + centred(to - from, width)
}

/// Slide `start` left until `start + width` fits inside `space`. Used for the right-parked clusters
/// so a narrow scanout loses the right margin before it loses a reachable button.
const fn fit_right(space: u32, width: u32) -> u32 {
    space.saturating_sub(width)
}

impl ShellPersona {
    /// Every persona, in traversal order. The array — not a `match` arm count — is what makes
    /// [`PERSONA_COUNT`] and the cycle agree.
    pub const ALL: [ShellPersona; PERSONA_COUNT] = [
        ShellPersona::Aletheia,
        ShellPersona::Windows,
        ShellPersona::Macos,
        ShellPersona::Gnome,
    ];

    /// A short, stable label. It is part of the boot evidence, so it is ASCII, lower case, and
    /// never changes shape once a release has quoted it.
    pub const fn label(self) -> &'static str {
        match self {
            ShellPersona::Aletheia => "aletheia",
            ShellPersona::Windows => "windows",
            ShellPersona::Macos => "macos",
            ShellPersona::Gnome => "gnome",
        }
    }

    /// The persona's position in [`ShellPersona::ALL`].
    pub const fn index(self) -> usize {
        match self {
            ShellPersona::Aletheia => 0,
            ShellPersona::Windows => 1,
            ShellPersona::Macos => 2,
            ShellPersona::Gnome => 3,
        }
    }

    /// The next persona in traversal order, wrapping. Cycling is total: there is no persona from
    /// which the user cannot reach every other one with repeated presses of one key.
    pub const fn next(self) -> ShellPersona {
        ShellPersona::ALL[(self.index() + 1) % PERSONA_COUNT]
    }

    /// The previous persona in traversal order, wrapping.
    pub const fn previous(self) -> ShellPersona {
        ShellPersona::ALL[(self.index() + PERSONA_COUNT - 1) % PERSONA_COUNT]
    }

    /// Which edge this persona pins the panel to.
    pub const fn edge(self) -> PanelEdge {
        match self {
            ShellPersona::Aletheia | ShellPersona::Windows => PanelEdge::Bottom,
            ShellPersona::Macos | ShellPersona::Gnome => PanelEdge::Top,
        }
    }

    /// Which side of a title bar this persona puts the window controls on.
    pub const fn controls(self) -> ControlSide {
        match self {
            ShellPersona::Macos => ControlSide::Left,
            _ => ControlSide::Right,
        }
    }

    /// Resolve the full chrome geometry for this persona at these metrics.
    ///
    /// The three clusters are placed so that they never overlap, always start inside the surface,
    /// and always end inside the surface. When the surface is too narrow for a persona's preferred
    /// arrangement, the layout degrades to packed left-to-right order rather than to an
    /// unreachable affordance — a cramped panel is a presentation problem, an unclickable button is
    /// a correctness one.
    pub fn layout(self, m: ChromeMetrics) -> ChromeLayout {
        let panel_y = match self.edge() {
            PanelEdge::Top => 0,
            PanelEdge::Bottom => m.surface_h as i32 - m.panel_h as i32,
        };
        let packed = ChromeLayout {
            edge: self.edge(),
            panel_y,
            launcher_x: 0,
            buttons_x: m.launcher_w,
            workspaces_x: m.launcher_w + m.buttons_w(),
            controls: self.controls(),
        };
        if m.packed_w() > m.surface_w {
            return packed;
        }
        let candidate = match self {
            // The in-house convention IS the packed order: launcher, applications, workspaces.
            ShellPersona::Aletheia => packed,
            // Windows parks the workspace strip on the right, where the tray lives, and keeps the
            // applications immediately beside the launcher.
            ShellPersona::Windows => ChromeLayout {
                workspaces_x: fit_right(m.surface_w, m.workspaces_w()),
                ..packed
            },
            // macOS centres the application cluster like a dock and parks the workspace strip
            // right, leaving the launcher as the leading menu.
            ShellPersona::Macos => {
                let workspaces_x = fit_right(m.surface_w, m.workspaces_w());
                ChromeLayout {
                    workspaces_x,
                    buttons_x: centred_in_span(m.launcher_w, workspaces_x, m.buttons_w()),
                    ..packed
                }
            }
            // GNOME keeps the workspace strip next to the activities launcher and centres the
            // application cluster in whatever space remains to its right.
            ShellPersona::Gnome => {
                let workspaces_x = m.launcher_w;
                let occupied = workspaces_x + m.workspaces_w();
                ChromeLayout {
                    workspaces_x,
                    buttons_x: centred_in_span(occupied, m.surface_w, m.buttons_w()),
                    ..packed
                }
            }
        };
        // A preferred arrangement is only honoured when it is actually reachable. Pushing a
        // cluster right to satisfy a convention can run it off the scanout on a narrow surface, so
        // the packed order is the fallback: every affordance stays clickable, the convention is
        // what gets dropped.
        if candidate.fits_within(m) && !candidate.clusters_overlap(m) {
            candidate
        } else {
            packed
        }
    }
}

/// Which cluster, if any, a panel-relative x coordinate lands in. The desktop turns this into its
/// own presentation ids; keeping the geometric question separate from the id assignment is what
/// lets the hit map be proved without a compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeHit {
    /// The launcher affordance.
    Launcher,
    /// Application button `n`, zero-based.
    Button(u32),
    /// Workspace `n`, ONE-based, matching the user-facing workspace numbering.
    Workspace(u8),
    /// Ordinary panel chrome — no affordance.
    None,
}

impl ChromeLayout {
    /// Resolve a panel-relative x coordinate to the affordance under it.
    pub fn hit(&self, m: ChromeMetrics, x: u32) -> ChromeHit {
        if x < m.launcher_w.saturating_add(self.launcher_x) && x >= self.launcher_x {
            return ChromeHit::Launcher;
        }
        if m.button_w > 0 && x >= self.buttons_x && x < self.buttons_x + m.buttons_w() {
            return ChromeHit::Button((x - self.buttons_x) / m.button_w);
        }
        if m.workspace_w > 0 && x >= self.workspaces_x && x < self.workspaces_x + m.workspaces_w() {
            return ChromeHit::Workspace(((x - self.workspaces_x) / m.workspace_w + 1) as u8);
        }
        ChromeHit::None
    }

    /// Whether any two clusters overlap. A true here is a layout bug: two affordances would claim
    /// the same pixel and the hit map would silently prefer one of them.
    pub fn clusters_overlap(&self, m: ChromeMetrics) -> bool {
        let launcher = (self.launcher_x, self.launcher_x + m.launcher_w);
        let buttons = (self.buttons_x, self.buttons_x + m.buttons_w());
        let workspaces = (self.workspaces_x, self.workspaces_x + m.workspaces_w());
        overlaps(launcher, buttons)
            || overlaps(launcher, workspaces)
            || overlaps(buttons, workspaces)
    }

    /// Whether every cluster ends inside a surface `surface_w` wide.
    pub fn fits_within(&self, m: ChromeMetrics) -> bool {
        self.launcher_x + m.launcher_w <= m.surface_w
            && self.buttons_x + m.buttons_w() <= m.surface_w
            && self.workspaces_x + m.workspaces_w() <= m.surface_w
    }
}

const fn overlaps(a: (u32, u32), b: (u32, u32)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The live desktop's chrome measurements, so the boot proof and the host proof reason about the
/// SAME geometry the machine actually paints rather than a convenient one. Kept here, beside the
/// policy, because a suite that invents its own metrics proves nothing about the desktop.
pub const LIVE_CHROME: ChromeMetrics = ChromeMetrics {
    surface_w: 640,
    surface_h: 240,
    panel_h: 2 * 8 + 10,
    launcher_w: 8 * 8,
    button_w: 14 * 8,
    button_count: 3,
    workspace_w: 7 * 8,
    workspace_count: 4,
};

/// The shell-persona contract, proved on every CPU at boot.
///
/// A persona is presentation, so the invariants are about REACHABILITY rather than taste: every
/// affordance stays inside the scanout, no two affordances claim one pixel, every pixel of a
/// cluster resolves to the affordance that owns it, the cycle is total, and the in-house persona
/// still reproduces the layout Aletheia shipped before personas existed.
pub fn persona_suite(
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
    let m = LIVE_CHROME;

    // 1 — the in-house persona is the historic packed layout, byte for byte. Every GUI proof
    //     recorded before personas existed was recorded against THESE origins.
    {
        let l = ShellPersona::Aletheia.layout(m);
        check!(
            l.launcher_x == 0
                && l.buttons_x == m.launcher_w
                && l.workspaces_x == m.launcher_w + m.buttons_w()
                && l.edge == PanelEdge::Bottom,
            "persona: the in-house convention reproduces the historic packed panel layout"
        );
    }
    // 2 — no persona paints an affordance past the right edge. This is the invariant the old
    //     fixed 18-cell button width violated: workspaces 3 and 4 ended at 720px on a 640px
    //     scanout and could not be clicked at all.
    {
        let mut ok = true;
        for persona in ShellPersona::ALL {
            ok &= persona.layout(m).fits_within(m);
        }
        check!(
            ok,
            "persona: every persona keeps every affordance inside the scanout"
        );
    }
    // 3 — no two clusters claim the same pixel under any persona.
    {
        let mut ok = true;
        for persona in ShellPersona::ALL {
            ok &= !persona.layout(m).clusters_overlap(m);
        }
        check!(
            ok,
            "persona: no persona lets two affordances claim the same pixel"
        );
    }
    // 4 — every pixel of every cluster resolves to the affordance that owns it, for every
    //     persona. The painter walks these same origins, so agreement here is agreement there.
    {
        let mut ok = true;
        for persona in ShellPersona::ALL {
            let l = persona.layout(m);
            ok &= l.hit(m, l.launcher_x) == ChromeHit::Launcher;
            ok &= l.hit(m, l.launcher_x + m.launcher_w - 1) == ChromeHit::Launcher;
            for b in 0..m.button_count {
                let x = l.buttons_x + b * m.button_w;
                ok &= l.hit(m, x) == ChromeHit::Button(b);
                ok &= l.hit(m, x + m.button_w - 1) == ChromeHit::Button(b);
            }
            for w in 0..m.workspace_count {
                let x = l.workspaces_x + w * m.workspace_w;
                ok &= l.hit(m, x) == ChromeHit::Workspace((w + 1) as u8);
                ok &= l.hit(m, x + m.workspace_w - 1) == ChromeHit::Workspace((w + 1) as u8);
            }
        }
        check!(
            ok,
            "persona: every cluster pixel resolves to the affordance that owns it"
        );
    }
    // 5 — the panel is pinned to a real edge, and the edge is the persona's declared one.
    {
        let mut ok = true;
        for persona in ShellPersona::ALL {
            let l = persona.layout(m);
            ok &= match persona.edge() {
                PanelEdge::Top => l.panel_y == 0,
                PanelEdge::Bottom => l.panel_y == m.surface_h as i32 - m.panel_h as i32,
            };
            ok &= l.edge == persona.edge();
        }
        check!(
            ok,
            "persona: the panel is pinned to the edge the persona declares"
        );
    }
    // 6 — cycling is total: from any persona, repeated presses reach every other one and return.
    {
        let mut ok = true;
        for start in ShellPersona::ALL {
            let mut seen = [false; PERSONA_COUNT];
            let mut cur = start;
            for _ in 0..PERSONA_COUNT {
                seen[cur.index()] = true;
                cur = cur.next();
            }
            ok &= cur == start && seen.iter().all(|s| *s);
        }
        check!(
            ok,
            "persona: cycling forward reaches every persona and returns to the start"
        );
    }
    // 7 — the macOS convention, and only it, moves the window controls to the leading edge.
    {
        let mut ok = true;
        for persona in ShellPersona::ALL {
            let expected = if matches!(persona, ShellPersona::Macos) {
                ControlSide::Left
            } else {
                ControlSide::Right
            };
            ok &= persona.controls() == expected;
        }
        check!(
            ok,
            "persona: only the macOS convention puts the window controls on the leading edge"
        );
    }
    // 8 — a scanout too narrow for a convention degrades to packed order rather than to an
    //     unreachable affordance. Presentation is what gets dropped; reachability never is.
    {
        let narrow = ChromeMetrics {
            surface_w: 200,
            ..m
        };
        let mut ok = true;
        for persona in ShellPersona::ALL {
            let l = persona.layout(narrow);
            ok &= l.launcher_x == 0
                && l.buttons_x == narrow.launcher_w
                && l.workspaces_x == narrow.launcher_w + narrow.buttons_w();
        }
        check!(
            ok,
            "persona: a scanout too narrow for a convention degrades to packed order"
        );
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A roomy scanout: wide enough that every persona's PREFERRED arrangement fits, so these
    /// tests exercise the placement rules themselves rather than the packed fallback.
    const LIVE: ChromeMetrics = ChromeMetrics {
        surface_w: 800,
        surface_h: 600,
        panel_h: 26,
        launcher_w: 64,
        button_w: 144,
        button_count: 3,
        workspace_w: 56,
        workspace_count: 4,
    };

    #[test]
    fn the_boot_suite_proves_every_persona_invariant_on_the_live_chrome() {
        let mut seen = 0;
        let n = persona_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the persona suite should hold on the live chrome");
        assert_eq!(n, 8);
        assert_eq!(seen, 8, "every invariant should be reported exactly once");
    }

    #[test]
    fn the_live_chrome_is_wide_enough_for_all_four_workspace_buttons() {
        for persona in ShellPersona::ALL {
            let layout = persona.layout(LIVE_CHROME);
            assert!(layout.fits_within(LIVE_CHROME), "{}", persona.label());
        }
    }

    #[test]
    fn the_in_house_persona_reproduces_the_packed_pre_persona_layout() {
        let layout = ShellPersona::Aletheia.layout(LIVE);
        assert_eq!(layout.launcher_x, 0);
        assert_eq!(layout.buttons_x, LIVE.launcher_w);
        assert_eq!(layout.workspaces_x, LIVE.launcher_w + LIVE.buttons_w());
        assert_eq!(layout.edge, PanelEdge::Bottom);
        assert_eq!(layout.panel_y, LIVE.surface_h as i32 - LIVE.panel_h as i32);
        assert_eq!(layout.controls, ControlSide::Right);
    }

    #[test]
    fn bottom_edge_personas_pin_the_panel_to_the_last_panel_height_of_the_scanout() {
        for persona in [ShellPersona::Aletheia, ShellPersona::Windows] {
            let layout = persona.layout(LIVE);
            assert_eq!(layout.edge, PanelEdge::Bottom, "{}", persona.label());
            assert_eq!(layout.panel_y, 574, "{}", persona.label());
        }
    }

    #[test]
    fn top_edge_personas_pin_the_panel_to_the_origin() {
        for persona in [ShellPersona::Macos, ShellPersona::Gnome] {
            let layout = persona.layout(LIVE);
            assert_eq!(layout.edge, PanelEdge::Top, "{}", persona.label());
            assert_eq!(layout.panel_y, 0, "{}", persona.label());
        }
    }

    #[test]
    fn only_the_macos_persona_moves_the_window_controls_to_the_left() {
        for persona in ShellPersona::ALL {
            let expected = if persona == ShellPersona::Macos {
                ControlSide::Left
            } else {
                ControlSide::Right
            };
            assert_eq!(persona.controls(), expected, "{}", persona.label());
        }
    }

    #[test]
    fn the_windows_persona_parks_the_workspace_strip_against_the_right_edge() {
        let layout = ShellPersona::Windows.layout(LIVE);
        assert_eq!(layout.launcher_x, 0);
        assert_eq!(layout.buttons_x, LIVE.launcher_w);
        assert_eq!(
            layout.workspaces_x + LIVE.workspaces_w(),
            LIVE.surface_w,
            "the workspace strip should end exactly at the right edge"
        );
    }

    #[test]
    fn the_macos_persona_centres_the_dock_in_the_space_the_other_clusters_leave() {
        let layout = ShellPersona::Macos.layout(LIVE);
        assert_eq!(
            layout.workspaces_x + LIVE.workspaces_w(),
            LIVE.surface_w,
            "the workspace strip should end exactly at the right edge"
        );
        let lead = layout.buttons_x - LIVE.launcher_w;
        let trail = layout.workspaces_x - (layout.buttons_x + LIVE.buttons_w());
        assert_eq!(
            lead, trail,
            "the dock should sit midway between its neighbours"
        );
    }

    #[test]
    fn the_gnome_persona_places_workspaces_immediately_after_the_activities_launcher() {
        let layout = ShellPersona::Gnome.layout(LIVE);
        assert_eq!(layout.workspaces_x, LIVE.launcher_w);
        assert!(layout.buttons_x >= LIVE.launcher_w + LIVE.workspaces_w());
    }

    #[test]
    fn no_persona_lets_two_clusters_claim_the_same_pixel() {
        for persona in ShellPersona::ALL {
            let layout = persona.layout(LIVE);
            assert!(
                !layout.clusters_overlap(LIVE),
                "{} overlaps: {layout:?}",
                persona.label()
            );
        }
    }

    #[test]
    fn no_persona_places_an_affordance_past_the_right_edge() {
        for persona in ShellPersona::ALL {
            let layout = persona.layout(LIVE);
            assert!(layout.fits_within(LIVE), "{} overflows", persona.label());
        }
    }

    #[test]
    fn a_surface_too_narrow_for_the_preferred_arrangement_degrades_to_packed_order() {
        let narrow = ChromeMetrics {
            surface_w: 200,
            ..LIVE
        };
        for persona in ShellPersona::ALL {
            let layout = persona.layout(narrow);
            assert_eq!(layout.launcher_x, 0, "{}", persona.label());
            assert_eq!(layout.buttons_x, narrow.launcher_w, "{}", persona.label());
            assert_eq!(
                layout.workspaces_x,
                narrow.launcher_w + narrow.buttons_w(),
                "{}",
                persona.label()
            );
        }
    }

    #[test]
    fn every_cluster_pixel_resolves_to_the_affordance_that_owns_it() {
        for persona in ShellPersona::ALL {
            let layout = persona.layout(LIVE);
            assert_eq!(layout.hit(LIVE, layout.launcher_x), ChromeHit::Launcher);
            assert_eq!(
                layout.hit(LIVE, layout.launcher_x + LIVE.launcher_w - 1),
                ChromeHit::Launcher,
                "{}",
                persona.label()
            );
            for n in 0..LIVE.button_count {
                let x = layout.buttons_x + n * LIVE.button_w + LIVE.button_w / 2;
                assert_eq!(
                    layout.hit(LIVE, x),
                    ChromeHit::Button(n),
                    "{}",
                    persona.label()
                );
            }
            for n in 0..LIVE.workspace_count {
                let x = layout.workspaces_x + n * LIVE.workspace_w + LIVE.workspace_w / 2;
                assert_eq!(
                    layout.hit(LIVE, x),
                    ChromeHit::Workspace((n + 1) as u8),
                    "{}",
                    persona.label()
                );
            }
        }
    }

    #[test]
    fn a_pixel_in_no_cluster_is_ordinary_panel_chrome() {
        let layout = ShellPersona::Windows.layout(LIVE);
        let gap = layout.buttons_x + LIVE.buttons_w();
        assert!(gap < layout.workspaces_x, "this persona should leave a gap");
        assert_eq!(layout.hit(LIVE, gap), ChromeHit::None);
    }

    #[test]
    fn cycling_forward_through_every_persona_returns_to_the_start() {
        let mut persona = ShellPersona::Aletheia;
        for _ in 0..PERSONA_COUNT {
            persona = persona.next();
        }
        assert_eq!(persona, ShellPersona::Aletheia);
    }

    #[test]
    fn cycling_backward_is_the_exact_inverse_of_cycling_forward() {
        for persona in ShellPersona::ALL {
            assert_eq!(persona.next().previous(), persona, "{}", persona.label());
            assert_eq!(persona.previous().next(), persona, "{}", persona.label());
        }
    }

    #[test]
    fn every_persona_has_a_distinct_label_and_a_distinct_index() {
        for (i, persona) in ShellPersona::ALL.iter().enumerate() {
            assert_eq!(persona.index(), i, "{}", persona.label());
            for other in ShellPersona::ALL.iter().skip(i + 1) {
                assert_ne!(persona.label(), other.label());
            }
        }
    }

    #[test]
    fn the_default_persona_is_the_in_house_one() {
        assert_eq!(ShellPersona::default(), ShellPersona::Aletheia);
    }
}
