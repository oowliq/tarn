//! Маршрутизатор: решает, уходит ли нажатие на сервер, и держит сервер в
//! сбалансированном состоянии.
//!
//! Источники событий (перехватчики ОС) вызывают [`Router::handle`] из своих
//! потоков; ответ `true` — событие обработано и ОС/egui его видеть не должны.
//! Правила:
//! - нажатие уходит на сервер, только пока идёт захват ([`Gate`]);
//! - отпускание клавиши, нажатой на сервере, уходит всегда, даже если захват
//!   уже снят, — иначе клавиша залипнет;
//! - одна клавиша на сервере может держаться несколькими исходными (⌘ как
//!   Ctrl и настоящий Ctrl), поэтому на сервере ведётся счётчик нажатий.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, mpsc};

use ironrdp_client::rdp::RdpInputEvent;
use tokio::sync::mpsc::UnboundedSender;

use super::remap::{CmdMode, Emit, Remap, Remapper};
use super::scancode::{
    CAPS_LOCK, DELETE, END, ENTER, Events, HOME, LALT, LCTRL, LockKeys, NUM_ENTER, Scancode,
};
use crate::keepalive::Activity;

/// Куда уходит ввод: канал активной сессии.
#[derive(Clone)]
pub struct Link {
    pub input: UnboundedSender<RdpInputEvent>,
    pub activity: Activity,
}

/// Событие от перехватчика ОС, уже в скан-кодах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    pub sc: Scancode,
    pub down: bool,
}

impl KeyInput {
    pub fn new(sc: Scancode, down: bool) -> Self {
        Self { sc, down }
    }
}

/// Команды для UI от локальных хоткеев.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    ToggleFullscreen,
    /// Захват снят (Ctrl+Alt+Home) до щелчка по экрану сессии.
    CaptureReleased,
}

/// Когда клавиатура принадлежит сессии. Флаги пишет UI, читают перехватчики.
#[derive(Default)]
pub struct Gate {
    /// Окно в фокусе, сессия подключена, egui не ждёт ввода текста.
    active: AtomicBool,
    /// Пользователь снял захват хоткеем.
    suspended: AtomicBool,
    /// Перехватывать системные сочетания (настройка).
    grab: AtomicBool,
    /// Глобальный перехватчик ОС установлен и работает.
    global_ready: AtomicBool,
}

impl Gate {
    pub fn capturing(&self) -> bool {
        self.active.load(Ordering::Acquire) && !self.suspended.load(Ordering::Acquire)
    }

    /// Глобальному перехватчику можно забирать нажатия.
    pub fn global_capturing(&self) -> bool {
        self.grab.load(Ordering::Acquire) && self.capturing()
    }

    pub fn grab(&self) -> bool {
        self.grab.load(Ordering::Acquire)
    }

    pub fn set_global_ready(&self, ready: bool) {
        self.global_ready.store(ready, Ordering::Release);
    }

