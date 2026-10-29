//! Recognition of a bare Win/Super key tap (TASK-S06D).
//!
//! Pure state machine: no Win32 calls. The keyboard hook feeds it every
//! physical key event (Windows-injected input never reaches it) and acts on
//! the outcome. It never asks for a key to be consumed: every Win event and
//! every other key always reaches Windows, so ordinary Win shortcuts keep
//! their native behavior.
//!
//! ```text
//!            Win down, nothing else down
//!   Idle ───────────────────────────────▶ SuperPending
//!    ▲  ▲                                   │      │
//!    │  │ last Win side up                  │      │ any other key down/up,
//!    │  └───────────── (TapCommitted) ◀─────┘      │ or the other Win side
//!    │                                             ▼
//!    │  last Win side up                     ChordPassthrough
//!    └─────────────────────────────────────────────┘
//! ```
//!
//! A Win down while another key is already down goes straight to
//! `ChordPassthrough`. `TapCommitted` is not stored: it is the outcome
//! returned for the release that ends a `SuperPending` hold.
//!
//! ## Physical sync: when the state may be trusted at all
//!
//! Independently of the phase, the machine is `Trusted` or in `Probation`.
//! In `Probation` no Win hold can become `SuperPending`, so nothing is owned
//! and every gesture is left to Windows.
//!
//! ```text
//!   Probation ──physical key-up leaving nothing believed down, and no
//!       ▲        other key down per Windows (veto)──────────────▶ Trusted
//!       └──── reset() / start / any unpaired physical key-up ◀─────┘
//! ```
//!
//! Probation is entered whenever the believed state may be incomplete:
//!
//! - at start and after `reset` (lock/unlock, secure desktop, elevated
//!   focus): a key held across it is invisible to the hook until it repeats
//!   or goes up;
//! - on any unpaired physical key-up: direct evidence that a key-down was
//!   missed. During `SuperPending` it also makes the hold a chord.
//!
//! Probation ends only at a physical key-up (an event, so injected input,
//! which never reaches this machine, cannot end it) that leaves no key or
//! Win side believed down, and only if `KeyStateProbe` reports no other key
//! down. The probe can only extend probation: it never ends it on its own,
//! never starts or ends a tap and never grants ownership. A gesture that
//! might contain a key held across a reset is therefore never owned.
//!
//! The probe is consulted only at that boundary, never while trusted. It is
//! `GetAsyncKeyState`, which also reflects injected input; the consequences
//! are bounded: an injected key-down only extends probation (fail-open), and
//! an injected key-up of exactly the key held across a reset hides it from
//! the veto, in which case Windows itself also treats that key as up.

/// Left Windows key.
pub const VK_LWIN: u8 = 0x5B;
/// Right Windows key.
pub const VK_RWIN: u8 = 0x5C;
/// The unassigned key injected as the Start mask.
const VK_MASK: u8 = 0xE8;

/// Asks Windows whether a key is down right now (`GetAsyncKeyState` in
/// production). Inside a `WH_KEYBOARD_LL` callback it reports the state before
/// the event being handled, so the key in flight still reads down at its own
/// key-up. Used only as a veto at the end of probation.
pub trait KeyStateProbe {
    fn is_down(&self, vk: u8) -> bool;
}

/// Keys the end-of-probation veto ignores for an event of key `vk`: the key in
/// flight and its generic Shift/Ctrl/Alt alias (both still read down at its
/// own key-up), mouse buttons, the Start mask, and invalid codes.
fn veto_ignores(vk: u8, candidate: u8) -> bool {
    let alias = match vk {
        0xA0 | 0xA1 => 0x10,
        0xA2 | 0xA3 => 0x11,
        0xA4 | 0xA5 => 0x12,
        _ => vk,
    };
    candidate == vk
        || candidate == alias
        || matches!(candidate, 0x00 | 0x01 | 0x02 | 0x04..=0x06 | 0xFF)
        || candidate == VK_MASK
}

/// Whether the machine's belief about physical key state may be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalSync {
    Trusted,
    /// The believed state may miss a held key: nothing is owned.
    Probation,
}

