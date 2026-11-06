//! Инструменты MCP: скриншот, лупа, мышь, клавиатура, пауза.
//!
//! Координаты во всех инструментах — в пикселях скриншота, который
//! уменьшен под ограничения модели. Масштаб — детерминированная функция
//! размера удалённого экрана, поэтому хранить состояние между вызовами не нужно.

use std::time::Duration;

use base64::Engine as _;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::imageops::FilterType;
use image::{ImageEncoder as _, RgbImage};
use ironrdp_client::rdp::RdpInputEvent;
use ironrdp_pdu::input::MousePdu;
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use ironrdp_pdu::input::mouse::PointerFlags;
use serde_json::{Value, json};
use smallvec::SmallVec;

use super::{AgentMode, Hub, Target};
use crate::keyboard::scancode::{self, ENTER, LALT, LCTRL, LSHIFT, LWIN, Scancode, TAB};
use crate::session::State;

type Events = SmallVec<[FastPathInputEvent; 2]>;

/// Ограничения на картинку для модели: длинная сторона и число пикселей.
const MAX_EDGE: f64 = 1568.0;
const MAX_PIXELS: f64 = 1_150_000.0;
const WHEEL_NOTCH: i16 = 120;
/// Сколько ждать, пока сессия подтвердит запись ввода в сокет.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TEXT_CHARS: usize = 10_000;

pub struct CallResult {
    pub content: Vec<Value>,
    pub is_error: bool,
}

fn text(t: impl Into<String>) -> Value {
    json!({ "type": "text", "text": t.into() })
}

fn ok(t: impl Into<String>) -> Result<Vec<Value>, String> {
    Ok(vec![text(t)])
}

pub fn list() -> Value {
    let xy = |what: &str| {
        json!({
            "x": { "type": "number", "description": format!("X {what}, пиксели скриншота") },
            "y": { "type": "number", "description": format!("Y {what}, пиксели скриншота") },
        })
    };
    let merge = |mut a: Value, b: Value| {
        if let (Some(a), Some(b)) = (a.as_object_mut(), b.as_object()) {
            a.extend(b.clone());
        }
        a
    };
    let modifiers = json!({
        "modifiers": {
            "type": "array",
            "items": { "type": "string", "enum": ["ctrl", "shift", "alt", "win"] },
            "description": "Клавиши, зажатые на время действия (ctrl+клик, shift+выделение)",
        },
    });
    let read_only = json!({ "readOnlyHint": true, "openWorldHint": false });
    let control = json!({ "readOnlyHint": false, "destructiveHint": true, "openWorldHint": true });
    json!([
        {
            "name": "screenshot",
            "title": "Скриншот удалённого экрана",
            "description": "Снимок удалённого рабочего стола. В ответе указан размер скриншота: \
                все координаты для остальных инструментов — в его пикселях.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "zoom",
            "title": "Лупа",
            "description": "Фрагмент экрана в полном разрешении — чтобы прочитать мелкий текст. \
                Прямоугольник задаётся в пикселях скриншота.",
            "inputSchema": {
                "type": "object",
                "properties": merge(xy("левого верхнего угла"), json!({
                    "width": { "type": "number", "description": "Ширина, пиксели скриншота" },
                    "height": { "type": "number", "description": "Высота, пиксели скриншота" },
                })),
                "required": ["x", "y", "width", "height"],
            },
            "annotations": read_only,
        },
        {
            "name": "click",
            "title": "Клик мышью",
            "description": "Клик в точке скриншота. Можно двойной/тройной и с зажатыми модификаторами.",
            "inputSchema": {
                "type": "object",
                "properties": merge(merge(xy("точки"), json!({
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" },
                    "clicks": { "type": "integer", "minimum": 1, "maximum": 3, "default": 1 },
                })), modifiers.clone()),
                "required": ["x", "y"],
            },
            "annotations": control,
        },
        {
            "name": "move_mouse",
            "title": "Навести курсор",
            "description": "Переместить курсор (например, чтобы показать подсказку).",
            "inputSchema": { "type": "object", "properties": xy("точки"), "required": ["x", "y"] },
            "annotations": control,
        },
        {
            "name": "drag",
            "title": "Перетащить",
            "description": "Зажать кнопку в (x, y), протянуть до (to_x, to_y) и отпустить.",
            "inputSchema": {
                "type": "object",
                "properties": merge(merge(xy("начала"), json!({
                    "to_x": { "type": "number" },
                    "to_y": { "type": "number" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" },
                })), modifiers),
                "required": ["x", "y", "to_x", "to_y"],
            },
            "annotations": control,
        },
        {
            "name": "scroll",
            "title": "Прокрутка",
            "description": "Прокрутить колесом над точкой. down > 0 — вниз, < 0 — вверх; \
                right > 0 — вправо. Единица — один щелчок колеса.",
            "inputSchema": {
                "type": "object",
                "properties": merge(xy("точки"), json!({
                    "down": { "type": "integer", "minimum": -50, "maximum": 50, "default": 0 },
                    "right": { "type": "integer", "minimum": -50, "maximum": 50, "default": 0 },
                })),
                "required": ["x", "y"],
            },
            "annotations": control,
        },
        {
            "name": "type_text",
            "title": "Напечатать текст",
            "description": "Напечатать текст в элемент с фокусом ввода. Символы уходят как Unicode — \
                раскладка клавиатуры сервера не важна. \\n — Enter, \\t — Tab. \
                Сначала кликни в нужное поле.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "maxLength": MAX_TEXT_CHARS },
                },
                "required": ["text"],
            },
            "annotations": control,
        },
        {
            "name": "press_keys",
            "title": "Нажать клавиши",
            "description": "Нажать клавишу или сочетание: «enter», «esc», «ctrl+c», «alt+f4», \
                «win+r», «ctrl+shift+esc». Клавиши через «+», нажимаются по порядку и \
                отпускаются в обратном. Буквы — по положению на английской раскладке \
                (ctrl+c работает при любой раскладке).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keys": { "type": "string", "description": "Сочетание, например «ctrl+shift+t»" },
                    "repeat": { "type": "integer", "minimum": 1, "maximum": 50, "default": 1 },
                },
                "required": ["keys"],
            },
            "annotations": control,
        },
        {
            "name": "wait",
            "title": "Подождать",
            "description": "Подождать, пока экран обновится (загрузка, анимация), до 10 секунд.",
            "inputSchema": {
                "type": "object",
                "properties": { "seconds": { "type": "number", "minimum": 0, "maximum": 10 } },
                "required": ["seconds"],
            },
            "annotations": read_only,
        },
    ])
}

