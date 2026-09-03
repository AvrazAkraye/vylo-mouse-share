//! Per-client input tuning applied on the *sending* side, just before an
//! event leaves for the peer: modifier-key remapping and pointer speed.
//!
//! Applying this on the capturing machine means each machine decides how
//! its own keyboard and mouse behave on the other one — a Mac user can
//! make ⌘ act as Ctrl on the Windows PC without the Windows side knowing
//! anything about it — and the peer only ever sees ordinary key codes.

use std::collections::HashMap;

use input_event::{Event, KeyboardEvent, PointerEvent, scancode::Linux};
use lan_mouse_ipc::{DEFAULT_SPEED, Modifier, ModifierMap, clamp_speed};

/// XKB modifier bits as used in [`KeyboardEvent::Modifiers`] (see the
/// `XMods` bitflags in the capture / emulation backends)
const CONTROL_MASK: u32 = 1 << 2;
const MOD1_MASK: u32 = 1 << 3;
const MOD4_MASK: u32 = 1 << 6;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClientTuning {
    pub speed: f64,
    pub modifiers: ModifierMap,
}

impl Default for ClientTuning {
    fn default() -> Self {
        Self {
            speed: DEFAULT_SPEED,
            modifiers: ModifierMap::default(),
        }
    }
}

impl ClientTuning {
    pub(crate) fn new(speed: f64, modifiers: ModifierMap) -> Self {
        Self {
            speed: clamp_speed(speed),
            modifiers,
        }
    }
}

/// Mutable per-client state needed to apply a [`ClientTuning`].
#[derive(Clone, Debug, Default)]
pub(crate) struct TuningState {
    /// fractional motion left over after scaling, so that slow speeds
    /// don't lose sub-pixel movement (receivers truncate to integers)
    rem_dx: f64,
    rem_dy: f64,
    /// modifier keys currently held, raw key code → the code the peer
    /// saw go down. A key-up must release exactly that code, even if
    /// the map changed in between; and when two local keys map to the
    /// same target, the target is released only once both are up.
    held: HashMap<u32, u32>,
}

impl TuningState {
    /// forget everything: a new session starts from a clean slate
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// scale a delta, carrying the fractional part to the next call
    fn scale(&mut self, dx: f64, dy: f64, speed: f64) -> (f64, f64) {
        if speed == DEFAULT_SPEED {
            return (dx, dy);
        }
        self.rem_dx += dx * speed;
        self.rem_dy += dy * speed;
        let (sx, sy) = (self.rem_dx.trunc(), self.rem_dy.trunc());
        self.rem_dx -= sx;
        self.rem_dy -= sy;
        (sx, sy)
    }

    /// The code to send for a modifier key event, or `None` if the peer
    /// should not see it (target already held via another local key).
    fn modifier_key(&mut self, map: &ModifierMap, key: u32, state: u8) -> Option<u32> {
        if state == 1 {
            // auto-repeat of a held key: keep the code it went down with
            if let Some(&sent) = self.held.get(&key) {
                return Some(sent);
            }
            let target = map_key(map, key);
            let already_down = self.held.values().any(|&s| s == target);
            self.held.insert(key, target);
            (!already_down).then_some(target)
        } else {
            let sent = self
                .held
                .remove(&key)
                .unwrap_or_else(|| map_key(map, key));
            let still_down = self.held.values().any(|&s| s == sent);
            (!still_down).then_some(sent)
        }
    }

    /// The code the peer saw for a raw key that is going up as part of a
    /// capture release (see `release_capture`): the recorded one if the
    /// key went down through us, else the map's current answer.
    pub(crate) fn release_code(&mut self, map: &ModifierMap, key: u32) -> u32 {
        self.held.remove(&key).unwrap_or_else(|| map_key(map, key))
    }

    /// codes the peer still holds that no raw key accounted for; drained
    pub(crate) fn drain_held(&mut self) -> Vec<u32> {
        let mut codes: Vec<u32> = self.held.drain().map(|(_, sent)| sent).collect();
        codes.sort_unstable();
        codes.dedup();
        codes
    }
}

/// the (left, right) key codes for a modifier role
fn keys_of(modifier: Modifier) -> (Linux, Linux) {
    match modifier {
        Modifier::Ctrl => (Linux::KeyLeftCtrl, Linux::KeyRightCtrl),
        Modifier::Alt => (Linux::KeyLeftAlt, Linux::KeyRightalt),
        Modifier::Meta => (Linux::KeyLeftMeta, Linux::KeyRightmeta),
    }
}

