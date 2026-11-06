//! Виртуальные коды клавиш macOS (`kVK_*`, по физическому положению) →
//! скан-коды PC и разбор событий keyDown/keyUp/flagsChanged.

use smallvec::SmallVec;

use super::router::KeyInput;
use super::scancode::Scancode;

const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
pub const FLAGS_CHANGED: u32 = 12;

pub const KEYCODE_CAPS_LOCK: u16 = 0x39;

/// Бит модификатора в «device-dependent» части флагов (NX_DEVICE*KEYMASK):
/// по нему видно, нажат или отпущен именно этот (левый/правый) модификатор.
fn modifier_mask(keycode: u16) -> Option<u64> {
    Some(match keycode {
        0x3B => 0x0001, // левый Control
        0x38 => 0x0002, // левый Shift
        0x3C => 0x0004, // правый Shift
        0x37 => 0x0008, // левый Command
        0x36 => 0x0010, // правый Command
        0x3A => 0x0020, // левый Option
        0x3D => 0x0040, // правый Option
        0x3E => 0x2000, // правый Control
        _ => return None,
    })
}

/// `iso` — клавиатура ISO: у неё macOS меняет местами коды клавиши слева от
/// «1» (§) и клавиши между левым Shift и Z.
pub fn scancode(keycode: u16, iso: bool) -> Option<Scancode> {
    let n = Scancode::new;
    let e = Scancode::ext;
    Some(match keycode {
        0x00 => n(0x1E),                          // A
        0x01 => n(0x1F),                          // S
        0x02 => n(0x20),                          // D
        0x03 => n(0x21),                          // F
        0x04 => n(0x23),                          // H
        0x05 => n(0x22),                          // G
        0x06 => n(0x2C),                          // Z
        0x07 => n(0x2D),                          // X
        0x08 => n(0x2E),                          // C
        0x09 => n(0x2F),                          // V
        0x0A => n(if iso { 0x29 } else { 0x56 }), // § (ISO)
        0x0B => n(0x30),                          // B
        0x0C => n(0x10),                          // Q
        0x0D => n(0x11),                          // W
        0x0E => n(0x12),                          // E
        0x0F => n(0x13),                          // R
        0x10 => n(0x15),                          // Y
        0x11 => n(0x14),                          // T
        0x12 => n(0x02),                          // 1
        0x13 => n(0x03),                          // 2
        0x14 => n(0x04),                          // 3
        0x15 => n(0x05),                          // 4
        0x16 => n(0x07),                          // 6
        0x17 => n(0x06),                          // 5
        0x18 => n(0x0D),                          // =
        0x19 => n(0x0A),                          // 9
        0x1A => n(0x08),                          // 7
        0x1B => n(0x0C),                          // -
        0x1C => n(0x09),                          // 8
        0x1D => n(0x0B),                          // 0
        0x1E => n(0x1B),                          // ]
        0x1F => n(0x18),                          // O
        0x20 => n(0x16),                          // U
        0x21 => n(0x1A),                          // [
        0x22 => n(0x17),                          // I
        0x23 => n(0x19),                          // P
        0x24 => n(0x1C),                          // Return
        0x25 => n(0x26),                          // L
        0x26 => n(0x24),                          // J
        0x27 => n(0x28),                          // '
        0x28 => n(0x25),                          // K
        0x29 => n(0x27),                          // ;
        0x2A => n(0x2B),                          // \
        0x2B => n(0x33),                          // ,
        0x2C => n(0x35),                          // /
        0x2D => n(0x31),                          // N
        0x2E => n(0x32),                          // M
        0x2F => n(0x34),                          // .
        0x30 => n(0x0F),                          // Tab
        0x31 => n(0x39),                          // Space
        0x32 => n(if iso { 0x56 } else { 0x29 }), // `
        0x33 => n(0x0E),                          // Delete (Backspace)
        0x35 => n(0x01),                          // Escape
        0x36 => e(0x5C),                          // правый Command → правый Win
        0x37 => e(0x5B),                          // Command → Win
        0x38 => n(0x2A),                          // Shift
        0x39 => n(0x3A),                          // Caps Lock
        0x3A => n(0x38),                          // Option → Alt
        0x3B => n(0x1D),                          // Control
        0x3C => n(0x36),                          // правый Shift
        0x3D => e(0x38),                          // правый Option → AltGr
        0x3E => e(0x1D),                          // правый Control
        0x40 => n(0x68),                          // F17
        0x41 => n(0x53),                          // Num .
        0x43 => n(0x37),                          // Num *
        0x45 => n(0x4E),                          // Num +
        0x47 => n(0x45),                          // Clear → Num Lock
        0x48 => e(0x30),                          // громче
        0x49 => e(0x2E),                          // тише
        0x4A => e(0x20),                          // без звука
        0x4B => e(0x35),                          // Num /
        0x4C => e(0x1C),                          // Num Enter
        0x4E => n(0x4A),                          // Num -
        0x4F => n(0x69),                          // F18
        0x50 => n(0x6A),                          // F19
        0x51 => n(0x59),                          // Num =
        0x52 => n(0x52),                          // Num 0
        0x53 => n(0x4F),                          // Num 1
        0x54 => n(0x50),                          // Num 2
        0x55 => n(0x51),                          // Num 3
        0x56 => n(0x4B),                          // Num 4
        0x57 => n(0x4C),                          // Num 5
        0x58 => n(0x4D),                          // Num 6
        0x59 => n(0x47),                          // Num 7
        0x5A => n(0x6B),                          // F20
        0x5B => n(0x48),                          // Num 8
        0x5C => n(0x49),                          // Num 9
        0x5D => n(0x7D),                          // JIS ¥
        0x5E => n(0x73),                          // JIS _
        0x5F => n(0x7E),                          // JIS Num ,
        0x60 => n(0x3F),                          // F5
        0x61 => n(0x40),                          // F6
        0x62 => n(0x41),                          // F7
        0x63 => n(0x3D),                          // F3
        0x64 => n(0x42),                          // F8
        0x65 => n(0x43),                          // F9
        0x67 => n(0x57),                          // F11
        0x69 => n(0x64),                          // F13
        0x6A => n(0x67),                          // F16
        0x6B => n(0x65),                          // F14
        0x6D => n(0x44),                          // F10
        0x6E => e(0x5D),                          // контекстное меню
        0x6F => n(0x58),                          // F12
        0x71 => n(0x66),                          // F15
        0x72 => e(0x52),                          // Help → Insert
        0x73 => e(0x47),                          // Home
        0x74 => e(0x49),                          // Page Up
        0x75 => e(0x53),                          // Forward Delete → Delete
        0x76 => n(0x3E),                          // F4
        0x77 => e(0x4F),                          // End
        0x78 => n(0x3C),                          // F2
        0x79 => e(0x51),                          // Page Down
        0x7A => n(0x3B),                          // F1
        0x7B => e(0x4B),                          // ←
        0x7C => e(0x4D),                          // →
        0x7D => e(0x50),                          // ↓
        0x7E => e(0x48),                          // ↑
        _ => return None,
    })
}

