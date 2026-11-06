//! Клавиатура: перехват нажатий ОС и передача на сервер скан-кодами.
//!
//! Устройство:
//! - платформенный бэкенд (`macos.rs`, `windows.rs`) перехватывает события
//!   ОС в двух местах: обычные — пока окно в фокусе, и глобально — системные
//!   сочетания, если включён их перехват;
//! - [`router::Router`] решает, уходит ли событие на сервер, переназначает ⌘
//!   на Mac ([`remap`]), ловит локальные хоткеи и держит сервер в
//!   сбалансированном состоянии;
//! - UI ([`Keyboard`]) сообщает, когда клавиатура принадлежит сессии, и
//!   получает команды хоткеев.
//!
//! Перехватчики работают в потоках ОС, а не в UI-цикле: ввод не ждёт кадра.

#[cfg(any(target_os = "macos", test))]
mod mac_keymap;
#[cfg(target_os = "macos")]
mod macos;
mod remap;
mod router;
pub mod scancode;
#[cfg(windows)]
mod windows;

use std::sync::{Arc, mpsc};

use serde::{Deserialize, Serialize};

pub use router::{Command, Link};

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "macos", windows)))]
use unsupported as platform;
#[cfg(windows)]
use windows as platform;

use remap::{CmdMode, Remap};
use router::Router;

/// Чем на сервере становится ⌘ (только macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MacRemap {
    /// ⌘+буква → Ctrl+буква, остальное и одиночный ⌘ → Win.
    #[default]
    Smart,
    CmdIsWin,
    CmdIsCtrl,
}

impl MacRemap {
    pub const ALL: [Self; 3] = [Self::Smart, Self::CmdIsWin, Self::CmdIsCtrl];

    pub fn label(self) -> &'static str {
        match self {
            Self::Smart => "Умно: ⌘C/⌘V → Ctrl, ⌘ и ⌘Tab → Win",
            Self::CmdIsWin => "⌘ = Win",
            Self::CmdIsCtrl => "⌘ = Ctrl",
        }
    }
}

/// Что делает Caps Lock на сервере.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CapsLockMode {
    #[default]
    CapsLock,
    /// Ctrl+Shift — переключение языка в Windows.
    CtrlShift,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardSettings {
    /// Перехватывать системные сочетания (Win, Alt+Tab, ⌘Tab…) и отправлять на сервер.
    pub grab_system_keys: bool,
    #[serde(deserialize_with = "crate::config::lenient")]
    pub mac_remap: MacRemap,
    #[serde(deserialize_with = "crate::config::lenient")]
    pub caps_lock: CapsLockMode,
}

impl Default for KeyboardSettings {
    fn default() -> Self {
        Self {
            grab_system_keys: true,
            mac_remap: MacRemap::Smart,
            caps_lock: CapsLockMode::CapsLock,
        }
    }
}

/// Состояние перехвата системных сочетаний — для окна настроек.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalStatus {
    Off,
    Starting,
    Active,
    /// macOS: нужен «Универсальный доступ».
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    NeedsPermission,
    /// Не удалось поставить перехватчик (на macOS он переподключается сам).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Failed,
}

pub struct Keyboard {
    router: Arc<Router>,
    commands: mpsc::Receiver<Command>,
    backend: platform::Backend,
}

impl Keyboard {
    /// Создавать в главном потоке, когда окно уже есть.
    pub fn new(cc: &eframe::CreationContext<'_>, settings: &KeyboardSettings) -> Self {
        let (tx, commands) = mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        let router = Arc::new(Router::new(tx, move || ctx.request_repaint()));
        let backend = platform::Backend::start(cc, router.clone());
        let kb = Self {
            router,
            commands,
            backend,
        };
        kb.apply(settings);
        kb
    }

    pub fn apply(&self, s: &KeyboardSettings) {
        let cmd = if cfg!(target_os = "macos") {
            match s.mac_remap {
                MacRemap::Smart => CmdMode::Smart,
                MacRemap::CmdIsWin => CmdMode::Win,
                MacRemap::CmdIsCtrl => CmdMode::Ctrl,
            }
        } else {
            CmdMode::Native
        };
        let caps = s.caps_lock == CapsLockMode::CtrlShift;
        self.router.set_remap(Remap {
            cmd,
            caps_to_ctrl_shift: caps,
        });
        self.router.set_grab(s.grab_system_keys);
        // На Windows Caps Lock забирает глобальный хук, иначе ОС успеет
        // переключить свой Caps Lock. На macOS хук это не предотвращает —
        // там бэкенд сам сбрасывает локальный Caps Lock.
        if s.grab_system_keys || (cfg!(windows) && caps) {
            self.backend.enable_global();
        }
    }

    /// Подключить к сессии (None — отключить).
    pub fn attach(&self, link: Option<Link>) {
        self.router.attach(link);
    }

    /// Клавиатура принадлежит сессии: окно в фокусе, сессия подключена,
    /// egui не ждёт ввода. Снятие отпускает всё нажатое на сервере.
    pub fn set_active(&self, active: bool) {
        self.router.set_active(active);
    }

    /// Выровнять Caps/Num/Scroll Lock сервера с локальными.
    pub fn sync_locks(&self) {
        self.router.sync(platform::lock_keys());
    }

    /// Захват снят хоткеем Ctrl+Alt+Home.
    pub fn suspended(&self) -> bool {
        self.router.suspended()
    }

    pub fn resume(&self) {
        self.router.resume();
    }

    pub fn send_ctrl_alt_del(&self) {
        self.router.send_ctrl_alt_del();
    }

    pub fn poll(&self) -> Option<Command> {
        self.commands.try_recv().ok()
    }

    pub fn global_status(&self) -> GlobalStatus {
        self.backend.status()
    }

    pub fn request_permission(&self) {
        self.backend.request_permission();
    }
}

/// Прочие ОС: клавиатура не перехватывается.
#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported {
    use std::sync::Arc;

    use super::GlobalStatus;
    use super::router::Router;
    use super::scancode::LockKeys;

    pub struct Backend;

    impl Backend {
        pub fn start(_cc: &eframe::CreationContext<'_>, _router: Arc<Router>) -> Self {
            tracing::warn!("клавиатура на этой ОС не поддерживается");
            Self
        }

        pub fn enable_global(&self) {}

        pub fn status(&self) -> GlobalStatus {
            GlobalStatus::Failed
        }

        pub fn request_permission(&self) {}
    }

    pub fn lock_keys() -> LockKeys {
        LockKeys::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_compat() {
        let s: KeyboardSettings =
            serde_json::from_str(r#"{"grab_system_keys":false,"mac_remap":"Smart"}"#).unwrap();
        assert!(!s.grab_system_keys);
        let s: KeyboardSettings = serde_json::from_str(r#"{"mac_remap":"Whatever"}"#).unwrap();
        assert_eq!(s, KeyboardSettings::default());
        let s: KeyboardSettings = serde_json::from_str(r#"{"mac_remap":"CmdIsCtrl"}"#).unwrap();
        assert_eq!(s.mac_remap, MacRemap::CmdIsCtrl);
        assert_eq!(s.caps_lock, CapsLockMode::CapsLock);
        let s: KeyboardSettings = serde_json::from_str(r#"{"caps_lock":"CtrlShift"}"#).unwrap();
        assert_eq!(s.caps_lock, CapsLockMode::CtrlShift);
    }
}
