//! Перехват клавиатуры на macOS.
//!
//! - Локальный монитор NSEvent видит нажатия, пока окно Tarn активно, и
//!   разрешений не требует. Системные сочетания (⌘Tab, ⌘Space, Mission
//!   Control) до приложения не доходят.
//! - CGEventTap на уровне сессии видит всё, включая системные сочетания, и
//!   забирает их до Dock/Spotlight. Нужен доступ «Универсальный доступ»; пока
//!   его нет, работает только монитор.
//!
//! Оба зовут один [`Router`]: что забрал tap, монитор уже не увидит.
//!
//! Caps Lock → Ctrl+Shift: macOS сообщает о Caps Lock одним flagsChanged на
//! переключение и сама меняет свой Caps Lock — ни монитор, ни tap этому не
//! мешают. Поэтому на сервер уходит нажатие Ctrl+Shift (для смены языка этого
//! достаточно), а локальный Caps Lock сразу выключается через IOKit.

use std::ffi::{c_char, c_void};
use std::ptr::{NonNull, null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::Duration;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask};

use super::GlobalStatus;
use super::mac_keymap::{FLAGS_CHANGED, KEYCODE_CAPS_LOCK, decode};
use super::router::Router;
use super::scancode::LockKeys;

type CGEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type TapCallback = unsafe extern "C" fn(*mut c_void, u32, CGEventRef, *mut c_void) -> CGEventRef;