    pub fn global_ready(&self) -> bool {
        self.global_ready.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct State {
    link: Option<Link>,
    /// Исходные клавиши, нажатие которых обработано нами.
    source: HashSet<Scancode>,
    /// Клавиши хоткеев: их нажатие и отпускание на сервер не идут.
    hotkey: HashSet<Scancode>,
    /// Сколько исходных клавиш держит каждую клавишу сервера.
    server: HashMap<Scancode, u8>,
    remap: Remapper,
}

pub struct Router {
    pub gate: Gate,
    state: Mutex<State>,
    mode: AtomicU8,
    /// Caps Lock → Ctrl+Shift.
    caps: AtomicBool,
    commands: mpsc::Sender<Command>,
    notify: Box<dyn Fn() + Send + Sync>,
}

const MODES: [CmdMode; 4] = [CmdMode::Native, CmdMode::Smart, CmdMode::Win, CmdMode::Ctrl];

impl Router {
    pub fn new(commands: mpsc::Sender<Command>, notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            gate: Gate::default(),
            state: Mutex::default(),
            mode: AtomicU8::new(0),
            caps: AtomicBool::new(false),
            commands,
            notify: Box::new(notify),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn remap(&self) -> Remap {
        Remap {
            cmd: MODES[usize::from(self.mode.load(Ordering::Acquire)).min(MODES.len() - 1)],
            caps_to_ctrl_shift: self.caps_remapped(),
        }
    }

    pub fn set_remap(&self, remap: Remap) {
        let i = MODES.iter().position(|&m| m == remap.cmd).unwrap_or(0);
        self.mode.store(i as u8, Ordering::Release);
        self.caps.store(remap.caps_to_ctrl_shift, Ordering::Release);
    }

    pub fn caps_remapped(&self) -> bool {
        self.caps.load(Ordering::Acquire)
    }

    /// Глобальному перехватчику забирать нажатие этой клавиши: всё — при
    /// перехвате системных сочетаний, Caps Lock — ещё и при его ремапе (чтобы
    /// ОС не переключала свой Caps Lock).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn global_wants(&self, sc: Scancode) -> bool {
        self.gate.capturing() && (self.gate.grab() || (sc == CAPS_LOCK && self.caps_remapped()))
    }

    pub fn set_grab(&self, grab: bool) {
        self.gate.grab.store(grab, Ordering::Release);
    }

    /// Окно в фокусе и клавиатура принадлежит сессии. При потере — всё отпускаем.
    pub fn set_active(&self, active: bool) {
        let was = self.gate.active.swap(active, Ordering::AcqRel);
        if was && !active {
            self.release_all();
        }
    }

    pub fn suspended(&self) -> bool {
        self.gate.suspended.load(Ordering::Acquire)
    }

    pub fn resume(&self) {
        self.gate.suspended.store(false, Ordering::Release);
    }

    /// Подключить к сессии (None — отключить). Старой сессии всё отпускаем.
    pub fn attach(&self, link: Option<Link>) {
        let mut st = self.state();
        if link.is_none() && st.link.is_none() {
            return;
        }
        st.release_all();
        st.link = link;
        self.gate.suspended.store(false, Ordering::Release);
    }

    /// Обработать событие перехватчика. `true` — событие наше, ОС его не отдавать.
    pub fn handle(&self, ev: KeyInput) -> bool {
        let capturing = self.gate.capturing();
        let remap = self.remap();
        let lone_tap = self.gate.grab() && self.gate.global_ready();
        let mut st = self.state();
        if st.link.is_none() {
            return false;
        }
        let held = st.source.contains(&ev.sc);
        if !ev.down {
            if st.hotkey.remove(&ev.sc) {
                return true;
            }
            if !st.source.remove(&ev.sc) {
                return false;
            }
            let out = st.remap.up(ev.sc, lone_tap);
            st.emit(&out);
            return true;
        }
        if st.hotkey.contains(&ev.sc) {
            return true;
        }
        if !capturing && !held {
            return false;
        }
        if !held && let Some(hk) = Hotkey::detect(&st.source, ev.sc) {
            st.hotkey.insert(ev.sc);
            drop(st);
            self.run_hotkey(hk);
            return true;
        }
        st.source.insert(ev.sc);
        let out = st.remap.down(ev.sc, held, remap);
        st.emit(&out);
        true
    }

    fn run_hotkey(&self, hk: Hotkey) {
        tracing::info!(?hk, "локальное сочетание клавиш");
        match hk {
            Hotkey::Fullscreen => {
                let _ = self.commands.send(Command::ToggleFullscreen);
            }
            Hotkey::ReleaseCapture => {
                self.gate.suspended.store(true, Ordering::Release);
                self.release_all();
                let _ = self.commands.send(Command::CaptureReleased);
            }
            Hotkey::CtrlAltDel => {
                // Ctrl и Alt уже нажаты на сервере пользователем.
                let mut st = self.state();
                st.emit(&[Emit {
                    sc: DELETE,
                    down: true,
                    repeat: false,
                }]);
                st.emit(&[Emit {
                    sc: DELETE,
                    down: false,
                    repeat: false,
                }]);
            }
        }
        (self.notify)();
    }

    /// Отпустить на сервере всё нажатое (потеря фокуса, отключение).
    pub fn release_all(&self) {
        self.state().release_all();
    }

    /// Выровнять Caps/Num/Scroll Lock сервера с локальными. Caps Lock,
    /// переназначенный на Ctrl+Shift, на сервере всегда выключен.
    pub fn sync(&self, mut locks: LockKeys) {
        if self.caps_remapped() {
            locks.caps = false;
        }
        let mut st = self.state();
        st.release_all();
        st.send(Events::from_elem(locks.event(), 1), false);
    }

    /// Ctrl+Alt+Del с кнопки меню.
    pub fn send_ctrl_alt_del(&self) {
        let mut st = self.state();
        for (sc, down) in [
            (LCTRL, true),
            (LALT, true),
            (DELETE, true),
            (DELETE, false),
            (LALT, false),
            (LCTRL, false),
        ] {
            st.emit(&[Emit {
                sc,
                down,
                repeat: false,
            }]);
        }
    }
}

impl State {
    fn emit(&mut self, out: &[Emit]) {
        let mut events = Events::new();
        for e in out {
            let count = self.server.entry(e.sc).or_default();
            let send = match (e.down, e.repeat) {
                (true, true) => *count > 0,
                (true, false) => {
                    *count += 1;
                    *count == 1
                }
                (false, _) => {
                    *count = count.saturating_sub(1);
                    *count == 0
                }
            };
            if *count == 0 {
                self.server.remove(&e.sc);
            }
            if send {
                events.extend(e.sc.events(e.down));
            }
        }
        self.send(events, true);
    }

    fn send(&self, events: Events, user: bool) {
        let Some(link) = &self.link else { return };
        if events.is_empty() {
            return;
        }
        if user {
            link.activity.touch();
        }
        let _ = link.input.send(RdpInputEvent::FastPath(events));
    }

    fn release_all(&mut self) {
        let mut events = Events::new();
        for sc in std::mem::take(&mut self.server).into_keys() {
            events.extend(sc.events(false));
        }
        self.source.clear();
        self.hotkey.clear();
        self.remap.reset();
        // Служебное отпускание — не активность пользователя.
        self.send(events, false);
    }
}

/// Локальные сочетания: Ctrl+Alt+клавиша (на Mac — ⌃⌥), как в mstsc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hotkey {
    /// Ctrl+Alt+Enter.
    Fullscreen,
    /// Ctrl+Alt+Home.
    ReleaseCapture,
    /// Ctrl+Alt+End → Ctrl+Alt+Del на сервере.
    CtrlAltDel,
}

impl Hotkey {
    fn detect(held: &HashSet<Scancode>, key: Scancode) -> Option<Self> {
        let ctrl = held.iter().any(|k| k.is_ctrl());
        let alt = held.iter().any(|k| k.is_alt());
        let others = held.iter().any(|k| !k.is_ctrl() && !k.is_alt());
        if !ctrl || !alt || others {
            return None;
        }
        match key {
            k if k == ENTER || k == NUM_ENTER => Some(Self::Fullscreen),
            k if k == HOME => Some(Self::ReleaseCapture),
            k if k == END => Some(Self::CtrlAltDel),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::scancode::{LWIN, parse_key};
    use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    struct Rig {
        router: Router,
        rx: UnboundedReceiver<RdpInputEvent>,
        commands: mpsc::Receiver<Command>,
    }

    fn rig(mode: CmdMode) -> Rig {
        let (tx, commands) = mpsc::channel();
        let router = Router::new(tx, || {});
        router.set_remap(Remap {
            cmd: mode,
            caps_to_ctrl_shift: false,
        });
        let (input, rx) = unbounded_channel();
        router.attach(Some(Link {
            input,
            activity: Activity::default(),
        }));
        router.set_active(true);
        Rig {
            router,
            rx,
            commands,
        }
    }

    /// (код, нажатие) всех клавиатурных событий, ушедших на сервер.
    fn sent(rx: &mut UnboundedReceiver<RdpInputEvent>) -> Vec<(u8, bool)> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let RdpInputEvent::FastPath(evs) = ev {
                for e in evs {
                    if let FastPathInputEvent::KeyboardEvent(f, code) = e {
                        out.push((code, !f.contains(KeyboardFlags::RELEASE)));
                    }
                }
            }
        }
        out
    }