/// None — инструмента с таким именем нет.
pub fn call(hub: &Hub, name: &str, args: &Value) -> Option<CallResult> {
    let _guard = hub.exec_lock();
    let res = match name {
        "screenshot" => screenshot(hub),
        "zoom" => zoom(hub, args),
        "wait" => wait(args),
        "click" => control(hub, |c| c.click(args)),
        "move_mouse" => control(hub, |c| c.move_mouse(args)),
        "drag" => control(hub, |c| c.drag(args)),
        "scroll" => control(hub, |c| c.scroll(args)),
        "type_text" => control(hub, |c| c.type_text(args)),
        "press_keys" => control(hub, |c| c.press_keys(args)),
        _ => return None,
    };
    Some(match res {
        Ok(content) => CallResult {
            content,
            is_error: false,
        },
        Err(e) => {
            hub.log(format!("{name}: {e}"), true);
            CallResult {
                content: vec![text(e)],
                is_error: true,
            }
        }
    })
}

// ── Геометрия ─────────────────────────────────────────────────────────────

/// Размер скриншота для экрана `w`×`h`.
pub fn shot_size([w, h]: [usize; 2]) -> (u32, u32) {
    let (fw, fh) = (w as f64, h as f64);
    let s = (MAX_EDGE / fw.max(fh))
        .min((MAX_PIXELS / (fw * fh)).sqrt())
        .min(1.0);
    (
        ((fw * s).round() as u32).max(1),
        ((fh * s).round() as u32).max(1),
    )
}

/// Точка скриншота → пиксель экрана.
fn to_remote(size: [usize; 2], x: f64, y: f64) -> Result<(u16, u16), String> {
    let (sw, sh) = shot_size(size);
    if !(x.is_finite() && y.is_finite())
        || x < 0.0
        || y < 0.0
        || x >= f64::from(sw)
        || y >= f64::from(sh)
    {
        return Err(format!(
            "Точка ({x}, {y}) вне скриншота {sw}×{sh}. Сделай новый screenshot."
        ));
    }
    let rx = ((x + 0.5) * size[0] as f64 / f64::from(sw)).floor() as usize;
    let ry = ((y + 0.5) * size[1] as f64 / f64::from(sh)).floor() as usize;
    Ok((
        rx.min(size[0] - 1).min(u16::MAX as usize) as u16,
        ry.min(size[1] - 1).min(u16::MAX as usize) as u16,
    ))
}

fn num(args: &Value, key: &str) -> Result<f64, String> {
    args.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("Нужен числовой параметр «{key}»"))
}

fn int_or(args: &Value, key: &str, default: i64, min: i64, max: i64) -> Result<i64, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => match v.as_f64() {
            Some(f) if f.fract() == 0.0 && (min as f64..=max as f64).contains(&f) => Ok(f as i64),
            _ => Err(format!("«{key}» — целое от {min} до {max}")),
        },
    }
}

