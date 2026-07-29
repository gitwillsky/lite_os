//! Desktop-owned global accelerator chord matching and key-grab state.

use display_proto::{AcceleratorChord, MAX_ACCELERATORS};

/// Physical keys that can compose one grab: the eight modifier key codes
/// behind the Shift/Ctrl/Alt/Super mask bits plus the chord key itself.
const MAX_GRAB_KEYS: usize = 9;

/// Routing decision for one keyboard transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum KeyRoute {
    /// Deliver to the presented focused surface.
    Focused,
    /// Deliver to the desktop (surface zero) as part of an accelerator sequence.
    Desktop,
}

/// Global accelerator state owned by the session's desktop connection.
///
/// The desktop replaces the chord table atomically and owns every shortcut
/// action; the compositor only matches fixed physical chords and forwards the
/// complete down/up sequence. Without this state every key routes only to the
/// focused surface and no system shortcut (Alt+Tab hold-cycle) can work.
pub(super) struct Accelerators {
    chords: [AcceleratorChord; MAX_ACCELERATORS],
    chord_count: usize,
    /// Active key grab; while `Some`, every key event belongs to the desktop
    /// until no chord key remains held.
    grab: Option<Grab>,
}

/// One matched chord's physical keys while the grab owns the key stream.
struct Grab {
    /// Modifier key codes pressed at match time plus the chord code. Bit i of
    /// `held` tracks whether `keys[i]` is currently down, so a key re-pressed
    /// during the grab (Alt held, Tab tapped again) rejoins the sequence.
    keys: [u32; MAX_GRAB_KEYS],
    count: usize,
    held: u16,
}

impl Grab {
    fn push(&mut self, code: u32) {
        if let Some(slot) = self.keys.get_mut(self.count) {
            *slot = code;
            self.held |= 1 << self.count;
            self.count += 1;
        }
    }

    fn index_of(&self, code: u32) -> Option<usize> {
        self.keys[..self.count].iter().position(|&key| key == code)
    }
}

impl Accelerators {
    pub(super) fn new() -> Self {
        Self {
            chords: [AcceleratorChord {
                modifiers: 0,
                code: 0,
            }; MAX_ACCELERATORS],
            chord_count: 0,
            grab: None,
        }
    }

    /// Atomically replaces the chord table (the wire codec already bounds the
    /// count). A grab in progress keeps running so the desktop still receives
    /// the complete sequence of the chord it started.
    pub(super) fn replace(&mut self, chords: impl Iterator<Item = AcceleratorChord>) {
        self.chord_count = 0;
        for chord in chords.take(MAX_ACCELERATORS) {
            self.chords[self.chord_count] = chord;
            self.chord_count += 1;
        }
    }

    /// Drops the table and force-ends any grab on desktop disconnect or epoch
    /// reset; without it a stale grab would keep stealing keys from the next
    /// desktop's focused surfaces.
    pub(super) fn clear(&mut self) {
        self.chord_count = 0;
        self.grab = None;
    }

    /// Matches one key transition and advances the grab state machine.
    ///
    /// 1. An active grab owns every key event: releasing a chord key clears
    ///    its held bit, pressing one again sets it, and the grab ends once no
    ///    chord key remains held — release order is irrelevant. New key downs
    ///    never re-match while grabbed.
    /// 2. Without a grab, a fresh key down (value one, never repeat) whose
    ///    exact `(modifiers, code)` pair matches a chord starts a grab over
    ///    the chord key plus the physical modifier keys behind the mask.
    /// 3. Everything else routes to the focused surface.
    ///
    /// # Parameters
    ///
    /// - `code`: Linux evdev key code of the transition.
    /// - `value`: Linux key value: zero up, one down, two repeat.
    /// - `modifiers`: Current Shift/Ctrl/Alt/Super mask after this event.
    /// - `modifier_keys`: Physical modifier key codes currently held.
    pub(super) fn route(
        &mut self,
        code: u32,
        value: i32,
        modifiers: u32,
        modifier_keys: &[u16],
    ) -> KeyRoute {
        if let Some(grab) = &mut self.grab {
            if let Some(index) = grab.index_of(code) {
                match value {
                    0 => grab.held &= !(1 << index),
                    1 => grab.held |= 1 << index,
                    _ => {}
                }
            }
            if grab.held == 0 {
                self.grab = None;
            }
            return KeyRoute::Desktop;
        }
        if value == 1
            && self.chords[..self.chord_count]
                .iter()
                .any(|chord| chord.modifiers == modifiers && chord.code == code)
        {
            let mut grab = Grab {
                keys: [0; MAX_GRAB_KEYS],
                count: 0,
                held: 0,
            };
            for &key in modifier_keys {
                grab.push(u32::from(key));
            }
            grab.push(code);
            self.grab = Some(grab);
            return KeyRoute::Desktop;
        }
        KeyRoute::Focused
    }
}

#[cfg(test)]
mod tests {
    use super::{Accelerators, KeyRoute};
    use display_proto::AcceleratorChord;

    const ALT: u16 = 56;
    const TAB: u32 = 15;

    fn alt_tab() -> AcceleratorChord {
        AcceleratorChord {
            modifiers: 4,
            code: TAB,
        }
    }

