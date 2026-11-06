//! Чем становится ⌘ на сервере (только macOS; на Windows клавиши идут как есть).
//!
//! От Mac-источника ⌘ приходит скан-кодом Win. В «умном» режиме решение
//! откладывается до следующей клавиши: ⌘+буква/цифра/знак → Ctrl (копировать,
//! вставить, сохранить), ⌘ с остальными (Tab, пробел, стрелки…) → Win, а
//! одиночное нажатие ⌘ — нажатие Win (меню «Пуск»).
//!
//! Отдельно, на обеих ОС: Caps Lock можно превратить в Ctrl+Shift
//! (переключение языка в Windows).

use std::collections::HashMap;

use smallvec::SmallVec;

use super::scancode::{BACKSPACE, CAPS_LOCK, LCTRL, LSHIFT, LWIN, RCTRL, RWIN, Scancode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdMode {
    /// Без переназначения (Windows-клиент).
    Native,
    Smart,
    Win,
    Ctrl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emit {
    pub sc: Scancode,
    pub down: bool,
    /// Автоповтор зажатой клавиши (уже нажатой на сервере).
    pub repeat: bool,
}

impl Emit {
    fn down(sc: Scancode) -> Self {
        Self {
            sc,
            down: true,
            repeat: false,
        }
    }

    fn up(sc: Scancode) -> Self {
        Self {
            sc,
            down: false,
            repeat: false,
        }
    }
}

pub type Emits = SmallVec<[Emit; 4]>;

/// Что переназначать.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remap {
    pub cmd: CmdMode,
    /// Caps Lock → Ctrl+Shift.
    pub caps_to_ctrl_shift: bool,
}

/// Клавиши сервера, которые держит одна исходная.
type Keys = SmallVec<[Scancode; 2]>;

#[derive(Default)]
pub struct Remapper {
    /// Во что превратилась каждая зажатая исходная клавиша.
    mapped: HashMap<Scancode, Keys>,
    /// Зажатые ⌘, для которых ещё не решено, чем они станут.
    pending: Vec<Scancode>,
}

/// ⌘ с этими клавишами — сочетание приложения (⌘C, ⌘S…), то есть Ctrl.
fn is_shortcut_key(sc: Scancode) -> bool {
    !sc.is_extended()
        && (matches!(sc.code(), 0x02..=0x0D | 0x10..=0x1B | 0x1E..=0x29 | 0x2B..=0x35 | 0x56)
            || sc == BACKSPACE)
}

fn as_ctrl(cmd: Scancode) -> Scancode {
    if cmd == RWIN { RCTRL } else { LCTRL }
}

impl Remapper {
    /// `lone_tap` — одиночный ⌘ можно отправить как Win: системные сочетания
    /// перехватываются, и ⌘ не мог уйти в Spotlight или переключатель окон.
    pub fn down(&mut self, sc: Scancode, repeat: bool, remap: Remap) -> Emits {
        let mut out = Emits::new();
        if repeat {
            // Автоповтор у сочетания (Caps → Ctrl+Shift) не нужен.
            if let Some([to]) = self.mapped.get(&sc).map(|k| k.as_slice()) {
                out.push(Emit {
                    sc: *to,
                    down: true,
                    repeat: true,
                });
            }
            return out;
        }
        // Ведёт себя как модификаторы: решения о ⌘ не принимает.
        if remap.caps_to_ctrl_shift && sc == CAPS_LOCK {
            let keys: Keys = [LCTRL, LSHIFT].into_iter().collect();
            out.extend(keys.iter().map(|&k| Emit::down(k)));
            self.mapped.insert(sc, keys);
            return out;
        }
        let mode = remap.cmd;
        let cmd = sc == LWIN || sc == RWIN;
        let to = match mode {
            CmdMode::Native => sc,
            CmdMode::Ctrl if cmd => as_ctrl(sc),
            CmdMode::Ctrl => sc,
            CmdMode::Smart | CmdMode::Win if cmd => {
                // Второй ⌘, когда первый уже определился, ведёт себя так же.
                match self.mapped.iter().find(|(k, _)| **k == LWIN || **k == RWIN) {
                    Some((_, to)) if to[0].is_ctrl() => as_ctrl(sc),
                    Some(_) => sc,
                    None => {
                        self.pending.push(sc);
                        return out;
                    }
                }
            }
            // Shift/Alt/Ctrl не решают судьбу ⌘: ⌘⇧Z — это Ctrl+Shift+Z.
            CmdMode::Smart | CmdMode::Win if sc.is_modifier() => sc,
            CmdMode::Smart | CmdMode::Win => {
                let ctrl = mode == CmdMode::Smart && is_shortcut_key(sc);
                for cmd in std::mem::take(&mut self.pending) {
                    let to = if ctrl { as_ctrl(cmd) } else { cmd };
                    self.mapped.insert(cmd, Keys::from_elem(to, 1));
                    out.push(Emit::down(to));
                }
                sc
            }
        };
        self.mapped.insert(sc, Keys::from_elem(to, 1));
        out.push(Emit::down(to));
        out
    }

    pub fn up(&mut self, sc: Scancode, lone_tap: bool) -> Emits {
        let mut out = Emits::new();
        if let Some(i) = self.pending.iter().position(|&p| p == sc) {
            self.pending.remove(i);
            if lone_tap {
                out.push(Emit::down(sc));
                out.push(Emit::up(sc));
            }
        } else if let Some(keys) = self.mapped.remove(&sc) {
            out.extend(keys.into_iter().rev().map(Emit::up));
        }
        out
    }

    pub fn reset(&mut self) {
        self.mapped.clear();
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::scancode::{LSHIFT, TAB, parse_key};

    fn key(n: &str) -> Scancode {
        parse_key(n).unwrap()
    }

    fn cmd(mode: CmdMode) -> Remap {
        Remap {
            cmd: mode,
            caps_to_ctrl_shift: false,
        }
    }

    fn run(mode: CmdMode, seq: &[(Scancode, bool)]) -> Vec<(Scancode, bool)> {
        let mut r = Remapper::default();
        let mut out = Vec::new();
        for &(sc, down) in seq {
            let e = if down {
                r.down(sc, false, cmd(mode))
            } else {
                r.up(sc, true)
            };
            out.extend(e.into_iter().map(|e| (e.sc, e.down)));
        }
        out
    }

    #[test]
    fn smart_cmd_letter_is_ctrl() {
        let c = key("c");
        let out = run(
            CmdMode::Smart,
            &[(LWIN, true), (c, true), (c, false), (LWIN, false)],
        );
        assert_eq!(
            out,
            vec![(LCTRL, true), (c, true), (c, false), (LCTRL, false)]
        );
    }

    #[test]
    fn smart_cmd_tab_is_win_and_lone_cmd_taps_win() {
        let out = run(
            CmdMode::Smart,
            &[(LWIN, true), (TAB, true), (TAB, false), (LWIN, false)],
        );
        assert_eq!(
            out,
            vec![(LWIN, true), (TAB, true), (TAB, false), (LWIN, false)]
        );
        let out = run(CmdMode::Smart, &[(RWIN, true), (RWIN, false)]);
        assert_eq!(out, vec![(RWIN, true), (RWIN, false)]);
    }

    #[test]
    fn lone_cmd_without_grab_sends_nothing() {
        let mut r = Remapper::default();
        assert!(r.down(LWIN, false, cmd(CmdMode::Smart)).is_empty());
        assert!(r.up(LWIN, false).is_empty());
    }

    #[test]
    fn modifiers_before_decision_dont_decide() {
        let z = key("z");
        let out = run(
            CmdMode::Smart,
            &[
                (LWIN, true),
                (LSHIFT, true),
                (z, true),
                (z, false),
                (LSHIFT, false),
                (LWIN, false),
            ],
        );
        assert_eq!(
            out,
            vec![
                (LSHIFT, true),
                (LCTRL, true),
                (z, true),
                (z, false),
                (LSHIFT, false),
                (LCTRL, false)
            ]
        );
    }

    #[test]
    fn cmd_ctrl_and_win_modes() {
        let c = key("c");
        assert_eq!(
            run(CmdMode::Ctrl, &[(RWIN, true), (RWIN, false)]),
            vec![(RCTRL, true), (RCTRL, false)]
        );
        assert_eq!(
            run(
                CmdMode::Win,
                &[(LWIN, true), (c, true), (c, false), (LWIN, false)]
            ),
            vec![(LWIN, true), (c, true), (c, false), (LWIN, false)]
        );
        assert_eq!(
            run(CmdMode::Native, &[(LWIN, true), (LWIN, false)]),
            vec![(LWIN, true), (LWIN, false)]
        );
    }

    #[test]
    fn repeat_goes_to_mapped_key() {
        let mut r = Remapper::default();
        r.down(LWIN, false, cmd(CmdMode::Smart));
        r.down(key("v"), false, cmd(CmdMode::Smart));
        let rep = r.down(LWIN, true, cmd(CmdMode::Smart));
        assert_eq!(rep[0].sc, LCTRL);
        assert!(rep[0].repeat);
    }

    #[test]
    fn caps_lock_becomes_ctrl_shift() {
        let remap = Remap {
            cmd: CmdMode::Native,
            caps_to_ctrl_shift: true,
        };
        let mut r = Remapper::default();
        let down: Vec<_> = r
            .down(CAPS_LOCK, false, remap)
            .iter()
            .map(|e| (e.sc, e.down))
            .collect();
        assert_eq!(down, vec![(LCTRL, true), (LSHIFT, true)]);
        assert!(r.down(CAPS_LOCK, true, remap).is_empty(), "без автоповтора");
        let up: Vec<_> = r
            .up(CAPS_LOCK, true)
            .iter()
            .map(|e| (e.sc, e.down))
            .collect();
        assert_eq!(up, vec![(LSHIFT, false), (LCTRL, false)]);
        // Выключено — Caps Lock как есть.
        let off = r.down(CAPS_LOCK, false, cmd(CmdMode::Native));
        assert_eq!(off[0].sc, CAPS_LOCK);
    }
}
