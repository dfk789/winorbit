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

/// Left Windows key.
pub const VK_LWIN: u8 = 0x5B;
/// Right Windows key.
pub const VK_RWIN: u8 = 0x5C;

/// Asks Windows whether a key is down right now (`GetAsyncKeyState` in
/// production).
///
/// Inside a `WH_KEYBOARD_LL` callback this reports the state *before* the
/// event being handled. So for the event's own key it tells a key repeat
/// (already down) from a fresh press (still up), and for every other key it
/// is that key's current state.
pub trait KeyStateProbe {
    fn is_down(&self, vk: u8) -> bool;
}

/// Physical Win keys believed down, one bit per side.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WinSides(u8);

impl WinSides {
    const SIDES: [(u8, u8); 2] = [(VK_LWIN, 0b01), (VK_RWIN, 0b10)];

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

    /// Drops every side except `current` that Windows reports as up: its
    /// key-up was lost (for example on the secure desktop).
    fn heal(&mut self, current: u8, probe: &impl KeyStateProbe) {
        for (vk, bit) in Self::SIDES {
            if vk != current && self.0 & bit != 0 && !probe.is_down(vk) {
                self.0 &= !bit;
            }
        }
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

    /// Drops every key Windows reports as up. Only the keys believed down are
    /// queried, normally none or a handful.
    fn heal(&mut self, probe: &impl KeyStateProbe) {
        for word in 0..self.0.len() {
            let mut bits = self.0[word];
            while bits != 0 {
                let bit = bits.trailing_zeros();
                bits &= bits - 1;
                let vk = (word as u32 * 64 + bit) as u8;
                if !probe.is_down(vk) {
                    self.0[word] &= !(1 << bit);
                }
            }
        }
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

/// A clean tap, reported on the key-up that ended it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapOutcome {
    /// Suppress Start and deliver the overview action.
    FirstTap { time: u32 },
    /// Second tap of a double tap: Windows opens Start natively.
    SecondTap { time: u32 },
}

#[derive(Debug)]
pub struct SuperTap {
    phase: SuperPhase,
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
            keys: KeysDown::default(),
            pair_anchor: None,
            double_tap_ms,
        }
    }

    /// Forgets all physical state, e.g. after the session was locked.
    pub fn reset(&mut self) {
        *self = Self::new(self.double_tap_ms);
    }

    /// Opens the double-tap window after a first tap's overview action was
    /// delivered. Taps that were not delivered never pair.
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
        if vk == VK_LWIN || vk == VK_RWIN {
            return if is_pressed {
                self.on_win_down(vk, time, probe);
                None
            } else {
                self.on_win_up(vk, time)
            };
        }