const SESSION_EVENT_TAP: u32 = 1;
const HEAD_INSERT: u32 = 0;
const TAP_DEFAULT: u32 = 0;
const KEY_EVENTS_MASK: u64 = (1 << 10) | (1 << 11) | (1 << 12);
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
const FIELD_KEYCODE: u32 = 9;
const FIELD_TARGET_PID: u32 = 40;
const HID_SYSTEM_STATE: i32 = 1;
const FLAG_CAPS_LOCK: u64 = 0x0001_0000;
/// `'ISO '` — тип раскладки клавиатуры ISO в Carbon.
const KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: TapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventSourceFlagsState(state: i32) -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: *const c_void;
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFMachPortCreateRunLoopSource(
        alloc: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> *mut c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    static kAXTrustedCheckOptionPrompt: *const c_void;
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IOServiceOpen(service: u32, owning_task: u32, kind: u32, connect: *mut u32) -> i32;
    fn IOServiceClose(connect: u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOHIDSetModifierLockState(connect: u32, selector: i32, state: bool) -> i32;
}

unsafe extern "C" {
    static mach_task_self_: u32;
}

const IO_MAIN_PORT_DEFAULT: u32 = 0;
const HID_PARAM_CONNECT: u32 = 1;
const HID_CAPS_LOCK_STATE: i32 = 1;

/// Выключить локальный Caps Lock. `false` — система не дала.
fn reset_caps_lock() -> bool {
    // SAFETY: стандартная последовательность IOKit; объекты освобождаются.
    unsafe {
        let service = IOServiceGetMatchingService(
            IO_MAIN_PORT_DEFAULT,
            IOServiceMatching(c"IOHIDSystem".as_ptr()),
        );
        if service == 0 {
            return false;
        }
        let mut connect = 0;
        let opened = IOServiceOpen(service, mach_task_self_, HID_PARAM_CONNECT, &mut connect) == 0;
        IOObjectRelease(service);
        if !opened {
            return false;
        }
        let ok = IOHIDSetModifierLockState(connect, HID_CAPS_LOCK_STATE, false) == 0;
        IOServiceClose(connect);
        ok
    }
}

/// Последний сброс Caps Lock удался: следующее «выключение» Caps Lock —
/// эхо этого сброса, а не нажатие.
static CAPS_RESET_OK: AtomicBool = AtomicBool::new(false);

/// Событие macOS → роутер. `allow_down` решает, можно ли забирать нажатие
/// (отпускания роутер получает всегда). `true` — событие наше.
fn dispatch(
    router: &Router,
    kind: u32,
    keycode: u16,
    flags: u64,
    iso: bool,
    allow_down: impl Fn() -> bool,
) -> bool {
    let caps = kind == FLAGS_CHANGED && keycode == KEYCODE_CAPS_LOCK && router.caps_remapped();
    let caps_on = flags & FLAG_CAPS_LOCK != 0;
    if caps && !caps_on && CAPS_RESET_OK.load(Ordering::Acquire) {
        return router.gate.capturing();
    }
    let mut ours = false;
    for k in decode(kind, keycode, flags, iso) {
        if !k.down || allow_down() {
            ours |= router.handle(k);
        }
    }
    if caps && caps_on && ours {
        let ok = reset_caps_lock();
        if !ok {
            tracing::warn!("клавиатура: не удалось выключить локальный Caps Lock");
        }
        CAPS_RESET_OK.store(ok, Ordering::Release);
    }
    ours
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn LMGetKbdType() -> u8;
    fn KBGetLayoutType(keyboard_type: i16) -> u32;
}

fn trusted() -> bool {
    // SAFETY: без аргументов, потокобезопасна.
    unsafe { AXIsProcessTrusted() != 0 }
}

fn iso_keyboard() -> bool {
    // SAFETY: чтение типа текущей клавиатуры.
    unsafe { KBGetLayoutType(i16::from(LMGetKbdType())) == KEYBOARD_ISO }
}

pub fn lock_keys() -> LockKeys {
    // SAFETY: чтение глобального состояния модификаторов.
    let flags = unsafe { CGEventSourceFlagsState(HID_SYSTEM_STATE) };
    LockKeys {
        caps: flags & FLAG_CAPS_LOCK != 0,
        // На Mac цифровой блок всегда печатает цифры.
        num: true,
        scroll: false,
    }
}

pub struct Backend {
    router: Arc<Router>,
    monitor: Option<Retained<AnyObject>>,
    tap_started: AtomicBool,
}

struct TapCtx {
    router: Arc<Router>,
    iso: bool,
    pid: i64,
    port: AtomicPtr<c_void>,
}

impl Backend {
    /// Вызывать в главном потоке (там живёт AppKit).
    pub fn start(_cc: &eframe::CreationContext<'_>, router: Arc<Router>) -> Self {
        let monitor = if MainThreadMarker::new().is_some() {
            install_monitor(router.clone())
        } else {
            tracing::error!("клавиатура: монитор событий можно поставить только в главном потоке");
            None
        };
        Self {
            router,
            monitor,
            tap_started: AtomicBool::new(false),
        }
    }

    /// Запустить перехват системных сочетаний (один раз; дальше его
    /// включает и выключает флаг `grab`).
    pub fn enable_global(&self) {
        if self.tap_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let ctx: &'static TapCtx = Box::leak(Box::new(TapCtx {
            router: self.router.clone(),
            iso: iso_keyboard(),
            pid: i64::from(std::process::id()),
            port: AtomicPtr::new(null_mut()),
        }));
        let spawned = std::thread::Builder::new()
            .name("kbd-tap".into())
            .spawn(move || run_tap(ctx));
        if let Err(e) = spawned {
            tracing::error!(%e, "клавиатура: не удалось запустить поток перехвата");
        }
    }

    pub fn status(&self) -> GlobalStatus {
        if !self.router.gate.grab() {
            GlobalStatus::Off
        } else if self.router.gate.global_ready() {
            GlobalStatus::Active
        } else if !trusted() {
            GlobalStatus::NeedsPermission
        } else {
            GlobalStatus::Starting
        }
    }

    /// Системный запрос «Универсального доступа»: добавляет Tarn в список.
    pub fn request_permission(&self) {
        // SAFETY: словарь из одной пары CF-констант, освобождается после вызова.
        unsafe {
            let keys = [kAXTrustedCheckOptionPrompt];
            let values = [kCFBooleanTrue];
            let dict = CFDictionaryCreate(
                null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            );
            AXIsProcessTrustedWithOptions(dict);
            if !dict.is_null() {
                CFRelease(dict);
            }
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(m) = self.monitor.take() {
            // SAFETY: монитор получен из addLocalMonitorForEventsMatchingMask.
            unsafe { NSEvent::removeMonitor(&m) };
        }
    }
}