fn mask_of(modifier: Modifier) -> u32 {
    match modifier {
        Modifier::Ctrl => CONTROL_MASK,
        Modifier::Alt => MOD1_MASK,
        Modifier::Meta => MOD4_MASK,
    }
}

/// which role a raw key code plays, if it is a modifier we remap
/// (`true` = right-hand variant)
fn role_of(key: u32) -> Option<(Modifier, bool)> {
    let key = Linux::try_from(key).ok()?;
    Some(match key {
        Linux::KeyLeftCtrl => (Modifier::Ctrl, false),
        Linux::KeyRightCtrl => (Modifier::Ctrl, true),
        Linux::KeyLeftAlt => (Modifier::Alt, false),
        Linux::KeyRightalt => (Modifier::Alt, true),
        Linux::KeyLeftMeta => (Modifier::Meta, false),
        Linux::KeyRightmeta => (Modifier::Meta, true),
        _ => return None,
    })
}

/// what a local modifier role acts as under `map`
fn target_of(map: &ModifierMap, modifier: Modifier) -> Modifier {
    match modifier {
        Modifier::Ctrl => map.ctrl,
        Modifier::Alt => map.alt,
        Modifier::Meta => map.meta,
    }
}

/// remap a single key code (left stays left, right stays right)
pub(crate) fn map_key(map: &ModifierMap, key: u32) -> u32 {
    if map.is_identity() {
        return key;
    }
    match role_of(key) {
        Some((role, right)) => {
            let (l, r) = keys_of(target_of(map, role));
            (if right { r } else { l }) as u32
        }
        None => key,
    }
}

/// remap the modifier bitmask that accompanies key events on some
/// backends (macOS emulation trusts it over the individual key events)
fn map_mods(map: &ModifierMap, mods: u32) -> u32 {
    if map.is_identity() {
        return mods;
    }
    let mut out = mods & !(CONTROL_MASK | MOD1_MASK | MOD4_MASK);
    for role in [Modifier::Ctrl, Modifier::Alt, Modifier::Meta] {
        if mods & mask_of(role) != 0 {
            out |= mask_of(target_of(map, role));
        }
    }
    out
}

