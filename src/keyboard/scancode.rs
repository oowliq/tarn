//! Скан-коды RDP (набор 1, как у PC-клавиатуры) и их имена.
//!
//! На сервер уходит физическая клавиша, а символ определяет раскладка
//! сервера — так же делает mstsc. Имена клавиш нужны инструментам MCP
//! (`press_keys`).

use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags, SynchronizeFlags};
use smallvec::SmallVec;

pub type Events = SmallVec<[FastPathInputEvent; 2]>;

/// Скан-код с префиксом: `0x1D` — левый Ctrl, `0xE01D` — правый,
/// `0xE11D` — Pause (единственная клавиша с префиксом E1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Scancode(u16);

impl Scancode {
    pub const fn new(code: u8) -> Self {
        Self(code as u16)
    }

    pub const fn ext(code: u8) -> Self {
        Self(0xE000 | code as u16)
    }

    #[cfg(windows)]
    pub const fn from_parts(code: u8, extended: bool) -> Self {
        if extended {
            Self::ext(code)
        } else {
            Self::new(code)
        }
    }

    pub const fn code(self) -> u8 {
        self.0 as u8
    }

    pub const fn is_extended(self) -> bool {
        self.0 >> 8 == 0xE0
    }

    /// События fast-path для нажатия/отпускания.
    pub fn events(self, down: bool) -> Events {
        let mut out = Events::new();
        if self == PAUSE {
            // Pause не имеет отпускания: при нажатии сразу уходит вся
            // последовательность E1 1D 45 E1 9D C5.
            if down {
                for flags in [KeyboardFlags::empty(), KeyboardFlags::RELEASE] {
                    out.push(FastPathInputEvent::KeyboardEvent(
                        flags | KeyboardFlags::EXTENDED1,
                        0x1D,
                    ));
                    out.push(FastPathInputEvent::KeyboardEvent(flags, 0x45));
                }
            }
            return out;
        }
        let mut flags = KeyboardFlags::empty();
        if !down {
            flags |= KeyboardFlags::RELEASE;
        }
        if self.is_extended() {
            flags |= KeyboardFlags::EXTENDED;
        }
        out.push(FastPathInputEvent::KeyboardEvent(flags, self.code()));
        out
    }

    pub fn is_ctrl(self) -> bool {
        self == LCTRL || self == RCTRL
    }

    pub fn is_alt(self) -> bool {
        self == LALT || self == RALT
    }

    pub fn is_win(self) -> bool {
        self == LWIN || self == RWIN
    }

    pub fn is_modifier(self) -> bool {
        self.is_ctrl() || self.is_alt() || self.is_win() || self == LSHIFT || self == RSHIFT
    }
}

pub const ESC: Scancode = Scancode::new(0x01);
pub const BACKSPACE: Scancode = Scancode::new(0x0E);
pub const TAB: Scancode = Scancode::new(0x0F);
pub const ENTER: Scancode = Scancode::new(0x1C);
pub const NUM_ENTER: Scancode = Scancode::ext(0x1C);
pub const LCTRL: Scancode = Scancode::new(0x1D);
pub const RCTRL: Scancode = Scancode::ext(0x1D);
pub const LSHIFT: Scancode = Scancode::new(0x2A);
pub const RSHIFT: Scancode = Scancode::new(0x36);
pub const LALT: Scancode = Scancode::new(0x38);
pub const RALT: Scancode = Scancode::ext(0x38);
pub const SPACE: Scancode = Scancode::new(0x39);
pub const CAPS_LOCK: Scancode = Scancode::new(0x3A);
pub const HOME: Scancode = Scancode::ext(0x47);
pub const END: Scancode = Scancode::ext(0x4F);
pub const DELETE: Scancode = Scancode::ext(0x53);
pub const LWIN: Scancode = Scancode::ext(0x5B);
pub const RWIN: Scancode = Scancode::ext(0x5C);
pub const PAUSE: Scancode = Scancode(0xE11D);

