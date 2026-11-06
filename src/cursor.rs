//! Курсор удалённой машины как системный курсор окна.

use std::sync::Arc;

use egui::CustomCursorImage;

/// Во сколько раз увеличить картинку курсора (в пикселях удалённого экрана),
/// чтобы он был того же размера, что и удалённый стол на экране.
///
/// `display_scale` — сколько точек egui занимает один пиксель удалённого
/// экрана. На macOS размер курсора задаётся в точках, на Windows/Linux —
/// в физических пикселях.
pub fn os_scale(display_scale: f32, native_ppp: f32) -> f32 {
    if cfg!(target_os = "macos") {
        display_scale
    } else {
        display_scale * native_ppp
    }
}

/// Курсор в нужном масштабе. Почти 1:1 — та же картинка (тот же `Arc`, чтобы
/// egui-winit не создавал системный курсор заново каждый кадр).
pub fn scaled(img: &CustomCursorImage, factor: f32) -> CustomCursorImage {
    if !factor.is_finite() || factor <= 0.0 || (factor - 1.0).abs() < 0.05 {
        return img.clone();
    }
    let [w, h] = img.size.map(u32::from);
    let nw = ((w as f32 * factor).round() as u32).clamp(1, 256);
    let nh = ((h as f32 * factor).round() as u32).clamp(1, 256);
    if (nw, nh) == (w, h) {
        return img.clone();
    }
    // Масштабируем с premultiply, иначе по краям прозрачных областей грязь.
    let mut buf = img.rgba.to_vec();
    for px in buf.as_chunks_mut::<4>().0 {
        let a = u16::from(px[3]);
        for c in &mut px[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    let Some(src) = image::RgbaImage::from_raw(w, h, buf) else {
        return img.clone();
    };
    let mut out = image::imageops::resize(&src, nw, nh, image::imageops::FilterType::Triangle);
    for px in out.pixels_mut() {
        let a = u16::from(px[3]);
        for c in &mut px.0[..3] {
            if let Some(v) = (u16::from(*c) * 255 + a / 2).checked_div(a) {
                *c = v.min(255) as u8;
            }
        }
    }
    let hot = |v: u16, max: u32| ((f32::from(v) * factor).round() as u32).min(max - 1) as u16;
    CustomCursorImage {
        rgba: Arc::from(out.into_raw()),
        size: [nw as u16, nh as u16],
        hotspot: [hot(img.hotspot[0], nw), hot(img.hotspot[1], nh)],
    }
}

/// Последний отмасштабированный курсор, чтобы не пересчитывать каждый кадр.
#[derive(Default)]
pub struct Cache {
    key: Option<(Arc<[u8]>, f32)>,
    image: Option<CustomCursorImage>,
}

impl Cache {
    pub fn get(&mut self, img: &CustomCursorImage, factor: f32) -> CustomCursorImage {
        let hit = self
            .key
            .as_ref()
            .is_some_and(|(src, f)| Arc::ptr_eq(src, &img.rgba) && (*f - factor).abs() < 0.01);
        if !hit || self.image.is_none() {
            self.key = Some((img.rgba.clone(), factor));
            self.image = Some(scaled(img, factor));
        }
        self.image.clone().expect("just set")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(size: u16, hotspot: u16) -> CustomCursorImage {
        CustomCursorImage {
            rgba: [200u8, 100, 50, 255]
                .repeat(usize::from(size) * usize::from(size))
                .into(),
            size: [size, size],
            hotspot: [hotspot, hotspot],
        }
    }

    #[test]
    fn keeps_same_bitmap_at_one_to_one() {
        let c = cursor(32, 4);
        assert!(Arc::ptr_eq(&scaled(&c, 1.02).rgba, &c.rgba));
    }

    #[test]
    fn halves_retina_cursor() {
        let s = scaled(&cursor(64, 10), 0.5);
        assert_eq!(s.size, [32, 32]);
        assert_eq!(s.hotspot, [5, 5]);
        assert_eq!(s.rgba.len(), 32 * 32 * 4);
        assert_eq!(&s.rgba[..4], &[200, 100, 50, 255]);
    }

    #[test]
    fn cache_reuses_scaled_bitmap() {
        let c = cursor(64, 0);
        let mut cache = Cache::default();
        let a = cache.get(&c, 0.5);
        let b = cache.get(&c, 0.5);
        assert!(Arc::ptr_eq(&a.rgba, &b.rgba));
        let d = cache.get(&c, 0.75);
        assert_eq!(d.size, [48, 48]);
    }
}