    fn with_table(chords: &[AcceleratorChord]) -> Accelerators {
        let mut accelerators = Accelerators::new();
        accelerators.replace(chords.iter().copied());
        accelerators
    }

    #[test]
    fn matching_chord_routes_to_desktop() {
        let mut accelerators = with_table(&[alt_tab()]);
        assert_eq!(
            accelerators.route(TAB, 1, 4, &[ALT]),
            KeyRoute::Desktop,
            "exact (modifiers, code) hit must route desktop"
        );
    }

    #[test]
    fn non_matching_keys_route_to_focused_surface() {
        let mut accelerators = with_table(&[alt_tab()]);
        // Extra Shift changes the mask: the chord match is exact.
        assert_eq!(accelerators.route(TAB, 1, 5, &[ALT, 42]), KeyRoute::Focused);
        // Same mask but a different key code.
        assert_eq!(accelerators.route(30, 1, 4, &[ALT]), KeyRoute::Focused);
        // Chord code without the modifier mask.
        assert_eq!(accelerators.route(TAB, 1, 0, &[]), KeyRoute::Focused);
    }

    #[test]
    fn repeat_never_starts_a_grab() {
        let mut accelerators = with_table(&[alt_tab()]);
        assert_eq!(accelerators.route(TAB, 2, 4, &[ALT]), KeyRoute::Focused);
    }

    #[test]
    fn grab_forwards_the_complete_sequence_until_every_chord_key_releases() {
        let mut accelerators = with_table(&[alt_tab()]);
        assert_eq!(accelerators.route(TAB, 1, 4, &[ALT]), KeyRoute::Desktop);
        // Unrelated keys and the modifier's own transitions stay desktop-bound.
        assert_eq!(accelerators.route(30, 1, 4, &[ALT]), KeyRoute::Desktop);
        assert_eq!(accelerators.route(TAB, 2, 4, &[ALT]), KeyRoute::Desktop);
        // Release order is irrelevant: Tab first, Alt last still ends the grab.
        assert_eq!(accelerators.route(TAB, 0, 4, &[ALT]), KeyRoute::Desktop);
        assert_eq!(
            accelerators.route(u32::from(ALT), 0, 0, &[]),
            KeyRoute::Desktop,
            "the final chord-key release is still part of the sequence"
        );
        // The grab ended: the next key returns to the focused surface.
        assert_eq!(accelerators.route(30, 0, 0, &[]), KeyRoute::Focused);
    }

    #[test]
    fn grab_ends_when_the_modifier_releases_first() {
        let mut accelerators = with_table(&[alt_tab()]);
        assert_eq!(accelerators.route(TAB, 1, 4, &[ALT]), KeyRoute::Desktop);
        assert_eq!(
            accelerators.route(u32::from(ALT), 0, 0, &[]),
            KeyRoute::Desktop
        );
        // Alt is up but Tab is still held: the sequence is not complete.
        assert_eq!(accelerators.route(TAB, 2, 0, &[]), KeyRoute::Desktop);
        assert_eq!(accelerators.route(TAB, 0, 0, &[]), KeyRoute::Desktop);
        assert_eq!(accelerators.route(30, 1, 0, &[]), KeyRoute::Focused);
    }

    #[test]
    fn repressed_chord_key_rejoins_the_held_set() {
        let mut accelerators = with_table(&[alt_tab()]);
        assert_eq!(accelerators.route(TAB, 1, 4, &[ALT]), KeyRoute::Desktop);
        assert_eq!(accelerators.route(TAB, 0, 4, &[ALT]), KeyRoute::Desktop);
        // Alt held, Tab tapped again: no re-match, but Tab is held again.
        assert_eq!(accelerators.route(TAB, 1, 4, &[ALT]), KeyRoute::Desktop);
        assert_eq!(
            accelerators.route(u32::from(ALT), 0, 0, &[]),
            KeyRoute::Desktop
        );
        // Tab still held, so the grab survives Alt's release until Tab is up.
        assert_eq!(accelerators.route(TAB, 0, 0, &[]), KeyRoute::Desktop);
        assert_eq!(accelerators.route(30, 0, 0, &[]), KeyRoute::Focused);
    }

    #[test]
    fn table_replacement_applies_atomically() {
        let mut accelerators = with_table(&[alt_tab()]);
        let ctrl_c = AcceleratorChord {
            modifiers: 2,
            code: 46,
        };
        accelerators.replace([ctrl_c].into_iter());
        assert_eq!(accelerators.route(TAB, 1, 4, &[ALT]), KeyRoute::Focused);
        assert_eq!(accelerators.route(46, 1, 2, &[29]), KeyRoute::Desktop);
    }

    #[test]
    fn clear_force_ends_a_grab_on_desktop_disconnect() {
        let mut accelerators = with_table(&[alt_tab()]);
        assert_eq!(accelerators.route(TAB, 1, 4, &[ALT]), KeyRoute::Desktop);
        accelerators.clear();
        assert_eq!(
            accelerators.route(u32::from(ALT), 0, 0, &[]),
            KeyRoute::Focused,
            "a stale grab must not steal keys from the next epoch"
        );
        // The table is gone too: the same chord no longer matches.
        assert_eq!(accelerators.route(TAB, 1, 4, &[ALT]), KeyRoute::Focused);
    }
}