    fn key(n: &str) -> Scancode {
        parse_key(n).unwrap()
    }

    #[test]
    fn passes_through_when_not_capturing() {
        let mut r = rig(CmdMode::Native);
        r.router.set_active(false);
        assert!(!r.router.handle(KeyInput::new(key("a"), true)));
        assert!(sent(&mut r.rx).is_empty());
    }

    #[test]
    fn release_reaches_server_after_capture_lost() {
        let mut r = rig(CmdMode::Native);
        let a = key("a");
        assert!(r.router.handle(KeyInput::new(a, true)));
        r.router.gate.active.store(false, Ordering::Release);
        assert!(r.router.handle(KeyInput::new(a, false)));
        assert_eq!(sent(&mut r.rx), vec![(0x1E, true), (0x1E, false)]);
        // Чужое отпускание не трогаем.
        assert!(!r.router.handle(KeyInput::new(a, false)));
    }

    #[test]
    fn focus_loss_releases_everything() {
        let mut r = rig(CmdMode::Native);
        r.router.handle(KeyInput::new(LCTRL, true));
        r.router.handle(KeyInput::new(key("a"), true));
        r.router.set_active(false);
        let out = sent(&mut r.rx);
        assert_eq!(out.iter().filter(|(_, d)| *d).count(), 2);
        assert_eq!(out.iter().filter(|(_, d)| !*d).count(), 2);
        // После этого физические отпускания уходят ОС.
        assert!(!r.router.handle(KeyInput::new(LCTRL, false)));
    }