        let was_down = self.keys.set(vk, is_pressed);
        if !is_pressed && !was_down {
            // A release with no press seen (e.g. a key-up leaking from the
            // secure desktop) is not part of anything.
            return None;
        }
        self.pair_anchor = None;
        self.phase = match self.phase {
            SuperPhase::SuperPending { sides, .. } | SuperPhase::ChordPassthrough { sides } => {
                SuperPhase::ChordPassthrough { sides }
            }
            SuperPhase::Idle => SuperPhase::Idle,
        };
        None
    }

    fn on_win_down(&mut self, vk: u8, time: u32, probe: &impl KeyStateProbe) {
        let mut sides = match self.phase {
            SuperPhase::Idle => WinSides::default(),
            SuperPhase::SuperPending { sides, .. } | SuperPhase::ChordPassthrough { sides } => {
                sides
            }
        };
        if sides.contains(vk) {
            if probe.is_down(vk) {
                // Auto-repeat of a key already down: no new press.
                return;
            }
            // Windows says this key was up, so its last key-up was lost and
            // this is a fresh press.
            sides.remove(vk);
        }
        sides.heal(vk, probe);

        if !sides.is_empty() {
            // The other Win side is (really) down too.
            sides.insert(vk);
            self.pair_anchor = None;
            self.phase = SuperPhase::ChordPassthrough { sides };
            return;
        }

        // A new hold. Keys whose key-up was lost must not make it a chord.
        self.keys.heal(probe);
        sides.insert(vk);
        let is_second = self
            .pair_anchor
            .take()
            .is_some_and(|anchor| time.wrapping_sub(anchor) <= self.double_tap_ms);
        self.phase = if self.keys.is_empty() {
            SuperPhase::SuperPending { sides, is_second }
        } else {
            SuperPhase::ChordPassthrough { sides }
        };
    }

    fn on_win_up(&mut self, vk: u8, time: u32) -> Option<TapOutcome> {
        let (mut sides, tap) = match self.phase {
            SuperPhase::Idle => return None,
            SuperPhase::SuperPending { sides, is_second } => (sides, Some(is_second)),
            SuperPhase::ChordPassthrough { sides } => (sides, None),
        };
        if !sides.contains(vk) {
            // Release of a side never seen going down.
            return None;
        }
        sides.remove(vk);
        if !sides.is_empty() {
            self.phase = SuperPhase::ChordPassthrough { sides };
            return None;
        }
        self.phase = SuperPhase::Idle;
        tap.map(|is_second| match is_second {
            true => TapOutcome::SecondTap { time },
            false => TapOutcome::FirstTap { time },
        })
    }

    #[cfg(test)]
    pub fn phase(&self) -> SuperPhase {
        self.phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const SHIFT: u8 = 0xA0;
    const CTRL: u8 = 0xA2;
    const L: u8 = 0x4C;
    const E: u8 = 0x45;

    /// Windows' key state as `GetAsyncKeyState` reports it to a hook: updated
    /// only after the hook has seen an event, unless a key-up is "lost".
    #[derive(Default)]
    struct FakeProbe(RefCell<KeysDown>);

    impl KeyStateProbe for FakeProbe {
        fn is_down(&self, vk: u8) -> bool {
            let (word, bit) = KeysDown::slot(vk);
            self.0.borrow().0[word] & bit != 0
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Step {
        Down(u8),
        Up(u8),
        /// Time passes (ms).
        Wait(u32),
        /// The key goes up without the hook seeing it (secure desktop).
        LoseUp(u8),
        /// The caller delivered the last first tap.
        Delivered,
        Reset,
    }
    use Step::*;

    struct Run {
        tap: SuperTap,
        probe: FakeProbe,
        now: u32,
        last_tap_time: u32,
    }

    impl Run {
        fn new(double_tap_ms: u32) -> Self {
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
                    self.probe.0.borrow_mut().set(vk, is_pressed);
                    if let Some(TapOutcome::FirstTap { time }) = outcome {
                        self.last_tap_time = time;
                    }
                    outcome
                }
                Wait(ms) => {
                    self.now = self.now.wrapping_add(ms);
                    None
                }
                LoseUp(vk) => {
                    self.probe.0.borrow_mut().set(vk, false);
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
                vec![
                    Down(VK_LWIN),
                    Down(E),
                    Up(E),
                    Up(VK_LWIN),
                    Down(VK_LWIN),
                    Up(VK_LWIN),
                ],
                vec!["first"],
            ),
            (
                "unpaired Win up does nothing",
                vec![Up(VK_LWIN), Down(VK_LWIN), Up(VK_LWIN)],
                vec!["first"],
            ),
            (
                "unpaired key up (secure desktop leak) does not chord",
                vec![Down(VK_LWIN), Up(0x39), Up(VK_LWIN)],
                vec!["first"],
            ),
            (
                "unpaired up of the other side does not commit",
                vec![Down(VK_LWIN), Up(VK_RWIN), Up(VK_LWIN)],
                vec!["first"],
            ),
            (
                "Win+L, both key-ups lost, next tap is clean",
                vec![
                    Down(VK_LWIN),
                    Down(L),
                    LoseUp(L),
                    LoseUp(VK_LWIN),
                    Down(VK_LWIN),
                    Up(VK_LWIN),
                ],
                vec!["first"],
            ),
            (
                "lost LWin up, then RWin tap is clean",
                vec![Down(VK_LWIN), LoseUp(VK_LWIN), Down(VK_RWIN), Up(VK_RWIN)],
                vec!["first"],
            ),
            (
                "lost key-up of a non-Win key, then a tap is clean",
                vec![Down(CTRL), LoseUp(CTRL), Down(VK_LWIN), Up(VK_LWIN)],
                vec!["first"],
            ),
            (
                "after recovery a chord is still a chord",
                vec![
                    Down(VK_LWIN),
                    Down(L),
                    LoseUp(L),
                    LoseUp(VK_LWIN),
                    Down(VK_LWIN),
                    Down(E),
                    Up(E),
                    Up(VK_LWIN),
                ],
                vec![],
            ),
            (
                "reset mid-hold, then a tap",
                vec![Down(VK_LWIN), Down(L), Reset, Down(VK_LWIN), Up(VK_LWIN)],
                vec!["first"],
            ),
        ];
        for (name, steps, expected) in cases {
            let mut run = Run::new(400);
            assert_eq!(run.outcomes(&steps), expected, "{name}");
            assert_eq!(run.tap.phase(), SuperPhase::Idle, "{name}: not idle");
        }
    }

    #[test]
    fn double_tap_table() {
        let tap = [Down(VK_LWIN), Up(VK_LWIN)];
        let cases: Vec<(&str, u32, Vec<Step>, Vec<&str>)> = vec![
            (
                "quick second tap",
                400,
                cat(&[&tap, &[Delivered, Wait(100)], &tap]),
                vec!["first", "second"],
            ),
            (
                "edge of the window",
                400,
                cat(&[&tap, &[Delivered, Wait(390)], &tap]),
                vec!["first", "second"],
            ),
            (
                "too slow",
                400,
                cat(&[&tap, &[Delivered, Wait(500)], &tap]),
                vec!["first", "first"],
            ),
            (
                "undelivered first tap never pairs",
                400,
                cat(&[&tap, &[Wait(100)], &tap]),
                vec!["first", "first"],
            ),
            (
                "third tap starts a new pair",
                400,
                cat(&[
                    &tap,
                    &[Delivered, Wait(100)],
                    &tap,
                    &[Wait(100)],
                    &tap,
                    &[Delivered, Wait(100)],
                    &tap,
                ]),
                vec!["first", "second", "first", "second"],
            ),
            (
                "a key between the taps resets the pair",
                400,
                cat(&[&tap, &[Delivered, Down(E), Up(E)], &tap]),
                vec!["first", "first"],
            ),
            (
                "a chord as the second press resets the pair",
                400,
                cat(&[
                    &tap,
                    &[Delivered, Down(VK_LWIN), Down(E), Up(E), Up(VK_LWIN)],
                    &tap,
                ]),
                vec!["first", "first"],
            ),
            (
                "other side as second tap",
                400,
                cat(&[&tap, &[Delivered, Down(VK_RWIN), Up(VK_RWIN)]]),
                vec!["first", "second"],
            ),
            (
                "reset closes the window",
                400,
                cat(&[&tap, &[Delivered, Reset], &tap]),
                vec!["first", "first"],
            ),
            (
                "double tap disabled",
                0,
                cat(&[&tap, &[Delivered, Wait(10)], &tap]),
                vec!["first", "first"],
            ),
        ];
        for (name, double_tap_ms, steps, expected) in cases {
            let mut run = Run::new(double_tap_ms);
            assert_eq!(run.outcomes(&steps), expected, "{name}");
        }
    }

    #[test]
    fn phases_follow_the_transition_table() {
        let mut run = Run::new(400);
        let left = WinSides(0b01);
        let both = WinSides(0b11);
        let expect = |run: &mut Run, step: Step, phase: SuperPhase| {
            run.step(step);
            assert_eq!(run.tap.phase(), phase, "after {step:?}");
        };
        expect(
            &mut run,
            Down(VK_LWIN),
            SuperPhase::SuperPending {
                sides: left,
                is_second: false,
            },
        );
        expect(
            &mut run,
            Down(VK_LWIN),
            SuperPhase::SuperPending {
                sides: left,
                is_second: false,
            },
        );
        expect(
            &mut run,
            Down(VK_RWIN),
            SuperPhase::ChordPassthrough { sides: both },
        );
        expect(
            &mut run,
            Up(VK_RWIN),
            SuperPhase::ChordPassthrough { sides: left },
        );
        expect(&mut run, Up(VK_LWIN), SuperPhase::Idle);
        expect(&mut run, Down(SHIFT), SuperPhase::Idle);
        expect(
            &mut run,
            Down(VK_LWIN),
            SuperPhase::ChordPassthrough { sides: left },
        );
        expect(&mut run, Up(VK_LWIN), SuperPhase::Idle);
    }
}
