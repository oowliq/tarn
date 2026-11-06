//! Перехват клавиатуры на Windows.
//!
//! - Сабкласс окна (`SetWindowSubclass`) видит WM_KEYDOWN/WM_KEYUP, пока окно
//!   в фокусе. Системные сочетания (Win, Alt+Tab) обрабатывает ОС.
//! - Низкоуровневый хук `WH_KEYBOARD_LL` в своём потоке видит всё, включая
//!   Win и Alt+Tab, и забирает их, пока активно окно Tarn (Ctrl+Alt+Del ОС не
//!   отдаёт никому — для него есть Ctrl+Alt+End).
//!
//! Важно: если процесс зарегистрирован на raw input клавиатуры, Windows не
//! вызывает его низкоуровневый хук, пока окно процесса в фокусе. winit
//! регистрирует raw input мыши и клавиатуры при создании цикла событий (для
//! `DeviceEvent`, которые egui не использует), поэтому клавиатуру из raw
//! input мы снимаем ([`unregister_raw_keyboard`]). Без этого Win и Alt+Tab
//! не доходят до сервера.
//!
//! Оба перехватчика зовут один [`Router`]: что забрал хук, окно уже не увидит.

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, VK_CAPITAL, VK_CONTROL, VK_LWIN, VK_MENU,
    VK_NUMLOCK, VK_PAUSE, VK_RWIN, VK_SCROLL,
};
use windows_sys::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_REMOVE, RegisterRawInputDevices};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageTime, GetMessageW,
    GetWindowThreadProcessId, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, PM_NOREMOVE,
    PeekMessageW, SetWindowsHookExW, WH_KEYBOARD_LL, WM_CHAR, WM_DEADCHAR, WM_KEYDOWN, WM_KEYFIRST,
    WM_KEYLAST, WM_KEYUP, WM_NCDESTROY, WM_SYSCHAR, WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use super::GlobalStatus;
use super::router::{KeyInput, Router};
use super::scancode::{LockKeys, PAUSE, Scancode};

const SUBCLASS_ID: usize = 0x7461_726E; // "tarn"
/// Фальшивый левый Ctrl, который Windows добавляет к AltGr.
const LL_FAKE_CTRL: u32 = 0x21D;
/// HID: Generic Desktop / Keyboard.
const HID_PAGE_GENERIC: u16 = 0x01;
const HID_USAGE_KEYBOARD: u16 = 0x06;

pub fn lock_keys() -> LockKeys {
    // SAFETY: чтение состояния клавиш текущего потока (UI).
    let on = |vk| unsafe { GetKeyState(i32::from(vk)) } & 1 != 0;
    LockKeys {
        caps: on(VK_CAPITAL),
        num: on(VK_NUMLOCK),
        scroll: on(VK_SCROLL),
    }
}

/// Скан-код события. `scan` — байт скан-кода (0 — не знаем, например у
/// событий, отправленных программами).
fn scancode(vk: u16, scan: u32, extended: bool) -> Option<Scancode> {
    if vk == VK_PAUSE {
        return Some(PAUSE);
    }
    let (mut code, mut ext) = (scan & 0xFF, extended);
    if code == 0 {
        // SAFETY: чистая функция над кодами.
        let v = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) };
        if v == 0 {
            return None;
        }
        code = v & 0xFF;
        ext = v >> 8 == 0xE0;
    }
    // Num Lock и правый Shift Windows помечает extended, а в RDP они без префикса.
    if code == 0x45 || code == 0x36 {
        ext = false;
    }
    Some(Scancode::from_parts(code as u8, ext))
}

/// Снять регистрацию raw input клавиатуры, которую сделал winit, — иначе
/// Windows не вызывает наш хук, пока окно Tarn в фокусе. Мышь не трогаем.
fn unregister_raw_keyboard() -> bool {
    let device = RAWINPUTDEVICE {
        usUsagePage: HID_PAGE_GENERIC,
        usUsage: HID_USAGE_KEYBOARD,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: null_mut(),
    };
    // SAFETY: один валидный RAWINPUTDEVICE, размер структуры передан верно.
    let ok = unsafe {
        RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
    } != 0;
    if !ok {
        let e = std::io::Error::last_os_error();
        tracing::warn!(%e, "клавиатура: не удалось снять raw input клавиатуры");
    }
    ok
}