/// Apply the tuning to one captured event. `None` means the event must
/// not be forwarded (a duplicate modifier press/release under a map
/// where two local keys act as the same target).
pub(crate) fn tune_event(
    tuning: &ClientTuning,
    state: &mut TuningState,
    event: Event,
) -> Option<Event> {
    Some(match event {
        Event::Pointer(PointerEvent::Motion { time, dx, dy }) => {
            let (dx, dy) = state.scale(dx, dy, tuning.speed);
            Event::Pointer(PointerEvent::Motion { time, dx, dy })
        }
        Event::Keyboard(KeyboardEvent::Key {
            time,
            key,
            state: key_state,
        }) if role_of(key).is_some() => {
            let key = state.modifier_key(&tuning.modifiers, key, key_state)?;
            Event::Keyboard(KeyboardEvent::Key {
                time,
                key,
                state: key_state,
            })
        }
        Event::Keyboard(KeyboardEvent::Modifiers {
            depressed,
            latched,
            locked,
            group,
        }) => Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: map_mods(&tuning.modifiers, depressed),
            latched: map_mods(&tuning.modifiers, latched),
            locked: map_mods(&tuning.modifiers, locked),
            group,
        }),
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIFT_MASK: u32 = 1 << 0;
    const LOCK_MASK: u32 = 1 << 1;

    fn swap_ctrl_meta() -> ModifierMap {
        ModifierMap {
            ctrl: Modifier::Meta,
            alt: Modifier::Alt,
            meta: Modifier::Ctrl,
        }
    }

    fn both_ctrl() -> ModifierMap {
        ModifierMap {
            ctrl: Modifier::Ctrl,
            alt: Modifier::Alt,
            meta: Modifier::Ctrl,
        }
    }

    fn key(key: Linux, state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: key as u32,
            state,
        })
    }

    fn motion(dx: f64, dy: f64) -> Event {
        Event::Pointer(PointerEvent::Motion { time: 0, dx, dy })
    }

    /// run a key sequence through one client's tuning, collecting what
    /// actually goes on the wire as (key, state)
    fn wire(map: ModifierMap, seq: &[(Linux, u8)]) -> Vec<(Linux, u8)> {
        let t = ClientTuning::new(1.0, map);
        let mut st = TuningState::default();
        seq.iter()
            .filter_map(|&(k, s)| tune_event(&t, &mut st, key(k, s)))
            .map(|e| match e {
                Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                    (Linux::try_from(key).unwrap(), state)
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[test]
    fn identity_leaves_everything_alone() {
        let t = ClientTuning::default();
        let mut st = TuningState::default();
        for k in [
            Linux::KeyLeftCtrl,
            Linux::KeyRightmeta,
            Linux::KeyA,
            Linux::KeyLeftShift,
        ] {
            assert_eq!(tune_event(&t, &mut st, key(k, 1)), Some(key(k, 1)));
            assert_eq!(tune_event(&t, &mut st, key(k, 0)), Some(key(k, 0)));
        }
        assert_eq!(tune_event(&t, &mut st, motion(3.0, -2.0)), Some(motion(3.0, -2.0)));
        assert!(st.held.is_empty());
    }

    #[test]
    fn swaps_ctrl_and_meta_preserving_side() {
        let map = swap_ctrl_meta();
        assert_eq!(
            map_key(&map, Linux::KeyLeftCtrl as u32),
            Linux::KeyLeftMeta as u32
        );
        assert_eq!(
            map_key(&map, Linux::KeyRightCtrl as u32),
            Linux::KeyRightmeta as u32
        );
        assert_eq!(
            map_key(&map, Linux::KeyLeftMeta as u32),
            Linux::KeyLeftCtrl as u32
        );
        assert_eq!(
            map_key(&map, Linux::KeyRightmeta as u32),
            Linux::KeyRightCtrl as u32
        );
        // alt untouched, ordinary keys untouched
        assert_eq!(
            map_key(&map, Linux::KeyLeftAlt as u32),
            Linux::KeyLeftAlt as u32
        );
        assert_eq!(map_key(&map, Linux::KeyC as u32), Linux::KeyC as u32);

        // and the same through the stateful path, with an ordinary key in between
        assert_eq!(
            wire(
                map,
                &[
                    (Linux::KeyLeftCtrl, 1),
                    (Linux::KeyC, 1),
                    (Linux::KeyC, 0),
                    (Linux::KeyLeftCtrl, 0)
                ]
            ),
            vec![
                (Linux::KeyLeftMeta, 1),
                (Linux::KeyC, 1),
                (Linux::KeyC, 0),
                (Linux::KeyLeftMeta, 0)
            ]
        );
    }

    #[test]
    fn collapsed_target_is_released_only_when_both_keys_are_up() {
        // "both Ctrl and Cmd act as Ctrl": hold Cmd, tap Ctrl, keep
        // holding Cmd — the peer must keep Ctrl down throughout
        let out = wire(
            both_ctrl(),
            &[
                (Linux::KeyLeftMeta, 1),
                (Linux::KeyLeftCtrl, 1),
                (Linux::KeyLeftCtrl, 0),
                (Linux::KeyS, 1),
                (Linux::KeyS, 0),
                (Linux::KeyLeftMeta, 0),
            ],
        );
        assert_eq!(
            out,
            vec![
                (Linux::KeyLeftCtrl, 1),
                (Linux::KeyS, 1),
                (Linux::KeyS, 0),
                (Linux::KeyLeftCtrl, 0),
            ]
        );
        assert_eq!(map_mods(&both_ctrl(), MOD4_MASK), CONTROL_MASK);
        assert_eq!(map_mods(&both_ctrl(), MOD4_MASK | CONTROL_MASK), CONTROL_MASK);
    }

    #[test]
    fn auto_repeat_of_a_held_modifier_keeps_its_code() {
        // Windows capture repeats WM_KEYDOWN for held modifiers
        let out = wire(
            swap_ctrl_meta(),
            &[
                (Linux::KeyLeftCtrl, 1),
                (Linux::KeyLeftCtrl, 1),
                (Linux::KeyLeftCtrl, 1),
                (Linux::KeyLeftCtrl, 0),
            ],
        );
        assert_eq!(
            out,
            vec![
                (Linux::KeyLeftMeta, 1),
                (Linux::KeyLeftMeta, 1),
                (Linux::KeyLeftMeta, 1),
                (Linux::KeyLeftMeta, 0),
            ]
        );
    }

    #[test]
    fn map_change_while_held_releases_what_the_peer_saw() {
        let mut st = TuningState::default();
        let before = ClientTuning::new(1.0, both_ctrl());
        // Cmd goes down as Ctrl
        assert_eq!(
            tune_event(&before, &mut st, key(Linux::KeyLeftMeta, 1)),
            Some(key(Linux::KeyLeftCtrl, 1))
        );
        // user flips the map back to identity mid-hold
        let after = ClientTuning::default();
        // the release still targets the code that went down
        assert_eq!(
            tune_event(&after, &mut st, key(Linux::KeyLeftMeta, 0)),
            Some(key(Linux::KeyLeftCtrl, 0))
        );
        assert!(st.held.is_empty());

        // same guarantee for the capture-release path
        let mut st = TuningState::default();
        tune_event(&before, &mut st, key(Linux::KeyLeftMeta, 1));
        assert_eq!(
            st.release_code(&after.modifiers, Linux::KeyLeftMeta as u32),
            Linux::KeyLeftCtrl as u32
        );
        // a key we never saw go down falls back to the current map
        assert_eq!(
            st.release_code(&after.modifiers, Linux::KeyLeftAlt as u32),
            Linux::KeyLeftAlt as u32
        );
    }

    #[test]
    fn drain_held_reports_each_peer_code_once() {
        let mut st = TuningState::default();
        let t = ClientTuning::new(1.0, both_ctrl());
        tune_event(&t, &mut st, key(Linux::KeyLeftMeta, 1));
        tune_event(&t, &mut st, key(Linux::KeyLeftCtrl, 1));
        tune_event(&t, &mut st, key(Linux::KeyLeftAlt, 1));
        let mut codes = st.drain_held();
        codes.sort_unstable();
        assert_eq!(
            codes,
            vec![Linux::KeyLeftCtrl as u32, Linux::KeyLeftAlt as u32]
        );
        assert!(st.held.is_empty());
    }

    #[test]
    fn remaps_modifier_bitmask_and_keeps_other_bits() {
        let map = swap_ctrl_meta();
        let mods = CONTROL_MASK | SHIFT_MASK | LOCK_MASK;
        assert_eq!(map_mods(&map, mods), MOD4_MASK | SHIFT_MASK | LOCK_MASK);
        assert_eq!(map_mods(&map, MOD1_MASK), MOD1_MASK);
        assert_eq!(map_mods(&map, 0), 0);

        let t = ClientTuning::new(1.0, map);
        let mut st = TuningState::default();
        let ev = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: CONTROL_MASK,
            latched: 0,
            locked: LOCK_MASK,
            group: 3,
        });
        assert_eq!(
            tune_event(&t, &mut st, ev),
            Some(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: MOD4_MASK,
                latched: 0,
                locked: LOCK_MASK,
                group: 3,
            }))
        );
    }

    #[test]
    fn speed_scales_motion_without_losing_fractions() {
        let t = ClientTuning::new(0.5, ModifierMap::default());
        let mut st = TuningState::default();
        let mut total = 0.0;
        // twenty 1px moves at half speed must add up to 10px, not 0
        for _ in 0..20 {
            match tune_event(&t, &mut st, motion(1.0, 0.0)) {
                Some(Event::Pointer(PointerEvent::Motion { dx, .. })) => {
                    assert_eq!(dx.fract(), 0.0, "sent deltas are whole pixels");
                    total += dx;
                }
                other => panic!("motion must stay motion: {other:?}"),
            }
        }
        assert_eq!(total, 10.0);
    }

    #[test]
    fn speed_scales_up_and_handles_negative_motion() {
        let t = ClientTuning::new(2.5, ModifierMap::default());
        let mut st = TuningState::default();
        assert_eq!(
            tune_event(&t, &mut st, motion(-3.0, 1.0)),
            Some(motion(-7.0, 2.0))
        );
        // the 0.5 remainders carry over
        assert_eq!(
            tune_event(&t, &mut st, motion(-3.0, 1.0)),
            Some(motion(-8.0, 3.0))
        );
        st.reset();
        assert_eq!(
            tune_event(&t, &mut st, motion(-3.0, 1.0)),
            Some(motion(-7.0, 2.0))
        );
    }

    #[test]
    fn speed_is_clamped() {
        assert_eq!(ClientTuning::new(0.0, ModifierMap::default()).speed, 0.25);
        assert_eq!(ClientTuning::new(99.0, ModifierMap::default()).speed, 4.0);
        assert_eq!(ClientTuning::new(f64::NAN, ModifierMap::default()).speed, 1.0);
    }

    #[test]
    fn modifier_map_toml_roundtrip_and_partial() {
        let map = swap_ctrl_meta();
        let s = toml::to_string(&map).unwrap();
        assert_eq!(toml::from_str::<ModifierMap>(&s).unwrap(), map);
        // missing keys fall back to identity for that key
        let partial: ModifierMap = toml::from_str("meta = \"ctrl\"").unwrap();
        assert_eq!(partial.ctrl, Modifier::Ctrl);
        assert_eq!(partial.alt, Modifier::Alt);
        assert_eq!(partial.meta, Modifier::Ctrl);
        assert!(!partial.is_identity());
        assert!(ModifierMap::default().is_identity());
    }
}