// ── Экран ─────────────────────────────────────────────────────────────────

fn session(hub: &Hub) -> Result<Target, String> {
    if hub.mode() == AgentMode::Off {
        return Err("Доступ для Claude выключен в Tarn.".into());
    }
    hub.target()
        .ok_or_else(|| "Нет активной RDP-сессии: попроси пользователя подключиться в Tarn.".into())
}

fn check_state(t: &Target) -> Result<(), String> {
    match t.shared.lock().unwrap().state {
        Some(State::Connected) => Ok(()),
        Some(State::Connecting) | None => Err("Сессия ещё подключается, подожди и повтори.".into()),
        Some(State::Closed(_)) => Err("Сессия закрыта.".into()),
    }
}

fn frame(t: &Target) -> Result<RgbImage, String> {
    check_state(t)?;
    let snap = t.shared.lock().unwrap().snapshot_rgb();
    let ([w, h], buf) = snap.ok_or("Кадр экрана ещё не получен.")?;
    RgbImage::from_raw(w as u32, h as u32, buf).ok_or_else(|| "Повреждённый кадр".into())
}

fn png_base64(img: &RgbImage) -> Result<String, String> {
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, PngFilter::Adaptive)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| format!("Не удалось закодировать PNG: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(out))
}

fn image_content(img: &RgbImage) -> Result<Value, String> {
    Ok(json!({ "type": "image", "data": png_base64(img)?, "mimeType": "image/png" }))
}

fn screenshot(hub: &Hub) -> Result<Vec<Value>, String> {
    let t = session(hub)?;
    let img = frame(&t)?;
    let size = [img.width() as usize, img.height() as usize];
    let (sw, sh) = shot_size(size);
    let shot = if (sw, sh) == (img.width(), img.height()) {
        img
    } else {
        image::imageops::resize(&img, sw, sh, FilterType::Triangle)
    };
    let mode = match hub.mode() {
        AgentMode::Control => "управление разрешено",
        _ => "только просмотр — управлять нельзя",
    };
    hub.log(format!("screenshot {sw}×{sh}"), false);
    Ok(vec![
        image_content(&shot)?,
        text(format!(
            "Скриншот {sw}×{sh} (реальный экран {}×{}). Координаты — в пикселях этого скриншота. Режим: {mode}.",
            size[0], size[1]
        )),
    ])
}

fn zoom(hub: &Hub, args: &Value) -> Result<Vec<Value>, String> {
    let t = session(hub)?;
    let img = frame(&t)?;
    let size = [img.width() as usize, img.height() as usize];
    let (x, y) = (num(args, "x")?, num(args, "y")?);
    let (w, h) = (num(args, "width")?, num(args, "height")?);
    if w < 1.0 || h < 1.0 {
        return Err("width и height должны быть ≥ 1".into());
    }
    let (sw, sh) = shot_size(size);
    let x1 = (x + w).min(f64::from(sw)) - 1.0;
    let y1 = (y + h).min(f64::from(sh)) - 1.0;
    let (rx0, ry0) = to_remote(size, x, y)?;
    let (rx1, ry1) = to_remote(size, x1.max(x), y1.max(y))?;
    let (cw, ch) = (u32::from(rx1 - rx0) + 1, u32::from(ry1 - ry0) + 1);
    let crop = image::imageops::crop_imm(&img, rx0.into(), ry0.into(), cw, ch).to_image();
    let (fw, fh) = shot_size([cw as usize, ch as usize]);
    let crop = if (fw, fh) == (cw, ch) {
        crop
    } else {
        image::imageops::resize(&crop, fw, fh, FilterType::Triangle)
    };
    hub.log(format!("zoom ({x}, {y}) {w}×{h}"), false);
    Ok(vec![
        image_content(&crop)?,
        text(format!(
            "Фрагмент скриншота ({x}, {y}) {w}×{h} в полном разрешении ({cw}×{ch} px экрана). \
             Координаты для кликов — по-прежнему в пикселях полного скриншота {sw}×{sh}."
        )),
    ])
}

fn wait(args: &Value) -> Result<Vec<Value>, String> {
    let s = num(args, "seconds")?;
    if !(0.0..=10.0).contains(&s) {
        return Err("seconds — от 0 до 10".into());
    }
    std::thread::sleep(Duration::from_secs_f64(s));
    ok(format!("Подождал {s} с."))
}

// ── Управление ────────────────────────────────────────────────────────────

/// « с ctrl+shift» для журнала.
fn mods_label(args: &Value) -> String {
    match args.get("modifiers").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => {
            let names: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
            format!(" с {}", names.join("+"))
        }
        _ => String::new(),
    }
}

const STOPPED: &str = "Управление остановлено пользователем: сейчас режим «только просмотр». \
    Не пытайся продолжать — попроси пользователя снова включить «Управление» в меню «🤖 Claude».";