/// Процесс активного окна (0 — окна нет). Сравниваем процесс, а не HWND: у
/// окна верхнего уровня могут быть владелец, IME-окна и т.п.
fn foreground_pid() -> u32 {
    // SAFETY: обе функции без побочных эффектов, из любого потока.
    unsafe {
        let w = GetForegroundWindow();
        if w.is_null() {
            return 0;
        }
        let mut owner = 0;
        GetWindowThreadProcessId(w, &mut owner);
        owner
    }
}

struct Ctx {
    router: Arc<Router>,
    pid: u32,
}

static HOOK: OnceLock<Ctx> = OnceLock::new();
/// Поставить хук не удалось.
static HOOK_FAILED: AtomicBool = AtomicBool::new(false);

pub struct Backend {
    router: Arc<Router>,
    hwnd: Option<isize>,
    hook_started: AtomicBool,
}

impl Backend {
    /// Вызывать в потоке окна (главном), когда цикл событий winit уже создан.
    pub fn start(cc: &eframe::CreationContext<'_>, router: Arc<Router>) -> Self {
        let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get()),
            _ => None,
        };
        match hwnd {
            Some(hwnd) => {
                let data = Arc::into_raw(router.clone()) as usize;
                // SAFETY: окно принадлежит этому потоку; data освобождается в WM_NCDESTROY.
                let ok = unsafe {
                    SetWindowSubclass(hwnd as HWND, Some(subclass_proc), SUBCLASS_ID, data)
                };
                if ok == 0 {
                    tracing::error!("клавиатура: SetWindowSubclass не удался");
                    // SAFETY: data не досталась окну — забираем обратно.
                    drop(unsafe { Arc::from_raw(data as *const Router) });
                }
            }
            None => tracing::error!("клавиатура: нет HWND окна — клавиатура не работает"),
        }
        unregister_raw_keyboard();
        Self {
            router,
            hwnd,
            hook_started: AtomicBool::new(false),
        }
    }

    /// Поставить низкоуровневый хук (один раз; дальше его включает и
    /// выключает флаг `grab`).
    pub fn enable_global(&self) {
        if self.hwnd.is_none() || self.hook_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = HOOK.set(Ctx {
            router: self.router.clone(),
            pid: std::process::id(),
        });
        let spawned = std::thread::Builder::new()
            .name("kbd-hook".into())
            .spawn(run_hook);
        if let Err(e) = spawned {
            tracing::error!(%e, "клавиатура: не удалось запустить поток хука");
            HOOK_FAILED.store(true, Ordering::Release);
        }
    }

    pub fn status(&self) -> GlobalStatus {
        if !self.router.gate.grab() {
            GlobalStatus::Off
        } else if self.router.gate.global_ready() {
            GlobalStatus::Active
        } else if self.hook_started.load(Ordering::Acquire) && !HOOK_FAILED.load(Ordering::Acquire)
        {
            GlobalStatus::Starting
        } else {
            GlobalStatus::Failed
        }
    }

    pub fn request_permission(&self) {}
}

fn run_hook() {
    let Some(ctx) = HOOK.get() else { return };
    // SAFETY: хук ставится на этот поток, сообщения крутятся здесь же.
    unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), GetModuleHandleW(null()), 0);
        if hook.is_null() {
            let e = std::io::Error::last_os_error();
            tracing::error!(%e, "клавиатура: SetWindowsHookExW не удался");
            HOOK_FAILED.store(true, Ordering::Release);
            return;
        }
        ctx.router.gate.set_global_ready(true);
        tracing::info!("клавиатура: перехват системных сочетаний включён");
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
        ctx.router.gate.set_global_ready(false);
    }
}

unsafe extern "system" fn ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && let Some(ctx) = HOOK.get()
    {
        // SAFETY: для HC_ACTION lparam указывает на KBDLLHOOKSTRUCT.
        let k = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if ll_handle(ctx, k, matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN)) {
            return 1;
        }
    }
    // SAFETY: передаём событие дальше по цепочке.
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

fn ll_handle(ctx: &Ctx, k: &KBDLLHOOKSTRUCT, down: bool) -> bool {
    let router = &ctx.router;
    let foreground = || foreground_pid() == ctx.pid;
    if k.scanCode == LL_FAKE_CTRL {
        return router.gate.global_capturing() && foreground();
    }
    let Some(sc) = scancode(k.vkCode as u16, k.scanCode, k.flags & LLKHF_EXTENDED != 0) else {
        return false;
    };
    // Отпускания предлагаем всегда: роутер знает, нажималась ли клавиша на сервере.
    if down && !(router.global_wants(sc) && foreground()) {
        return false;
    }
    router.handle(KeyInput::new(sc, down))
}

