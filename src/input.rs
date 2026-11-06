//! Перевод событий мыши egui в события RDP fast-path с учётом зажатых кнопок.

use std::collections::HashSet;

use egui::{Event, MouseWheelUnit, PointerButton, Pos2, Rect};
use ironrdp_pdu::input::MousePdu;
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use ironrdp_pdu::input::mouse::PointerFlags;
use smallvec::SmallVec;

pub type Events = SmallVec<[FastPathInputEvent; 2]>;

/// Единиц колеса на один «щелчок» (как в Windows).
const WHEEL_DELTA: f32 = 120.0;
/// Максимум за одно событие (поле 9-битное со знаком).
const WHEEL_MAX: i16 = 240;

#[derive(Default)]
pub struct InputState {
    pressed_buttons: HashSet<u8>,
    last_pos: (u16, u16),
    wheel_acc: (f32, f32),
}

/// Где на экране нарисован удалённый рабочий стол и какого он размера в пикселях.
pub struct Viewport {
    pub rect: Rect,
    pub remote_size: (u16, u16),
}

impl Viewport {
    fn map(&self, pos: Pos2) -> Option<(u16, u16)> {
        if !self.rect.contains(pos) || self.rect.width() <= 0.0 || self.rect.height() <= 0.0 {
            return None;
        }
        let fx = (pos.x - self.rect.min.x) / self.rect.width();
        let fy = (pos.y - self.rect.min.y) / self.rect.height();
        let max_x = self.remote_size.0.saturating_sub(1);
        let max_y = self.remote_size.1.saturating_sub(1);
        let x = (fx * f32::from(self.remote_size.0)).clamp(0.0, f32::from(max_x)) as u16;
        let y = (fy * f32::from(self.remote_size.1)).clamp(0.0, f32::from(max_y)) as u16;
        Some((x, y))
    }
}

fn button_flag(b: PointerButton) -> Option<(u8, PointerFlags)> {
    match b {
        PointerButton::Primary => Some((1, PointerFlags::LEFT_BUTTON)),
        PointerButton::Secondary => Some((2, PointerFlags::RIGHT_BUTTON)),
        PointerButton::Middle => Some((3, PointerFlags::MIDDLE_BUTTON_OR_WHEEL)),
        _ => None,
    }
}

impl InputState {
    fn mouse(&self, flags: PointerFlags, wheel: i16) -> FastPathInputEvent {
        FastPathInputEvent::MouseEvent(MousePdu {
            flags,
            number_of_wheel_rotation_units: wheel,
            x_position: self.last_pos.0,
            y_position: self.last_pos.1,
        })
    }

    /// Возвращает события мыши; клавиатура не передаётся.
    /// `pointer_over` — указатель над картинкой и не перекрыт другими окнами egui.
    pub fn translate(&mut self, events: &[Event], vp: &Viewport, pointer_over: bool) -> Events {
        let mut out = Events::new();
        for ev in events {
            match ev {
                Event::PointerMoved(pos) if pointer_over => {
                    if let Some(p) = vp.map(*pos)
                        && p != self.last_pos
                    {
                        self.last_pos = p;
                        out.push(self.mouse(PointerFlags::MOVE, 0));
                    }
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    let Some((id, flag)) = button_flag(*button) else {
                        continue;
                    };
                    if *pressed {
                        if !pointer_over {
                            continue;
                        }
                        let Some(p) = vp.map(*pos) else { continue };
                        self.last_pos = p;
                        self.pressed_buttons.insert(id);
                        out.push(self.mouse(flag | PointerFlags::DOWN, 0));
                    } else if self.pressed_buttons.remove(&id) {
                        if let Some(p) = vp.map(*pos) {
                            self.last_pos = p;
                        }
                        out.push(self.mouse(flag, 0));
                    }
                }
                Event::MouseWheel { unit, delta, .. } if pointer_over => {
                    let factor = match unit {
                        MouseWheelUnit::Point => WHEEL_DELTA / 40.0,
                        MouseWheelUnit::Line => WHEEL_DELTA,
                        MouseWheelUnit::Page => WHEEL_DELTA * 3.0,
                    };
                    self.wheel_acc.0 -= delta.x * factor;
                    self.wheel_acc.1 += delta.y * factor;
                    self.flush_wheel(&mut out);
                }
                _ => {}
            }
        }
        out
    }

    fn flush_wheel(&mut self, out: &mut Events) {
        for horizontal in [false, true] {
            let acc = if horizontal {
                &mut self.wheel_acc.0
            } else {
                &mut self.wheel_acc.1
            };
            let mut pending = Vec::new();
            while acc.abs() >= 1.0 {
                let units = acc
                    .trunc()
                    .clamp(-f32::from(WHEEL_MAX), f32::from(WHEEL_MAX))
                    as i16;
                *acc -= f32::from(units);
                pending.push(units);
            }
            let dir = if horizontal {
                PointerFlags::HORIZONTAL_WHEEL
            } else {
                PointerFlags::VERTICAL_WHEEL
            };
            for units in pending {
                out.push(self.mouse(dir, units));
            }
        }
    }

    /// Отпустить зажатые кнопки мыши.
    pub fn release_all(&mut self) -> Events {
        let mut out = Events::new();
        for id in std::mem::take(&mut self.pressed_buttons) {
            let flag = match id {
                1 => PointerFlags::LEFT_BUTTON,
                2 => PointerFlags::RIGHT_BUTTON,
                _ => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
            };
            out.push(self.mouse(flag, 0));
        }
        self.wheel_acc = (0.0, 0.0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Modifiers, pos2, vec2};

    fn vp() -> Viewport {
        Viewport {
            rect: Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 50.0)),
            remote_size: (200, 100),
        }
    }

    #[test]
    fn maps_coordinates_with_scaling() {
        assert_eq!(vp().map(pos2(50.0, 25.0)), Some((100, 50)));
        assert_eq!(vp().map(pos2(100.0, 50.0)), Some((199, 99)));
        assert_eq!(vp().map(pos2(150.0, 25.0)), None);
    }

    #[test]
    fn mouse_button_released_after_focus_loss() {
        let mut st = InputState::default();
        let press = Event::PointerButton {
            pos: pos2(10.0, 10.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(st.translate(&[press], &vp(), true).len(), 1);
        assert_eq!(st.release_all().len(), 1);
        assert!(st.release_all().is_empty());
    }

    #[test]
    fn wheel_accumulates() {
        let mut st = InputState::default();
        let wheel = |dy: f32| Event::MouseWheel {
            unit: MouseWheelUnit::Line,
            delta: vec2(0.0, dy),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::NONE,
            source: egui::MouseWheelSource::Wheel,
        };
        let ev = st.translate(&[wheel(1.0)], &vp(), true);
        assert!(
            matches!(ev[0], FastPathInputEvent::MouseEvent(m) if m.number_of_wheel_rotation_units == 120)
        );
        let ev = st.translate(&[wheel(-3.0)], &vp(), true);
        let total: i32 = ev
            .iter()
            .map(|e| match e {
                FastPathInputEvent::MouseEvent(m) => i32::from(m.number_of_wheel_rotation_units),
                _ => 0,
            })
            .sum();
        assert_eq!(total, -360);
    }
}