/// Physical Win keys believed down, one bit per side.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WinSides(u8);

impl WinSides {
    fn bit(vk: u8) -> u8 {
        if vk == VK_LWIN {
            0b01
        } else {
            0b10
        }
    }

    fn contains(self, vk: u8) -> bool {
        self.0 & Self::bit(vk) != 0
    }

    fn insert(&mut self, vk: u8) {
        self.0 |= Self::bit(vk);
    }

    fn remove(&mut self, vk: u8) {
        self.0 &= !Self::bit(vk);
    }

    fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Non-Win keys believed physically down, one bit per virtual-key code.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct KeysDown([u64; 4]);

impl KeysDown {
    fn slot(vk: u8) -> (usize, u64) {
        ((vk >> 6) as usize, 1 << (vk & 63))
    }

    /// Records a key's state; returns whether it was already believed down.
    fn set(&mut self, vk: u8, is_down: bool) -> bool {
        let (word, bit) = Self::slot(vk);
        let was_down = self.0[word] & bit != 0;
        if is_down {
            self.0[word] |= bit;
        } else {
            self.0[word] &= !bit;
        }
        was_down
    }

    fn is_empty(&self) -> bool {
        self.0.iter().all(|word| *word == 0)
    }
}

impl std::fmt::Debug for KeysDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut list = f.debug_list();
        for vk in 0..=255u8 {
            let (word, bit) = Self::slot(vk);
            if self.0[word] & bit != 0 {
                list.entry(&format_args!("{vk:#04x}"));
            }
        }
        list.finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperPhase {
    Idle,
    /// One Win key held and nothing else touched so far.
    SuperPending {
        sides: WinSides,
        /// This hold started within the double-tap window of a delivered tap.
        is_second: bool,
    },
    /// Another key (or the other Win side) took part in this hold. Windows
    /// owns it; its release commits nothing.
    ChordPassthrough {
        sides: WinSides,
    },
}

/// A clean, trusted tap, reported on the key-up that ended it. Both kinds are
/// owned: Start is suppressed and the overview input is delivered; what a
/// second tap means is up to the overview controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapOutcome {
    FirstTap {
        time: u32,
    },
    /// Started within the double-tap window of a delivered first tap.
    SecondTap {
        time: u32,
    },
}

#[derive(Debug)]
pub struct SuperTap {
    phase: SuperPhase,
    sync: PhysicalSync,
    keys: KeysDown,
    /// `KBDLLHOOKSTRUCT.time` of the release of the last delivered first tap,
    /// while the next tap may still be its second tap.
    pair_anchor: Option<u32>,
    /// 0 disables double tap.
    double_tap_ms: u32,
}

impl SuperTap {
    pub fn new(double_tap_ms: u32) -> Self {
        Self {
            phase: SuperPhase::Idle,
            // Nothing is known about keys held when the hook starts.
            sync: PhysicalSync::Probation,
            keys: KeysDown::default(),
            pair_anchor: None,
            double_tap_ms,
        }
    }

    /// Forgets all physical state and enters probation. Called when key-ups
    /// may have been lost (session lock/unlock, input desktop switch, focus
    /// moving to an elevated window).
    pub fn reset(&mut self) {
        *self = Self::new(self.double_tap_ms);
    }

    /// Opens the double-tap window after a first tap's overview input was
    /// delivered. Taps that were not delivered never pair, and a second tap
    /// never opens a window for a third.
    pub fn tap_delivered(&mut self, time: u32) {
        if self.double_tap_ms > 0 {
            self.pair_anchor = Some(time);
        }
    }