/// AltGr приходит как левый Ctrl + правый Alt с тем же временем: такой Ctrl
/// серверу не нужен — он сам добавит его к AltGr.
fn is_fake_ctrl(hwnd: HWND, vk: u16, lparam: LPARAM) -> bool {
    if vk != VK_CONTROL || (lparam >> 24) & 1 != 0 {
        return false;
    }
    // SAFETY: просмотр очереди своего окна без извлечения.
    unsafe {
        let mut next: MSG = std::mem::zeroed();
        PeekMessageW(&mut next, hwnd, WM_KEYFIRST, WM_KEYLAST, PM_NOREMOVE) != 0
            && next.wParam as u16 == VK_MENU
            && (next.lParam >> 24) & 1 != 0
            && next.time == GetMessageTime() as u32
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: data — Arc<Router>, отданный в SetWindowSubclass.
    let router = unsafe { &*(data as *const Router) };
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            let vk = wparam as u16;
            // Win без перехвата системных сочетаний остаётся у ОС.
            if vk != VK_LWIN && vk != VK_RWIN {
                if is_fake_ctrl(hwnd, vk, lparam) {
                    if router.gate.capturing() {
                        return 0;
                    }
                } else if let Some(sc) =
                    scancode(vk, (lparam >> 16) as u32 & 0xFF, (lparam >> 24) & 1 != 0)
                    && router.handle(KeyInput::new(sc, down))
                {
                    return 0;
                }
            }
        }
        // Символы от уже переданных на сервер клавиш egui не нужны.
        WM_CHAR | WM_SYSCHAR | WM_DEADCHAR | WM_SYSDEADCHAR if router.gate.capturing() => {
            return 0;
        }
        WM_NCDESTROY => {
            // SAFETY: снимаем свой сабкласс и забираем Arc обратно.
            unsafe {
                RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
                drop(Arc::from_raw(data as *const Router));
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
        }
        _ => {}
    }
    // SAFETY: штатная обработка остальных сообщений.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::scancode::{LWIN, RSHIFT, parse_key};
    use windows_sys::Win32::UI::Input::GetRegisteredRawInputDevices;

    fn keyboard_registered() -> bool {
        let mut devices = [RAWINPUTDEVICE {
            usUsagePage: 0,
            usUsage: 0,
            dwFlags: 0,
            hwndTarget: null_mut(),
        }; 8];
        let mut count = devices.len() as u32;
        // SAFETY: буфер на `count` элементов, размер структуры передан верно.
        let n = unsafe {
            GetRegisteredRawInputDevices(
                devices.as_mut_ptr(),
                &mut count,
                std::mem::size_of::<RAWINPUTDEVICE>() as u32,
            )
        };
        assert_ne!(n, u32::MAX, "GetRegisteredRawInputDevices");
        devices[..n as usize]
            .iter()
            .any(|d| d.usUsagePage == HID_PAGE_GENERIC && d.usUsage == HID_USAGE_KEYBOARD)
    }

    /// Регрессия: пока процесс зарегистрирован на raw input клавиатуры (это
    /// делает winit), Windows не зовёт наш хук при активном окне Tarn — Win и
    /// Alt+Tab не доходят до сервера.
    #[test]
    fn raw_keyboard_registration_is_removed() {
        // Регистрируем клавиатуру, как winit (без окна — ввод идёт за фокусом).
        let device = RAWINPUTDEVICE {
            usUsagePage: HID_PAGE_GENERIC,
            usUsage: HID_USAGE_KEYBOARD,
            dwFlags: 0,
            hwndTarget: null_mut(),
        };
        // SAFETY: один валидный RAWINPUTDEVICE.
        let registered = unsafe {
            RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
        } != 0;
        assert!(registered, "{}", std::io::Error::last_os_error());
        assert!(keyboard_registered());
        assert!(unregister_raw_keyboard());
        assert!(!keyboard_registered());
    }

    #[test]
    fn scancode_fixups() {
        assert_eq!(scancode(VK_PAUSE, 0x45, false), Some(PAUSE));
        // Num Lock приходит с extended — в RDP без префикса.
        assert_eq!(scancode(VK_NUMLOCK, 0x45, true), parse_key("numlock"));
        assert_eq!(scancode(0xA1, 0x36, true), Some(RSHIFT));
        assert_eq!(scancode(VK_LWIN, 0x5B, true), Some(LWIN));
        // Без скан-кода (событие от программы) — берём из виртуального кода.
        assert_eq!(scancode(VK_LWIN, 0, false), Some(LWIN));
        assert_eq!(scancode(0x41, 0, false), parse_key("a"));
    }
}