/// Событие macOS → нажатия. Caps Lock приходит одним flagsChanged на каждое
/// переключение, поэтому на сервер уходит полное нажатие.
pub fn decode(kind: u32, keycode: u16, flags: u64, iso: bool) -> SmallVec<[KeyInput; 2]> {
    let mut out = SmallVec::new();
    let Some(sc) = scancode(keycode, iso) else {
        return out;
    };
    match kind {
        KEY_DOWN => out.push(KeyInput::new(sc, true)),
        KEY_UP => out.push(KeyInput::new(sc, false)),
        FLAGS_CHANGED if keycode == KEYCODE_CAPS_LOCK => {
            out.push(KeyInput::new(sc, true));
            out.push(KeyInput::new(sc, false));
        }
        FLAGS_CHANGED => {
            if let Some(mask) = modifier_mask(keycode) {
                out.push(KeyInput::new(sc, flags & mask != 0));
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::scancode::{LALT, LWIN, RCTRL, parse_key};

    #[test]
    fn letters_follow_physical_position() {
        assert_eq!(scancode(0x00, false), parse_key("a"));
        assert_eq!(scancode(0x0C, false), parse_key("q"));
        assert_eq!(scancode(0x7E, false), parse_key("up"));
        assert_eq!(scancode(0x75, false), parse_key("delete"));
    }

    #[test]
    fn iso_swaps_section_and_grave() {
        assert_eq!(scancode(0x0A, true), parse_key("`"));
        assert_eq!(scancode(0x32, true), parse_key("intlbackslash"));
        assert_eq!(scancode(0x32, false), parse_key("`"));
    }

    #[test]
    fn modifiers_from_flags() {
        assert_eq!(
            decode(FLAGS_CHANGED, 0x37, 0x0010_0008, false).as_slice(),
            &[KeyInput::new(LWIN, true)]
        );
        assert_eq!(
            decode(FLAGS_CHANGED, 0x37, 0, false).as_slice(),
            &[KeyInput::new(LWIN, false)]
        );
        // Отпустили левый Option, правый ещё зажат.
        assert_eq!(
            decode(FLAGS_CHANGED, 0x3A, 0x0040, false).as_slice(),
            &[KeyInput::new(LALT, false)]
        );
        assert_eq!(
            decode(FLAGS_CHANGED, 0x3E, 0x2000, false).as_slice(),
            &[KeyInput::new(RCTRL, true)]
        );
        assert_eq!(decode(FLAGS_CHANGED, 0x39, 0, false).len(), 2);
        // Fn не передаётся.
        assert!(decode(FLAGS_CHANGED, 0x3F, 0x80_0000, false).is_empty());
    }
}