fn pause(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

fn button(args: &Value) -> Result<PointerFlags, String> {
    match args.get("button").and_then(Value::as_str).unwrap_or("left") {
        "left" => Ok(PointerFlags::LEFT_BUTTON),
        "right" => Ok(PointerFlags::RIGHT_BUTTON),
        "middle" => Ok(PointerFlags::MIDDLE_BUTTON_OR_WHEEL),
        other => Err(format!("Неизвестная кнопка «{other}»")),
    }
}

/// Модификаторы из `modifiers` в порядке нажатия.
fn modifiers(args: &Value) -> Result<Vec<Scancode>, String> {
    let Some(v) = args.get("modifiers").filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let list = v.as_array().ok_or("«modifiers» — массив строк")?;
    let mut out = Vec::new();
    for m in list {
        let sc = match m.as_str() {
            Some("ctrl") => LCTRL,
            Some("shift") => LSHIFT,
            Some("alt") => LALT,
            Some("win") => LWIN,
            _ => {
                return Err(format!(
                    "Неизвестный модификатор {m}: допустимы ctrl, shift, alt, win"
                ));
            }
        };
        if !out.contains(&sc) {
            out.push(sc);
        }
    }
    Ok(out)
}

/// Действие управления. Ответ — только когда весь ввод ушёл в сокет: иначе
/// агент снимает скриншот раньше, чем ввод дошёл до сервера.
fn control(
    hub: &Hub,
    f: impl FnOnce(&Ctl<'_>) -> Result<Vec<Value>, String>,
) -> Result<Vec<Value>, String> {
    let c = Ctl::new(hub)?;
    let res = f(&c);
    // Даже при ошибке дожидаемся отпусканий, ушедших через release().
    let flushed = c.flush();
    let out = res?;
    flushed?;
    Ok(out)
}

struct Ctl<'a> {
    hub: &'a Hub,
    t: Target,
    size: [usize; 2],
}

impl<'a> Ctl<'a> {
    fn new(hub: &'a Hub) -> Result<Self, String> {
        let t = session(hub)?;
        if hub.mode() != AgentMode::Control {
            return Err(STOPPED.into());
        }
        check_state(&t)?;
        let size = t
            .shared
            .lock()
            .unwrap()
            .frame_size()
            .ok_or("Кадр экрана ещё не получен.")?;
        Ok(Self { hub, t, size })
    }

    fn send(&self, events: Events) -> Result<(), String> {
        // Стоп проверяем перед каждой порцией: пользователь мог нажать его посреди действия.
        if !self.hub.control_allowed() {
            return Err(STOPPED.into());
        }
        self.t.activity.touch();
        self.t
            .input
            .send(RdpInputEvent::FastPath(events))
            .map_err(|_| "Сессия закрыта.".to_owned())
    }

    /// Дождаться, пока сессия запишет в сокет всё отправленное раньше.
    fn flush(&self) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.t
            .input
            .send(RdpInputEvent::Flush(tx))
            .map_err(|_| "Сессия закрыта.".to_owned())?;
        match rx.recv_timeout(FLUSH_TIMEOUT) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "Сессия не подтвердила отправку ввода за {} с — возможно, идёт переподключение. \
                 Сделай screenshot и проверь, что дошло.",
                FLUSH_TIMEOUT.as_secs()
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err("Сессия закрыта.".into()),
        }
    }

    /// Отпускания уходят всегда, даже после «Стоп»: иначе на сервере залипнет
    /// кнопка или Ctrl.
    fn release(&self, events: Events) {
        if !events.is_empty() {
            let _ = self.t.input.send(RdpInputEvent::FastPath(events));
        }
    }

    /// Нажать клавиши по порядку, выполнить `f` и отпустить их в обратном
    /// порядке — всегда, даже если нажатие или `f` прервались.
    fn holding<T>(
        &self,
        keys: &[Scancode],
        f: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut pressed = Vec::new();
        let mut res = Ok(());
        for &k in keys {
            res = self.send(k.events(true));
            if res.is_err() {
                break;
            }
            pressed.push(k);
            pause(15);
        }
        let out = res.and_then(|()| f());
        for k in pressed.into_iter().rev() {
            self.release(k.events(false));
            pause(10);
        }
        out
    }

    fn point(&self, args: &Value, kx: &str, ky: &str) -> Result<(u16, u16), String> {
        to_remote(self.size, num(args, kx)?, num(args, ky)?)
    }

    fn mouse(flags: PointerFlags, wheel: i16, (x, y): (u16, u16)) -> FastPathInputEvent {
        FastPathInputEvent::MouseEvent(MousePdu {
            flags,
            number_of_wheel_rotation_units: wheel,
            x_position: x,
            y_position: y,
        })
    }

    fn move_to(&self, p: (u16, u16)) -> Result<(), String> {
        self.send(SmallVec::from_elem(
            Self::mouse(PointerFlags::MOVE, 0, p),
            1,
        ))
    }

    fn click(&self, args: &Value) -> Result<Vec<Value>, String> {
        let p = self.point(args, "x", "y")?;
        let btn = button(args)?;
        let clicks = int_or(args, "clicks", 1, 1, 3)?;
        let mods = modifiers(args)?;
        self.hub.note_action(Some((p.0, p.1, true)));
        self.move_to(p)?;
        pause(30);
        self.holding(&mods, || {
            for i in 0..clicks {
                if i > 0 {
                    pause(60);
                }
                self.send(SmallVec::from_elem(
                    Self::mouse(btn | PointerFlags::DOWN, 0, p),
                    1,
                ))?;
                pause(25);
                self.release(SmallVec::from_elem(Self::mouse(btn, 0, p), 1));
            }
            Ok(())
        })?;
        let what = match clicks {
            2 => "двойной клик",
            3 => "тройной клик",
            _ => "клик",
        };
        self.hub.log(
            format!(
                "{what}{} ({}, {}) → экран ({}, {})",
                mods_label(args),
                args["x"],
                args["y"],
                p.0,
                p.1
            ),
            false,
        );
        ok(format!(
            "{what} выполнен. Сделай screenshot, чтобы проверить результат."
        ))
    }

    fn move_mouse(&self, args: &Value) -> Result<Vec<Value>, String> {
        let p = self.point(args, "x", "y")?;
        self.hub.note_action(Some((p.0, p.1, false)));
        self.move_to(p)?;
        self.hub
            .log(format!("курсор ({}, {})", args["x"], args["y"]), false);
        ok("Курсор перемещён.")
    }

    fn drag(&self, args: &Value) -> Result<Vec<Value>, String> {
        let from = self.point(args, "x", "y")?;
        let to = self.point(args, "to_x", "to_y")?;
        let btn = button(args)?;
        let mods = modifiers(args)?;
        self.hub.note_action(Some((from.0, from.1, true)));
        self.move_to(from)?;
        pause(30);
        self.holding(&mods, || {
            self.send(SmallVec::from_elem(
                Self::mouse(btn | PointerFlags::DOWN, 0, from),
                1,
            ))?;
            pause(80);
            let mut at = from;
            let mut res = Ok(());
            const STEPS: i32 = 12;
            for i in 1..=STEPS {
                let lerp = |a: u16, b: u16| {
                    (i32::from(a) + (i32::from(b) - i32::from(a)) * i / STEPS) as u16
                };
                let p = (lerp(from.0, to.0), lerp(from.1, to.1));
                res = self.move_to(p);
                if res.is_err() {
                    break;
                }
                at = p;
                self.hub.note_action(Some((p.0, p.1, false)));
                pause(20);
            }
            if res.is_ok() {
                pause(60);
            }
            // Кнопку отпускаем всегда — там, где остановились: после «Стоп» не бросаем
            // перетаскиваемое в конечную точку.
            self.release(SmallVec::from_elem(Self::mouse(btn, 0, at), 1));
            res
        })?;
        self.hub.note_action(Some((to.0, to.1, true)));
        self.hub.log(
            format!(
                "перетаскивание ({}, {}) → ({}, {})",
                args["x"], args["y"], args["to_x"], args["to_y"]
            ),
            false,
        );
        ok("Перетаскивание выполнено.")
    }

    fn scroll(&self, args: &Value) -> Result<Vec<Value>, String> {
        let p = self.point(args, "x", "y")?;
        let down = int_or(args, "down", 0, -50, 50)?;
        let right = int_or(args, "right", 0, -50, 50)?;
        if down == 0 && right == 0 {
            return Err("Укажи down или right".into());
        }
        self.hub.note_action(Some((p.0, p.1, false)));
        self.move_to(p)?;
        // В RDP положительное вертикальное колесо — вверх, горизонтальное — вправо.
        for (n, flag, sign) in [
            (down, PointerFlags::VERTICAL_WHEEL, -1i16),
            (right, PointerFlags::HORIZONTAL_WHEEL, 1),
        ] {
            let units = if n > 0 {
                WHEEL_NOTCH * sign
            } else {
                -WHEEL_NOTCH * sign
            };
            for _ in 0..n.unsigned_abs() {
                self.send(SmallVec::from_elem(Self::mouse(flag, units, p), 1))?;
                pause(15);
            }
        }
        self.hub.log(
            format!(
                "прокрутка в ({}, {}): вниз {down}, вправо {right}",
                args["x"], args["y"]
            ),
            false,
        );
        ok("Прокрутил.")
    }

    fn type_text(&self, args: &Value) -> Result<Vec<Value>, String> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or("Нужен строковый параметр «text»")?;
        let count = text.chars().count();
        if count == 0 {
            return Err("«text» пустой".into());
        }
        if count > MAX_TEXT_CHARS {
            return Err(format!("Не больше {MAX_TEXT_CHARS} символов за раз"));
        }
        self.hub.note_action(None);
        let mut units = [0u16; 2];
        for ch in text.chars() {
            match ch {
                '\r' => continue,
                '\n' => self.tap(ENTER)?,
                '\t' => self.tap(TAB)?,
                _ => {
                    for &u in ch.encode_utf16(&mut units).iter() {
                        self.send(scancode::unicode_events(u, true))?;
                        self.release(scancode::unicode_events(u, false));
                    }
                }
            }
            pause(4);
        }
        // Содержимое в журнал не пишем: там может оказаться что-то личное.
        self.hub
            .log(format!("напечатан текст ({count} симв.)"), false);
        ok(format!(
            "Напечатано {count} символов. Сделай screenshot, чтобы проверить результат."
        ))
    }

    /// Нажать и отпустить одну клавишу.
    fn tap(&self, key: Scancode) -> Result<(), String> {
        self.holding(&[key], || Ok(()))
    }

    fn press_keys(&self, args: &Value) -> Result<Vec<Value>, String> {
        let combo = args
            .get("keys")
            .and_then(Value::as_str)
            .ok_or("Нужен строковый параметр «keys», например «ctrl+c»")?;
        let keys = scancode::parse_combo(combo)?;
        let repeat = int_or(args, "repeat", 1, 1, 50)?;
        self.hub.note_action(None);
        for i in 0..repeat {
            if i > 0 {
                pause(40);
            }
            self.holding(&keys, || {
                pause(20);
                Ok(())
            })?;
        }
        let times = if repeat > 1 {
            format!(" ×{repeat}")
        } else {
            String::new()
        };
        self.hub.log(format!("клавиши {combo}{times}"), false);
        ok(format!(
            "Нажато {combo}{times}. Сделай screenshot, чтобы проверить результат."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keepalive::Activity;
    use crate::session::Shared;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    type Rx = std::sync::mpsc::Receiver<SmallVec<[FastPathInputEvent; 2]>>;

    /// Вместо цикла сессии: подтверждает Flush и пересылает ввод в тест.
    fn pump(mut rx: mpsc::UnboundedReceiver<RdpInputEvent>) -> Rx {
        let (tx, out) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Some(ev) = rx.blocking_recv() {
                match ev {
                    RdpInputEvent::FastPath(evs) => {
                        let _ = tx.send(evs);
                    }
                    RdpInputEvent::Flush(ack) => {
                        let _ = ack.send(());
                    }
                    _ => {}
                }
            }
        });
        out
    }

    fn setup(mode: AgentMode, size: [usize; 2]) -> (Hub, Rx) {
        let hub = Hub::new(|| {});
        hub.set_mode(mode);
        let mut sh = Shared::default();
        let rgba = [10u8, 20, 30, 255].repeat(size[0] * size[1]);
        sh.apply_region(size, 0, 0, size[0], size[1], rgba);
        sh.state = Some(State::Connected);
        let (tx, rx) = mpsc::unbounded_channel();
        hub.set_target(Some(Target {
            shared: Arc::new(Mutex::new(sh)),
            input: tx,
            activity: Activity::default(),
        }));
        (hub, pump(rx))
    }

    /// Вызовы синхронны и ждут Flush, так что к этому моменту всё уже переслано.
    fn drain(rx: &mut Rx) -> Vec<FastPathInputEvent> {
        rx.try_iter().flatten().collect()
    }

    #[test]
    fn click_fails_when_session_closed() {
        let (hub, _rx) = setup(AgentMode::Control, [100, 100]);
        let target = super::session(&hub).unwrap();
        let (tx, dead) = mpsc::unbounded_channel();
        drop(dead);
        hub.set_target(Some(Target {
            input: tx,
            ..target
        }));
        let out = call(&hub, "click", &json!({"x": 1, "y": 1})).unwrap();
        assert!(out.is_error);
        assert!(
            out.content[0]["text"].as_str().unwrap().contains("закрыта"),
            "{:?}",
            out.content
        );
    }

    #[test]
    fn shot_size_respects_limits() {
        assert_eq!(shot_size([1024, 768]), (1024, 768));
        let (w, h) = shot_size([2560, 1440]);
        assert!(
            w <= 1568 && f64::from(w * h) <= MAX_PIXELS + 2000.0,
            "{w}x{h}"
        );
        assert!((f64::from(w) / f64::from(h) - 16.0 / 9.0).abs() < 0.01);
    }

    #[test]
    fn maps_shot_coordinates_to_remote() {
        let size = [2560, 1440];
        let (sw, sh) = shot_size(size);
        assert_eq!(to_remote(size, 0.0, 0.0).unwrap(), (0, 0));
        let (x, y) = to_remote(size, f64::from(sw) - 1.0, f64::from(sh) - 1.0).unwrap();
        assert!(x >= 2557 && y >= 1437, "{x},{y}");
        assert!(to_remote(size, f64::from(sw), 0.0).is_err());
        assert!(to_remote(size, -1.0, 0.0).is_err());
    }

    #[test]
    fn screenshot_returns_png_of_shot_size() {
        let (hub, _rx) = setup(AgentMode::View, [1600, 900]);
        let out = call(&hub, "screenshot", &json!({})).unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(out.content[0]["mimeType"], "image/png");
        let png = base64::engine::general_purpose::STANDARD
            .decode(out.content[0]["data"].as_str().unwrap())
            .unwrap();
        let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        assert_eq!((img.width(), img.height()), shot_size([1600, 900]));
        assert!(
            out.content[1]["text"]
                .as_str()
                .unwrap()
                .contains("1600×900")
        );
    }

    #[test]
    fn view_mode_blocks_control_but_allows_screenshot() {
        let (hub, mut rx) = setup(AgentMode::View, [100, 100]);
        let out = call(&hub, "click", &json!({"x": 10, "y": 10})).unwrap();
        assert!(out.is_error);
        assert!(drain(&mut rx).is_empty());
        assert!(!call(&hub, "screenshot", &json!({})).unwrap().is_error);
        hub.set_mode(AgentMode::Off);
        assert!(call(&hub, "screenshot", &json!({})).unwrap().is_error);
    }

    #[test]
    fn double_click_sends_balanced_events() {
        let (hub, mut rx) = setup(AgentMode::Control, [200, 100]);
        let out = call(&hub, "click", &json!({"x": 50, "y": 20, "clicks": 2})).unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        let evs = drain(&mut rx);
        let downs = evs
            .iter()
            .filter(|e| matches!(e, FastPathInputEvent::MouseEvent(m) if m.flags.contains(PointerFlags::DOWN)))
            .count();
        assert_eq!(downs, 2);
        assert!(evs.iter().any(|e| matches!(e, FastPathInputEvent::MouseEvent(m) if m.x_position == 50 && m.y_position == 20)));
        let p = hub.pointer().unwrap();
        assert_eq!((p.x, p.y, p.click), (50, 20, true));
    }

    #[test]
    fn scroll_direction() {
        let (hub, mut rx) = setup(AgentMode::Control, [100, 100]);
        assert!(
            !call(&hub, "scroll", &json!({"x": 1, "y": 1, "down": 2}))
                .unwrap()
                .is_error
        );
        let wheel: Vec<i16> = drain(&mut rx)
            .iter()
            .filter_map(|e| match e {
                FastPathInputEvent::MouseEvent(m)
                    if m.flags.contains(PointerFlags::VERTICAL_WHEEL) =>
                {
                    Some(m.number_of_wheel_rotation_units)
                }
                _ => None,
            })
            .collect();
        assert_eq!(wheel, vec![-120, -120]);
    }

    #[test]
    fn zoom_crops_at_full_resolution() {
        let (hub, _rx) = setup(AgentMode::View, [3000, 2000]);
        let out = call(
            &hub,
            "zoom",
            &json!({"x": 0, "y": 0, "width": 100, "height": 50}),
        )
        .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        let png = base64::engine::general_purpose::STANDARD
            .decode(out.content[0]["data"].as_str().unwrap())
            .unwrap();
        let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        // Скриншот уменьшен, лупа — нет: фрагмент крупнее, чем был на скриншоте.
        assert!(img.width() > 100, "{}", img.width());
    }

    #[test]
    fn stop_mid_drag_releases_button_where_it_stopped() {
        let (hub, mut rx) = setup(AgentMode::Control, [1000, 100]);
        let stopper = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                hub.set_mode(AgentMode::View);
            })
        };
        let out = call(
            &hub,
            "drag",
            &json!({"x": 0, "y": 50, "to_x": 999, "to_y": 50}),
        )
        .unwrap();
        stopper.join().unwrap();
        assert!(out.is_error);
        let mouse: Vec<MousePdu> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                FastPathInputEvent::MouseEvent(m) => Some(m),
                _ => None,
            })
            .collect();
        let last = mouse.last().unwrap();
        assert!(last.flags.contains(PointerFlags::LEFT_BUTTON));
        assert!(!last.flags.contains(PointerFlags::DOWN));
        assert!(last.x_position < 999, "отпустили в конечной точке");
    }

    #[test]
    fn no_empty_batches() {
        let (hub, rx) = setup(AgentMode::Control, [100, 100]);
        assert!(
            !call(&hub, "click", &json!({"x": 1, "y": 1}))
                .unwrap()
                .is_error
        );
        for evs in rx.try_iter() {
            assert!(!evs.is_empty());
        }
    }

    #[test]
    fn unknown_tool_is_none() {
        let (hub, _rx) = setup(AgentMode::Control, [10, 10]);
        assert!(call(&hub, "rm_rf", &json!({})).is_none());
    }

    /// (код, нажатие) клавиатурных событий.
    fn keys(evs: &[FastPathInputEvent]) -> Vec<(u8, bool)> {
        use ironrdp_pdu::input::fast_path::KeyboardFlags;
        evs.iter()
            .filter_map(|e| match e {
                FastPathInputEvent::KeyboardEvent(f, c) => {
                    Some((*c, !f.contains(KeyboardFlags::RELEASE)))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn press_keys_is_balanced_and_ordered() {
        let (hub, mut rx) = setup(AgentMode::Control, [100, 100]);
        let out = call(&hub, "press_keys", &json!({"keys": "ctrl+shift+esc"})).unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            keys(&drain(&mut rx)),
            vec![
                (0x1D, true),
                (0x2A, true),
                (0x01, true),
                (0x01, false),
                (0x2A, false),
                (0x1D, false)
            ]
        );
        let bad = call(&hub, "press_keys", &json!({"keys": "ctrl+nope"})).unwrap();
        assert!(bad.is_error);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn type_text_sends_unicode_and_enter() {
        use ironrdp_pdu::input::fast_path::KeyboardFlags;
        let (hub, mut rx) = setup(AgentMode::Control, [100, 100]);
        let out = call(&hub, "type_text", &json!({"text": "Я😀\n"})).unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        let evs = drain(&mut rx);
        let units: Vec<u16> = evs
            .iter()
            .filter_map(|e| match e {
                FastPathInputEvent::UnicodeKeyboardEvent(f, u)
                    if !f.contains(KeyboardFlags::RELEASE) =>
                {
                    Some(*u)
                }
                _ => None,
            })
            .collect();
        assert_eq!(units, "Я😀".encode_utf16().collect::<Vec<_>>());
        assert_eq!(keys(&evs), vec![(0x1C, true), (0x1C, false)]);
        // Содержимое не попадает в журнал.
        assert!(!hub.log_entries()[0].text.contains('Я'));
    }

    #[test]
    fn click_with_modifiers_releases_them() {
        let (hub, mut rx) = setup(AgentMode::Control, [100, 100]);
        let out = call(
            &hub,
            "click",
            &json!({"x": 5, "y": 5, "modifiers": ["ctrl", "shift"]}),
        )
        .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        let evs = drain(&mut rx);
        assert_eq!(
            keys(&evs),
            vec![(0x1D, true), (0x2A, true), (0x2A, false), (0x1D, false)]
        );
        // Клик — между нажатием и отпусканием модификаторов.
        let first_mouse_down = evs
            .iter()
            .position(|e| matches!(e, FastPathInputEvent::MouseEvent(m) if m.flags.contains(PointerFlags::DOWN)))
            .unwrap();
        let shift_down = evs
            .iter()
            .position(|e| matches!(e, FastPathInputEvent::KeyboardEvent(_, 0x2A)))
            .unwrap();
        assert!(shift_down < first_mouse_down);
        let bad = call(
            &hub,
            "click",
            &json!({"x": 5, "y": 5, "modifiers": ["hyper"]}),
        )
        .unwrap();
        assert!(bad.is_error);
    }

    #[test]
    fn view_mode_blocks_keyboard() {
        let (hub, mut rx) = setup(AgentMode::View, [100, 100]);
        assert!(
            call(&hub, "type_text", &json!({"text": "x"}))
                .unwrap()
                .is_error
        );
        assert!(
            call(&hub, "press_keys", &json!({"keys": "enter"}))
                .unwrap()
                .is_error
        );
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn stop_mid_typing_leaves_nothing_pressed() {
        let (hub, mut rx) = setup(AgentMode::Control, [100, 100]);
        let stopper = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                hub.set_mode(AgentMode::View);
            })
        };
        let out = call(&hub, "type_text", &json!({"text": "a".repeat(500)})).unwrap();
        stopper.join().unwrap();
        assert!(out.is_error);
        use ironrdp_pdu::input::fast_path::KeyboardFlags;
        let mut balance = 0i32;
        for e in drain(&mut rx) {
            if let FastPathInputEvent::UnicodeKeyboardEvent(f, _) = e {
                balance += if f.contains(KeyboardFlags::RELEASE) {
                    -1
                } else {
                    1
                };
            }
        }
        assert_eq!(balance, 0);
    }
}