/// Имена клавиш (в нижнем регистре); первое имя — основное.
const KEYS: &[(&[&str], Scancode)] = &[
    (&["esc", "escape"], ESC),
    (&["1"], Scancode::new(0x02)),
    (&["2"], Scancode::new(0x03)),
    (&["3"], Scancode::new(0x04)),
    (&["4"], Scancode::new(0x05)),
    (&["5"], Scancode::new(0x06)),
    (&["6"], Scancode::new(0x07)),
    (&["7"], Scancode::new(0x08)),
    (&["8"], Scancode::new(0x09)),
    (&["9"], Scancode::new(0x0A)),
    (&["0"], Scancode::new(0x0B)),
    (&["-", "minus"], Scancode::new(0x0C)),
    (&["=", "equal", "equals"], Scancode::new(0x0D)),
    (&["backspace"], BACKSPACE),
    (&["tab"], TAB),
    (&["q"], Scancode::new(0x10)),
    (&["w"], Scancode::new(0x11)),
    (&["e"], Scancode::new(0x12)),
    (&["r"], Scancode::new(0x13)),
    (&["t"], Scancode::new(0x14)),
    (&["y"], Scancode::new(0x15)),
    (&["u"], Scancode::new(0x16)),
    (&["i"], Scancode::new(0x17)),
    (&["o"], Scancode::new(0x18)),
    (&["p"], Scancode::new(0x19)),
    (&["[", "bracketleft"], Scancode::new(0x1A)),
    (&["]", "bracketright"], Scancode::new(0x1B)),
    (&["enter", "return"], ENTER),
    (&["ctrl", "control", "lctrl"], LCTRL),
    (&["a"], Scancode::new(0x1E)),
    (&["s"], Scancode::new(0x1F)),
    (&["d"], Scancode::new(0x20)),
    (&["f"], Scancode::new(0x21)),
    (&["g"], Scancode::new(0x22)),
    (&["h"], Scancode::new(0x23)),
    (&["j"], Scancode::new(0x24)),
    (&["k"], Scancode::new(0x25)),
    (&["l"], Scancode::new(0x26)),
    (&[";", "semicolon"], Scancode::new(0x27)),
    (&["'", "quote"], Scancode::new(0x28)),
    (&["`", "backquote", "grave"], Scancode::new(0x29)),
    (&["shift", "lshift"], LSHIFT),
    (&["\\", "backslash"], Scancode::new(0x2B)),
    (&["z"], Scancode::new(0x2C)),
    (&["x"], Scancode::new(0x2D)),
    (&["c"], Scancode::new(0x2E)),
    (&["v"], Scancode::new(0x2F)),
    (&["b"], Scancode::new(0x30)),
    (&["n"], Scancode::new(0x31)),
    (&["m"], Scancode::new(0x32)),
    (&[",", "comma"], Scancode::new(0x33)),
    (&[".", "period"], Scancode::new(0x34)),
    (&["/", "slash"], Scancode::new(0x35)),
    (&["rshift"], RSHIFT),
    (&["num*", "nummultiply"], Scancode::new(0x37)),
    (&["alt", "lalt", "option"], LALT),
    (&["space"], SPACE),
    (&["capslock"], CAPS_LOCK),
    (&["f1"], Scancode::new(0x3B)),
    (&["f2"], Scancode::new(0x3C)),
    (&["f3"], Scancode::new(0x3D)),
    (&["f4"], Scancode::new(0x3E)),
    (&["f5"], Scancode::new(0x3F)),
    (&["f6"], Scancode::new(0x40)),
    (&["f7"], Scancode::new(0x41)),
    (&["f8"], Scancode::new(0x42)),
    (&["f9"], Scancode::new(0x43)),
    (&["f10"], Scancode::new(0x44)),
    (&["numlock"], Scancode::new(0x45)),
    (&["scrolllock"], Scancode::new(0x46)),
    (&["num7"], Scancode::new(0x47)),
    (&["num8"], Scancode::new(0x48)),
    (&["num9"], Scancode::new(0x49)),
    (&["num-", "numminus"], Scancode::new(0x4A)),
    (&["num4"], Scancode::new(0x4B)),
    (&["num5"], Scancode::new(0x4C)),
    (&["num6"], Scancode::new(0x4D)),
    (&["num+", "numplus"], Scancode::new(0x4E)),
    (&["num1"], Scancode::new(0x4F)),
    (&["num2"], Scancode::new(0x50)),
    (&["num3"], Scancode::new(0x51)),
    (&["num0"], Scancode::new(0x52)),
    (&["num.", "numdecimal"], Scancode::new(0x53)),
    (&["intlbackslash", "oem102"], Scancode::new(0x56)),
    (&["f11"], Scancode::new(0x57)),
    (&["f12"], Scancode::new(0x58)),
    (&["f13"], Scancode::new(0x64)),
    (&["f14"], Scancode::new(0x65)),
    (&["f15"], Scancode::new(0x66)),
    (&["f16"], Scancode::new(0x67)),
    (&["f17"], Scancode::new(0x68)),
    (&["f18"], Scancode::new(0x69)),
    (&["f19"], Scancode::new(0x6A)),
    (&["f20"], Scancode::new(0x6B)),
    (&["f21"], Scancode::new(0x6C)),
    (&["f22"], Scancode::new(0x6D)),
    (&["f23"], Scancode::new(0x6E)),
    (&["f24"], Scancode::new(0x76)),
    (&["numenter"], NUM_ENTER),
    (&["rctrl"], RCTRL),
    (&["mute", "volumemute"], Scancode::ext(0x20)),
    (&["volumedown"], Scancode::ext(0x2E)),
    (&["volumeup"], Scancode::ext(0x30)),
    (&["num/", "numdivide"], Scancode::ext(0x35)),
    (&["printscreen", "prtsc", "print"], Scancode::ext(0x37)),
    (&["ralt", "altgr"], RALT),
    (&["pause", "break"], PAUSE),
    (&["home"], HOME),
    (&["up"], Scancode::ext(0x48)),
    (&["pageup", "pgup"], Scancode::ext(0x49)),
    (&["left"], Scancode::ext(0x4B)),
    (&["right"], Scancode::ext(0x4D)),
    (&["end"], END),
    (&["down"], Scancode::ext(0x50)),
    (&["pagedown", "pgdn"], Scancode::ext(0x51)),
    (&["insert", "ins"], Scancode::ext(0x52)),
    (&["delete", "del"], DELETE),
    (&["win", "lwin", "meta", "super", "cmd", "command"], LWIN),
    (&["rwin"], RWIN),
    (&["menu", "apps", "contextmenu"], Scancode::ext(0x5D)),
];