    #[test]
    fn autorepeat_is_forwarded() {
        let mut r = rig(CmdMode::Native);
        let a = key("a");
        for _ in 0..3 {
            r.router.handle(KeyInput::new(a, true));
        }
        r.router.handle(KeyInput::new(a, false));
        assert_eq!(
            sent(&mut r.rx),
            vec![(0x1E, true), (0x1E, true), (0x1E, true), (0x1E, false)]
        );
    }

    #[test]
    fn shared_server_key_is_refcounted() {
        // ⌘ как Ctrl + настоящий Ctrl: Ctrl на сервере отпускается последним.
        let mut r = rig(CmdMode::Ctrl);
        r.router.handle(KeyInput::new(LCTRL, true));
        r.router.handle(KeyInput::new(LWIN, true));
        r.router.handle(KeyInput::new(LCTRL, false));
        assert_eq!(sent(&mut r.rx), vec![(0x1D, true)]);
        r.router.handle(KeyInput::new(LWIN, false));
        assert_eq!(sent(&mut r.rx), vec![(0x1D, false)]);
    }

    #[test]
    fn fullscreen_hotkey_is_local() {
        let mut r = rig(CmdMode::Native);
        r.router.handle(KeyInput::new(LCTRL, true));
        r.router.handle(KeyInput::new(LALT, true));
        assert!(r.router.handle(KeyInput::new(ENTER, true)));
        assert!(
            r.router.handle(KeyInput::new(ENTER, true)),
            "автоповтор хоткея"
        );
        assert!(r.router.handle(KeyInput::new(ENTER, false)));
        r.router.handle(KeyInput::new(LALT, false));
        r.router.handle(KeyInput::new(LCTRL, false));
        let out = sent(&mut r.rx);
        assert!(!out.iter().any(|(c, _)| *c == 0x1C), "{out:?}");
        assert_eq!(r.commands.try_recv(), Ok(Command::ToggleFullscreen));
    }

    #[test]
    fn release_capture_hotkey_suspends_until_resume() {
        let mut r = rig(CmdMode::Native);
        r.router.handle(KeyInput::new(LCTRL, true));
        r.router.handle(KeyInput::new(LALT, true));
        r.router.handle(KeyInput::new(HOME, true));
        assert!(r.router.suspended());
        assert_eq!(r.commands.try_recv(), Ok(Command::CaptureReleased));
        let out = sent(&mut r.rx);
        assert_eq!(out.iter().filter(|(_, d)| *d).count(), 2);
        assert_eq!(out.iter().filter(|(_, d)| !*d).count(), 2);
        assert!(!r.router.handle(KeyInput::new(key("a"), true)));
        r.router.resume();
        assert!(r.router.handle(KeyInput::new(key("a"), true)));
    }

    #[test]
    fn ctrl_alt_end_sends_ctrl_alt_del() {
        let mut r = rig(CmdMode::Native);
        r.router.handle(KeyInput::new(LCTRL, true));
        r.router.handle(KeyInput::new(LALT, true));
        r.router.handle(KeyInput::new(END, true));
        r.router.handle(KeyInput::new(END, false));
        assert_eq!(
            sent(&mut r.rx),
            vec![(0x1D, true), (0x38, true), (0x53, true), (0x53, false)]
        );
    }

    #[test]
    fn detach_releases_and_ignores() {
        let mut r = rig(CmdMode::Native);
        r.router.handle(KeyInput::new(key("a"), true));
        r.router.attach(None);
        assert_eq!(sent(&mut r.rx), vec![(0x1E, true), (0x1E, false)]);
        assert!(!r.router.handle(KeyInput::new(key("b"), true)));
    }

    #[test]
    fn caps_remap_and_lock_sync() {
        let mut r = rig(CmdMode::Native);
        r.router.set_remap(Remap {
            cmd: CmdMode::Native,
            caps_to_ctrl_shift: true,
        });
        r.router.handle(KeyInput::new(CAPS_LOCK, true));
        r.router.handle(KeyInput::new(CAPS_LOCK, false));
        assert_eq!(
            sent(&mut r.rx),
            vec![(0x1D, true), (0x2A, true), (0x2A, false), (0x1D, false)]
        );
        r.router.sync(LockKeys {
            caps: true,
            num: true,
            scroll: false,
        });
        let mut synced = None;
        while let Ok(RdpInputEvent::FastPath(evs)) = r.rx.try_recv() {
            for e in evs {
                if let FastPathInputEvent::SyncEvent(f) = e {
                    synced = Some(f);
                }
            }
        }
        use ironrdp_pdu::input::fast_path::SynchronizeFlags;
        let f = synced.unwrap();
        assert!(!f.contains(SynchronizeFlags::CAPS_LOCK));
        assert!(f.contains(SynchronizeFlags::NUM_LOCK));
    }
}