fn install_monitor(router: Arc<Router>) -> Option<Retained<AnyObject>> {
    let iso = iso_keyboard();
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit передаёт живое событие на время вызова.
        let ev = unsafe { event.as_ref() };
        let kind = ev.r#type().0 as u32;
        let flags = ev.modifierFlags().0 as u64;
        let ours = dispatch(&router, kind, ev.keyCode(), flags, iso, || true);
        if ours { null_mut() } else { event.as_ptr() }
    });
    let mask = NSEventMask::KeyDown | NSEventMask::KeyUp | NSEventMask::FlagsChanged;
    // SAFETY: главный поток; блок живёт, пока монитор не снят (AppKit копирует его).
    let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) };
    if monitor.is_none() {
        tracing::error!("клавиатура: не удалось поставить монитор событий");
    }
    monitor
}

fn run_tap(ctx: &'static TapCtx) {
    let gate = &ctx.router.gate;
    loop {
        // Ждём, пока пользователь выдаст доступ (и не выключил перехват).
        while !(gate.grab() && trusted()) {
            std::thread::sleep(Duration::from_secs(1));
        }
        // SAFETY: ctx — 'static, callback не паникует наружу.
        let port = unsafe {
            CGEventTapCreate(
                SESSION_EVENT_TAP,
                HEAD_INSERT,
                TAP_DEFAULT,
                KEY_EVENTS_MASK,
                tap_callback,
                (ctx as *const TapCtx).cast_mut().cast(),
            )
        };
        if port.is_null() {
            tracing::warn!("клавиатура: CGEventTapCreate не удался, повтор через 2 с");
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        ctx.port.store(port, Ordering::Release);
        // SAFETY: порт валиден, источник добавляется в цикл этого потока.
        unsafe {
            let source = CFMachPortCreateRunLoopSource(null(), port, 0);
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            CGEventTapEnable(port, true);
        }
        gate.set_global_ready(true);
        tracing::info!("клавиатура: перехват системных сочетаний включён");
        // SAFETY: крутит цикл событий потока, пока жив процесс.
        unsafe { CFRunLoopRun() };
        gate.set_global_ready(false);
        break;
    }
}

unsafe extern "C" fn tap_callback(
    _proxy: *mut c_void,
    kind: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    // SAFETY: user_info — &'static TapCtx из run_tap.
    let ctx = unsafe { &*user_info.cast::<TapCtx>() };
    if kind == TAP_DISABLED_BY_TIMEOUT || kind == TAP_DISABLED_BY_USER_INPUT {
        // Система отключает медленный tap — включаем обратно.
        let port = ctx.port.load(Ordering::Acquire);
        if !port.is_null() {
            // SAFETY: порт создан в run_tap и не освобождается.
            unsafe { CGEventTapEnable(port, true) };
        }
        return event;
    }
    // SAFETY: event валиден на время вызова.
    let (keycode, flags, pid) = unsafe {
        (
            CGEventGetIntegerValueField(event, FIELD_KEYCODE) as u16,
            CGEventGetFlags(event),
            CGEventGetIntegerValueField(event, FIELD_TARGET_PID),
        )
    };
    let router = &ctx.router;
    let ours_app = pid == 0 || pid == ctx.pid;
    // Нажатия забираем, только если перехват включён и активно наше окно;
    // отпускания — всегда предлагаем роутеру (он знает, чьи они).
    let ours = dispatch(router, kind, keycode, flags, ctx.iso, || {
        router.gate.global_capturing() && ours_app
    });
    if ours { null_mut() } else { event }
}