/// Клавиша по имени, без учёта регистра.
pub fn parse_key(name: &str) -> Option<Scancode> {
    let name = name.trim().to_ascii_lowercase();
    KEYS.iter()
        .find(|(names, _)| names.contains(&name.as_str()))
        .map(|&(_, sc)| sc)
}

/// Сочетание вида `ctrl+shift+esc`: клавиши в порядке нажатия.
pub fn parse_combo(combo: &str) -> Result<Vec<Scancode>, String> {
    let mut keys = Vec::new();
    for part in combo.split('+') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!(
                "Пустая клавиша в «{combo}». Клавиши разделяются «+», сама клавиша «+» — num+ или shift+="
            ));
        }
        let sc = parse_key(part).ok_or_else(|| {
            format!(
                "Неизвестная клавиша «{part}». Допустимые имена: {}",
                key_names()
            )
        })?;
        if keys.contains(&sc) {
            return Err(format!("Клавиша «{part}» указана дважды"));
        }
        keys.push(sc);
    }
    if keys.len() > 6 {
        return Err("Не больше 6 клавиш в одном сочетании".into());
    }
    Ok(keys)
}

/// Основные имена всех клавиш — для сообщений об ошибке.
pub fn key_names() -> String {
    let names: Vec<&str> = KEYS.iter().map(|(names, _)| names[0]).collect();
    names.join(", ")
}

/// Состояние Caps/Num/Scroll Lock для события синхронизации.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LockKeys {
    pub caps: bool,
    pub num: bool,
    pub scroll: bool,
}

impl LockKeys {
    pub fn event(self) -> FastPathInputEvent {
        let mut f = SynchronizeFlags::empty();
        f.set(SynchronizeFlags::CAPS_LOCK, self.caps);
        f.set(SynchronizeFlags::NUM_LOCK, self.num);
        f.set(SynchronizeFlags::SCROLL_LOCK, self.scroll);
        FastPathInputEvent::SyncEvent(f)
    }
}

/// Символ текста в виде Unicode-событий (для `type_text`): не зависит от
/// раскладки ни клиента, ни сервера.
pub fn unicode_events(unit: u16, down: bool) -> Events {
    let flags = if down {
        KeyboardFlags::empty()
    } else {
        KeyboardFlags::RELEASE
    };
    SmallVec::from_elem(FastPathInputEvent::UnicodeKeyboardEvent(flags, unit), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_parse() {
        let mut seen = std::collections::HashSet::new();
        for (names, _) in KEYS {
            for n in *names {
                assert!(seen.insert(*n), "имя {n} повторяется");
                assert_eq!(n.to_ascii_lowercase(), *n);
            }
        }
        assert_eq!(parse_key("F15"), Some(Scancode::new(0x66)));
        assert_eq!(parse_key(" Ctrl "), Some(LCTRL));
        assert_eq!(parse_key("nope"), None);
    }

    #[test]
    fn combo_parsing() {
        assert_eq!(
            parse_combo("ctrl+shift+esc").unwrap(),
            vec![LCTRL, LSHIFT, ESC]
        );
        assert!(parse_combo("ctrl++").is_err());
        assert!(parse_combo("ctrl+ctrl").is_err());
        let err = parse_combo("ctrl+щ").unwrap_err();
        assert!(err.contains("Допустимые"), "{err}");
    }

    #[test]
    fn extended_and_release_flags() {
        let ev = RCTRL.events(false);
        assert!(matches!(
            ev[0],
            FastPathInputEvent::KeyboardEvent(f, 0x1D)
                if f.contains(KeyboardFlags::RELEASE) && f.contains(KeyboardFlags::EXTENDED)
        ));
        assert_eq!(PAUSE.events(true).len(), 4);
        assert!(PAUSE.events(false).is_empty());
        assert!(!PAUSE.is_extended());
    }
}