    /// Handles one physical key event. `time` is `KBDLLHOOKSTRUCT.time`.
    pub fn on_key(
        &mut self,
        vk: u8,
        is_pressed: bool,
        time: u32,
        probe: &impl KeyStateProbe,
    ) -> Option<TapOutcome> {
        let outcome = if vk == VK_LWIN || vk == VK_RWIN {
            if is_pressed {
                self.on_win_down(vk, time);
                None
            } else {
                match self.on_win_up(vk, time) {
                    WinUp::Unpaired => {
                        self.enter_probation();
                        return None;
                    }
                    WinUp::Paired(outcome) => outcome,
                }
            }
        } else {
            let was_down = self.keys.set(vk, is_pressed);
            if !is_pressed && !was_down {
                // A physical release without its press: a key-down was missed
                // (e.g. it was pressed on the secure desktop or in an elevated
                // window), so this key may have been held all along.
                self.enter_probation();
                return None;
            }
            self.pair_anchor = None;
            if let SuperPhase::SuperPending { sides, .. } = self.phase {
                self.phase = SuperPhase::ChordPassthrough { sides };
            }
            None
        };
        if !is_pressed {
            self.try_end_probation(vk, probe);
        }
        outcome
    }

    /// Any doubt about the believed state: own nothing until it is resolved.
    /// A pending tap becomes a chord, so it can no longer commit.
    fn enter_probation(&mut self) {
        self.sync = PhysicalSync::Probation;
        self.pair_anchor = None;
        if let SuperPhase::SuperPending { sides, .. } = self.phase {
            self.phase = SuperPhase::ChordPassthrough { sides };
        }
    }

    /// At the physical key-up of `vk`: ends probation if nothing is believed
    /// down any more and Windows reports no other key down either.
    fn try_end_probation(&mut self, vk: u8, probe: &impl KeyStateProbe) {
        if self.sync == PhysicalSync::Trusted
            || self.phase != SuperPhase::Idle
            || !self.keys.is_empty()
        {
            return;
        }
        let held =
            (0..=255u8).any(|candidate| !veto_ignores(vk, candidate) && probe.is_down(candidate));
        if !held {
            self.sync = PhysicalSync::Trusted;
        }
    }

    fn on_win_down(&mut self, vk: u8, time: u32) {
        let mut sides = match self.phase {
            SuperPhase::Idle => WinSides::default(),
            SuperPhase::SuperPending { sides, .. } | SuperPhase::ChordPassthrough { sides } => {
                sides
            }
        };
        if sides.contains(vk) {
            // Auto-repeat (or a press whose earlier key-up was lost): no new
            // press, so a tap can neither start nor be split here.
            return;
        }
        if !sides.is_empty() {
            // The other Win side is down too.
            sides.insert(vk);
            self.pair_anchor = None;
            self.phase = SuperPhase::ChordPassthrough { sides };
            return;
        }

        sides.insert(vk);
        let is_second = self
            .pair_anchor
            .take()
            .is_some_and(|anchor| time.wrapping_sub(anchor) <= self.double_tap_ms);
        self.phase = if self.keys.is_empty() && self.sync == PhysicalSync::Trusted {
            SuperPhase::SuperPending { sides, is_second }
        } else {
            // Another key is down, or a held key may be unknown: native.
            SuperPhase::ChordPassthrough { sides }
        };
    }

    fn on_win_up(&mut self, vk: u8, time: u32) -> WinUp {
        let (mut sides, tap) = match self.phase {
            SuperPhase::Idle => return WinUp::Unpaired,
            SuperPhase::SuperPending { sides, is_second } => (sides, Some(is_second)),
            SuperPhase::ChordPassthrough { sides } => (sides, None),
        };
        if !sides.contains(vk) {
            // Release of a side never seen going down.
            return WinUp::Unpaired;
        }
        sides.remove(vk);
        if !sides.is_empty() {
            self.phase = SuperPhase::ChordPassthrough { sides };
            return WinUp::Paired(None);
        }
        self.phase = SuperPhase::Idle;
        WinUp::Paired(tap.map(|is_second| match is_second {
            true => TapOutcome::SecondTap { time },
            false => TapOutcome::FirstTap { time },
        }))
    }

    #[cfg(test)]
    pub fn phase(&self) -> SuperPhase {
        self.phase
    }

    #[cfg(test)]
    pub fn sync(&self) -> PhysicalSync {
        self.sync
    }
}

