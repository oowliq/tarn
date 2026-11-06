//! Настройки экрана, качества и производительности подключения.

use ironrdp_pdu::rdp::client_info::PerformanceFlags;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DisplaySettings {
    /// Глубина цвета: 32 или 16 бит.
    pub color_depth: u32,
    /// Оформление удалённого стола.
    pub wallpaper: bool,
    pub themes: bool,
    pub font_smoothing: bool,
    pub desktop_composition: bool,
    pub full_window_drag: bool,
    pub menu_animations: bool,
    pub cursor_shadow: bool,
    /// Сжатие трафика (MPPC). Экономит канал, добавляет нагрузку на CPU.
    pub compression: bool,
    /// Использовать полное разрешение Retina/HiDPI (иначе — логическое, в 2–4 раза меньше пикселей).
    pub hidpi: bool,
    /// Подстраивать разрешение удалённого стола под размер окна.
    pub auto_resize: bool,
    /// Фиксированное разрешение, если `auto_resize` выключен.
    pub fixed_width: u16,
    pub fixed_height: u16,
    /// Сглаживание при масштабировании картинки.
    pub smooth_scaling: bool,
    /// Графический конвейер (GFX): целые кадры без тиринга. Только для 32 бит.
    pub gfx: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Fast,
    Balanced,
    Best,
}

impl Preset {
    pub const ALL: [Preset; 3] = [Preset::Fast, Preset::Balanced, Preset::Best];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Fast => "Быстро",
            Preset::Balanced => "Сбалансированно",
            Preset::Best => "Красиво",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Preset::Fast => "Медленный канал: 16 бит, без эффектов и Retina, со сжатием",
            Preset::Balanced => "Обычная сеть: сглаживание шрифтов и темы, без обоев и анимаций",
            Preset::Best => "Быстрая сеть: все эффекты Windows, обои, Retina",
        }
    }
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self::preset(Preset::Balanced)
    }
}

impl DisplaySettings {
    pub fn preset(p: Preset) -> Self {
        let best = p == Preset::Best;
        let fast = p == Preset::Fast;
        Self {
            color_depth: if fast { 16 } else { 32 },
            wallpaper: best,
            themes: !fast,
            font_smoothing: !fast,
            desktop_composition: best,
            full_window_drag: best,
            menu_animations: best,
            cursor_shadow: best,
            compression: fast,
            hidpi: !fast,
            auto_resize: true,
            fixed_width: 1920,
            fixed_height: 1080,
            smooth_scaling: true,
            gfx: true,
        }
    }

    /// Какой пресет совпадает с текущими настройками (без учёта разрешения).
    pub fn matching_preset(&self) -> Option<Preset> {
        Preset::ALL.into_iter().find(|&p| {
            let mut x = Self::preset(p);
            x.auto_resize = self.auto_resize;
            x.fixed_width = self.fixed_width;
            x.fixed_height = self.fixed_height;
            x.smooth_scaling = self.smooth_scaling;
            x.gfx = self.gfx;
            x == *self
        })
    }

    pub fn apply_preset(&mut self, p: Preset) {
        let keep = (
            self.auto_resize,
            self.fixed_width,
            self.fixed_height,
            self.smooth_scaling,
            self.gfx,
        );
        *self = Self::preset(p);
        (
            self.auto_resize,
            self.fixed_width,
            self.fixed_height,
            self.smooth_scaling,
            self.gfx,
        ) = keep;
    }

    pub fn performance_flags(&self) -> PerformanceFlags {
        let mut f = PerformanceFlags::empty();
        f.set(PerformanceFlags::DISABLE_WALLPAPER, !self.wallpaper);
        f.set(PerformanceFlags::DISABLE_THEMING, !self.themes);
        f.set(
            PerformanceFlags::DISABLE_FULLWINDOWDRAG,
            !self.full_window_drag,
        );
        f.set(
            PerformanceFlags::DISABLE_MENUANIMATIONS,
            !self.menu_animations,
        );
        f.set(PerformanceFlags::DISABLE_CURSOR_SHADOW, !self.cursor_shadow);
        f.set(PerformanceFlags::ENABLE_FONT_SMOOTHING, self.font_smoothing);
        f.set(
            PerformanceFlags::ENABLE_DESKTOP_COMPOSITION,
            self.desktop_composition,
        );
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_detected() {
        for p in Preset::ALL {
            assert_eq!(DisplaySettings::preset(p).matching_preset(), Some(p));
        }
        let mut s = DisplaySettings::preset(Preset::Best);
        s.wallpaper = false;
        assert_eq!(s.matching_preset(), None);
        s.apply_preset(Preset::Fast);
        assert_eq!(s.matching_preset(), Some(Preset::Fast));
    }

    #[test]
    fn flags_follow_settings() {
        let f = DisplaySettings::preset(Preset::Fast).performance_flags();
        assert!(
            f.contains(PerformanceFlags::DISABLE_WALLPAPER | PerformanceFlags::DISABLE_THEMING)
        );
        assert!(!f.contains(PerformanceFlags::ENABLE_FONT_SMOOTHING));
        let f = DisplaySettings::preset(Preset::Best).performance_flags();
        assert!(f.contains(
            PerformanceFlags::ENABLE_FONT_SMOOTHING | PerformanceFlags::ENABLE_DESKTOP_COMPOSITION
        ));
        assert!(!f.contains(PerformanceFlags::DISABLE_WALLPAPER));
    }
}