/// What a Win key-up was, for `on_key`.
enum WinUp {
    /// Its side was not believed down: a key-down was missed.
    Unpaired,
    Paired(Option<TapOutcome>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const SHIFT: u8 = 0xA0;
    const CTRL: u8 = 0xA2;
    const L: u8 = 0x4C;
    const E: u8 = 0x45;
    /// An unassigned key used to end probation in tests.
    const SYNC: u8 = 0x87;

    /// The physical truth, as `GetAsyncKeyState` reports it to a hook: updated
    /// after the hook has seen an event, or without any event for keys held
    /// or released while the hook could not see them.
    struct FakeProbe(RefCell<[bool; 256]>);

    impl Default for FakeProbe {
        fn default() -> Self {
            Self(RefCell::new([false; 256]))
        }
    }

    impl KeyStateProbe for FakeProbe {
        fn is_down(&self, vk: u8) -> bool {
            let keys = self.0.borrow();
            match vk {
                0x10 => keys[0xA0] || keys[0xA1],
                0x11 => keys[0xA2] || keys[0xA3],
                0x12 => keys[0xA4] || keys[0xA5],
                _ => keys[vk as usize],
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Step {
        Down(u8),
        Up(u8),
        /// Time passes (ms).
        Wait(u32),
        /// Physically pressed while the hook could not see it (secure desktop,
        /// elevated window). Nothing reaches the state machine.
        Hold(u8),
        /// Physically released while the hook could not see it.
        LoseUp(u8),
        /// The caller delivered the last first tap.
        Delivered,
        Reset,
        /// A plain key tap, which ends probation if nothing else is held.
        Sync,
    }
    use Step::*;

    struct Run {
        tap: SuperTap,
        probe: FakeProbe,
        now: u32,
        last_tap_time: u32,
    }

    impl Run {
        /// Starts trusted: the startup probation has already been ended.
        fn new(double_tap_ms: u32) -> Self {
            let mut run = Self::starting(double_tap_ms);
            run.step(Sync);
            assert_eq!(run.tap.sync(), PhysicalSync::Trusted);
            run
        }

        /// Starts as the hook does: in probation.
        fn starting(double_tap_ms: u32) -> Self {
            Self {
                tap: SuperTap::new(double_tap_ms),
                probe: FakeProbe::default(),
                // Close to wrap-around, so time arithmetic must wrap.
                now: u32::MAX - 1_000,
                last_tap_time: 0,
            }
        }

        fn step(&mut self, step: Step) -> Option<TapOutcome> {
            match step {
                Down(vk) | Up(vk) => {
                    let is_pressed = matches!(step, Down(_));
                    self.now = self.now.wrapping_add(10);
                    let outcome = self.tap.on_key(vk, is_pressed, self.now, &self.probe);
                    self.probe.0.borrow_mut()[vk as usize] = is_pressed;
                    if let Some(TapOutcome::FirstTap { time }) = outcome {
                        self.last_tap_time = time;
                    }
                    outcome
                }
                Wait(ms) => {
                    self.now = self.now.wrapping_add(ms);
                    None
                }
                Hold(vk) => {
                    self.probe.0.borrow_mut()[vk as usize] = true;
                    None
                }
                LoseUp(vk) => {
                    self.probe.0.borrow_mut()[vk as usize] = false;
                    None
                }
                Delivered => {
                    self.tap.tap_delivered(self.last_tap_time);
                    None
                }
                Reset => {
                    self.tap.reset();
                    None
                }
                Sync => {
                    self.step(Down(SYNC));
                    self.step(Up(SYNC))
                }
            }
        }

        /// Runs the steps and returns the kinds of the outcomes, in order.
        fn outcomes(&mut self, steps: &[Step]) -> Vec<&'static str> {
            steps
                .iter()
                .filter_map(|step| self.step(*step))
                .map(|outcome| match outcome {
                    TapOutcome::FirstTap { .. } => "first",
                    TapOutcome::SecondTap { .. } => "second",
                })
                .collect()
        }
    }

    fn repeat(vk: u8, times: usize) -> Vec<Step> {
        vec![Down(vk); times]
    }

    fn cat(parts: &[&[Step]]) -> Vec<Step> {
        parts.concat()
    }

    const TAP: [Step; 2] = [Down(VK_LWIN), Up(VK_LWIN)];

    #[test]
    fn tap_classification_table() {
        let cases: Vec<(&str, Vec<Step>, Vec<&str>)> = vec![
            ("LWin tap", vec![Down(VK_LWIN), Up(VK_LWIN)], vec!["first"]),
            ("RWin tap", vec![Down(VK_RWIN), Up(VK_RWIN)], vec!["first"]),
            (
                "auto-repeat is one tap",
                cat(&[&repeat(VK_LWIN, 30), &[Up(VK_LWIN)]]),
                vec!["first"],
            ),
            (
                "long hold has no timeout",
                vec![Down(VK_LWIN), Wait(10_000), Up(VK_LWIN)],
                vec!["first"],
            ),
            (
                "both sides overlap",
                vec![Down(VK_LWIN), Down(VK_RWIN), Up(VK_RWIN), Up(VK_LWIN)],
                vec![],
            ),
            (
                "both sides, released in press order",
                vec![Down(VK_LWIN), Down(VK_RWIN), Up(VK_LWIN), Up(VK_RWIN)],
                vec![],
            ),
            (
                "Shift held before Win",
                vec![Down(SHIFT), Down(VK_LWIN), Up(VK_LWIN), Up(SHIFT)],
                vec![],
            ),
            (
                "Shift held before Win, released during the hold",
                vec![Down(SHIFT), Down(VK_LWIN), Up(SHIFT), Up(VK_LWIN)],
                vec![],
            ),
            (
                "Shift pressed during the hold",
                vec![Down(VK_LWIN), Down(SHIFT), Up(SHIFT), Up(VK_LWIN)],
                vec![],
            ),
            (
                "Win+E",
                vec![Down(VK_LWIN), Down(E), Up(E), Up(VK_LWIN)],
                vec![],
            ),
            (
                "Win+E, E released after Win",
                vec![Down(VK_LWIN), Down(E), Up(VK_LWIN), Up(E)],
                vec![],
            ),
            (
                "Win+Shift+S",
                vec![
                    Down(VK_LWIN),
                    Down(SHIFT),
                    Down(0x53),
                    Up(0x53),
                    Up(SHIFT),
                    Up(VK_LWIN),
                ],
                vec![],
            ),
            (
                "Win+Ctrl+O",
                vec![
                    Down(VK_LWIN),
                    Down(CTRL),
                    Down(0x4F),
                    Up(0x4F),
                    Up(CTRL),
                    Up(VK_LWIN),
                ],
                vec![],
            ),
            (
                "a chord does not taint the next tap",
                cat(&[&[Down(VK_LWIN), Down(E), Up(E), Up(VK_LWIN)], &TAP]),
                vec!["first"],
            ),
            // Unpaired physical key-ups: evidence of a missed key-down.
            (
                "unpaired key-up during the hold makes it a chord",
                vec![Down(VK_LWIN), Up(0x39), Up(VK_LWIN)],
                vec![],
            ),
            (
                "unpaired up of the other side during the hold makes it a chord",
                vec![Down(VK_LWIN), Up(VK_RWIN), Up(VK_LWIN)],
                vec![],
            ),
            (
                "unpaired Win up: the next tap is native, the one after owned",
                cat(&[&[Up(VK_LWIN)], &TAP, &TAP]),
                vec!["first"],
            ),
            (
                "unpaired key-up: probation ends at the next clean key-up",
                cat(&[&[Up(0x39), Sync], &TAP]),
                vec!["first"],
            ),
        ];
        for (name, steps, expected) in cases {
            let mut run = Run::new(400);
            assert_eq!(run.outcomes(&steps), expected, "{name}");
            assert_eq!(run.tap.phase(), SuperPhase::Idle, "{name}: not idle");
        }
    }

    /// Resets, keys held across them, and how trust is regained.
    #[test]
    fn probation_table() {
        let cases: Vec<(&str, Vec<Step>, Vec<&str>)> = vec![
            (
                "startup: first tap native, second owned",
                cat(&[&TAP, &TAP]),
                vec!["first"],
            ),
            (
                "startup: a plain key tap ends probation",
                cat(&[&[Sync], &TAP]),
                vec!["first"],
            ),
            (
                "Win+L, key-ups lost, reset: first tap native, then owned",
                cat(&[
                    &[
                        Sync,
                        Down(VK_LWIN),
                        Down(L),
                        LoseUp(L),
                        LoseUp(VK_LWIN),
                        Reset,
                    ],
                    &TAP,
                    &TAP,
                ]),
                vec!["first"],
            ),
            (
                "after reset, typing ends probation without spending a tap",
                cat(&[&[Sync, Reset, Down(E), Up(E)], &TAP]),
                vec!["first"],
            ),
            (
                "Shift held across reset, Win tap",
                cat(&[&[Sync, Hold(SHIFT), Reset], &TAP]),
                vec![],
            ),
            (
                "Shift held across reset stays vetoed for every tap",
                cat(&[
                    &[Sync, Hold(SHIFT), Reset],
                    &TAP,
                    &TAP,
                    &TAP,
                    &[Down(E), Up(E)],
                    &TAP,
                ]),
                vec![],
            ),
            (
                "Shift held across reset, released during the Win hold",
                cat(&[&[
                    Sync,
                    Hold(SHIFT),
                    Reset,
                    Down(VK_LWIN),
                    Up(SHIFT),
                    Up(VK_LWIN),
                ]]),
                vec![],
            ),
            (
                "Shift held across reset: owned again only after it is released",
                cat(&[&[Sync, Hold(SHIFT), Reset], &TAP, &[Up(SHIFT)], &TAP, &TAP]),
                vec!["first"],
            ),
            (
                "Ctrl held across reset",
                cat(&[&[Sync, Hold(CTRL), Reset], &TAP, &TAP]),
                vec![],
            ),
            (
                "other Win side held across reset",
                cat(&[&[Sync, Hold(VK_RWIN), Reset], &TAP, &TAP]),
                vec![],
            ),
            (
                "other Win side held across reset, released: native, then owned",
                cat(&[
                    &[Sync, Hold(VK_RWIN), Reset],
                    &TAP,
                    &[Up(VK_RWIN)],
                    &TAP,
                    &TAP,
                ]),
                vec!["first"],
            ),
            (
                "reset during a pending tap: its release and the next tap are native",
                cat(&[&[Sync, Down(VK_LWIN), Reset, Up(VK_LWIN)], &TAP, &TAP]),
                vec!["first"],
            ),
            (
                "after recovery a chord is still a chord",
                cat(&[
                    &[Sync, Reset, Sync],
                    &[Down(VK_LWIN), Down(E), Up(E), Up(VK_LWIN)],
                ]),
                vec![],
            ),
            (
                "a key-up while another key is still down does not end probation",
                cat(&[&[Sync, Reset, Down(SHIFT), Down(E), Up(E)], &TAP]),
                vec![],
            ),
        ];
        for (name, steps, expected) in cases {
            let mut run = Run::starting(400);
            assert_eq!(run.outcomes(&steps), expected, "{name}");
        }
    }

    #[test]
    fn double_tap_table() {
        let cases: Vec<(&str, u32, Vec<Step>, Vec<&str>)> = vec![
            (
                "quick second tap",
                400,
                cat(&[&TAP, &[Delivered, Wait(100)], &TAP]),
                vec!["first", "second"],
            ),
            (
                "edge of the window",
                400,
                cat(&[&TAP, &[Delivered, Wait(390)], &TAP]),
                vec!["first", "second"],
            ),
            (
                "too slow",
                400,
                cat(&[&TAP, &[Delivered, Wait(500)], &TAP]),
                vec!["first", "first"],
            ),
            (
                "undelivered first tap never pairs",
                400,
                cat(&[&TAP, &[Wait(100)], &TAP]),
                vec!["first", "first"],
            ),
            (
                "third tap starts a new pair",
                400,
                cat(&[
                    &TAP,
                    &[Delivered, Wait(100)],
                    &TAP,
                    &[Wait(100)],
                    &TAP,
                    &[Delivered, Wait(100)],
                    &TAP,
                ]),
                vec!["first", "second", "first", "second"],
            ),
            (
                "a key between the taps resets the pair",
                400,
                cat(&[&TAP, &[Delivered, Down(E), Up(E)], &TAP]),
                vec!["first", "first"],
            ),
            (
                "a chord as the second press resets the pair",
                400,
                cat(&[
                    &TAP,
                    &[Delivered, Down(VK_LWIN), Down(E), Up(E), Up(VK_LWIN)],
                    &TAP,
                ]),
                vec!["first", "first"],
            ),
            (
                "other side as second tap",
                400,
                cat(&[&TAP, &[Delivered, Down(VK_RWIN), Up(VK_RWIN)]]),
                vec!["first", "second"],
            ),
            (
                "reset closes the window",
                400,
                cat(&[&TAP, &[Delivered, Reset, Sync], &TAP]),
                vec!["first", "first"],
            ),
            (
                "double tap disabled",
                0,
                cat(&[&TAP, &[Delivered, Wait(10)], &TAP]),
                vec!["first", "first"],
            ),
        ];
        for (name, double_tap_ms, steps, expected) in cases {
            let mut run = Run::new(double_tap_ms);
            assert_eq!(run.outcomes(&steps), expected, "{name}");
        }
    }

    #[test]
    fn phases_and_sync_follow_the_transition_table() {
        let mut run = Run::starting(400);
        let left = WinSides(0b01);
        let both = WinSides(0b11);
        let expect = |run: &mut Run, step: Step, phase: SuperPhase, sync: PhysicalSync| {
            run.step(step);
            assert_eq!(run.tap.phase(), phase, "after {step:?}");
            assert_eq!(run.tap.sync(), sync, "after {step:?}");
        };
        use PhysicalSync::{Probation, Trusted};
        let pending = SuperPhase::SuperPending {
            sides: left,
            is_second: false,
        };
        expect(
            &mut run,
            Down(VK_LWIN),
            SuperPhase::ChordPassthrough { sides: left },
            Probation,
        );
        expect(&mut run, Up(VK_LWIN), SuperPhase::Idle, Trusted);
        expect(&mut run, Down(VK_LWIN), pending, Trusted);
        expect(&mut run, Down(VK_LWIN), pending, Trusted);
        expect(
            &mut run,
            Down(VK_RWIN),
            SuperPhase::ChordPassthrough { sides: both },
            Trusted,
        );
        expect(
            &mut run,
            Up(VK_RWIN),
            SuperPhase::ChordPassthrough { sides: left },
            Trusted,
        );
        expect(&mut run, Up(VK_LWIN), SuperPhase::Idle, Trusted);
        expect(&mut run, Down(SHIFT), SuperPhase::Idle, Trusted);
        expect(
            &mut run,
            Down(VK_LWIN),
            SuperPhase::ChordPassthrough { sides: left },
            Trusted,
        );
        expect(&mut run, Up(VK_LWIN), SuperPhase::Idle, Trusted);
        expect(&mut run, Up(SHIFT), SuperPhase::Idle, Trusted);
        expect(&mut run, Down(VK_LWIN), pending, Trusted);
        expect(
            &mut run,
            Up(0x39),
            SuperPhase::ChordPassthrough { sides: left },
            Probation,
        );
        expect(&mut run, Up(VK_LWIN), SuperPhase::Idle, Trusted);
        expect(&mut run, Reset, SuperPhase::Idle, Probation);
    }

    #[test]
    fn the_veto_ignores_only_the_key_in_flight_and_non_keys() {
        // The key in flight and its generic alias read down at its own key-up.
        assert!(veto_ignores(SHIFT, SHIFT));
        assert!(veto_ignores(SHIFT, 0x10));
        assert!(veto_ignores(VK_LWIN, VK_LWIN));
        // The other Win side and the other Shift still veto.
        assert!(!veto_ignores(VK_LWIN, VK_RWIN));
        assert!(!veto_ignores(SHIFT, 0xA1));
        assert!(!veto_ignores(E, 0x10));
        // Mouse buttons and the Start mask never veto.
        for vk in [0x01, 0x02, 0x04, 0x05, 0x06, VK_MASK] {
            assert!(veto_ignores(E, vk), "{vk:#x}");
        }
    }
}
