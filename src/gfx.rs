//! Графический конвейер RDP (GFX, [MS-RDPEGFX]) без H.264.
//!
//! Windows 11 без GFX шлёт старые bitmap-обновления полосами и без границ
//! кадра, поэтому при перетаскивании окон виден тиринг. В GFX каждый кадр
//! обрамлён StartFrame/EndFrame: мы собираем его на своих поверхностях и
//! отдаём в [`Shared`] целиком по EndFrame.
//!
//! Кодеки: RemoteFX Progressive, ClearCodec, Planar, несжатый; плюс заливка,
//! копирование прямоугольников и кэш. H.264 (AVC) не объявляем.
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use ironrdp_egfx::client::{
    BitmapUpdate, GraphicsPipelineClient, GraphicsPipelineHandler, Surface as GfxSurface,
};
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, CapabilitiesV10Flags, CapabilitiesV103Flags, CapabilitiesV104Flags,
    CapabilitiesV107Flags, CapabilitySet, Codec1Type, Codec2Type, DeleteEncodingContextPdu,
    EvictCacheEntryPdu, GfxPdu, SolidFillPdu, SurfaceToCachePdu, SurfaceToSurfacePdu,
    WireToSurface1Pdu, WireToSurface2Pdu,
};
// Декодеры из ironrdp-graphics 0.11: в 0.9 они не справлялись с потоками Windows
// (плитки «полного качества», V-Bar в ClearCodec, SRL и т. д.).
use ironrdp_graphics_next::clearcodec::ClearCodecDecoder;
use ironrdp_graphics_next::progressive::ProgressiveDecoder;
use ironrdp_graphics_next::rdp6::BitmapStreamDecoder;
use ironrdp_pdu::geometry::ExclusiveRectangle;

use crate::session::{Shared, State};

/// Канал GFX для одного подключения.
pub fn client(shared: Arc<Mutex<Shared>>, ctx: egui::Context) -> GraphicsPipelineClient {
    GraphicsPipelineClient::new(Box::new(Handler::new(shared, ctx)), None)
}

/// Прямоугольник `[l, r) × [t, b)` в пикселях.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    l: usize,
    t: usize,
    r: usize,
    b: usize,
}

impl Rect {
    fn new(l: usize, t: usize, w: usize, h: usize) -> Self {
        Self {
            l,
            t,
            r: l + w,
            b: t + h,
        }
    }

    fn from_pdu(r: &ExclusiveRectangle) -> Self {
        Self {
            l: usize::from(r.left),
            t: usize::from(r.top),
            r: usize::from(r.right),
            b: usize::from(r.bottom),
        }
    }

    fn w(self) -> usize {
        self.r.saturating_sub(self.l)
    }

    fn h(self) -> usize {
        self.b.saturating_sub(self.t)
    }

    fn is_empty(self) -> bool {
        self.w() == 0 || self.h() == 0
    }

    fn intersect(self, o: Rect) -> Rect {
        Rect {
            l: self.l.max(o.l),
            t: self.t.max(o.t),
            r: self.r.min(o.r),
            b: self.b.min(o.b),
        }
    }

    fn union(self, o: Rect) -> Rect {
        Rect {
            l: self.l.min(o.l),
            t: self.t.min(o.t),
            r: self.r.max(o.r),
            b: self.b.max(o.b),
        }
    }
}

/// Раскладка входных пикселей.
#[derive(Clone, Copy)]
enum Fmt {
    Rgba,
    Bgra,
    Rgb,
}

impl Fmt {
    fn bpp(self) -> usize {
        match self {
            Fmt::Rgba | Fmt::Bgra => 4,
            Fmt::Rgb => 3,
        }
    }
}

/// Поверхность сервера: RGBA, построчно.
struct Surface {
    w: usize,
    h: usize,
    px: Vec<u8>,
    /// Куда на «экран» она выведена (None — внеэкранная).
    origin: Option<(usize, usize)>,
    /// Что поменялось в текущем кадре.
    dirty: Option<Rect>,
}

impl Surface {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: [0, 0, 0, 255].repeat(w * h),
            origin: None,
            dirty: None,
        }
    }

    fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.w, self.h)
    }

    fn mark(&mut self, r: Rect) {
        let r = r.intersect(self.bounds());
        if !r.is_empty() {
            self.dirty = Some(self.dirty.map_or(r, |d| d.union(r)));
        }
    }

    /// Положить картинку `src` (плотно упакованную, размером `at`) в `at`,
    /// но только внутри `clip`.
    fn put(&mut self, at: Rect, src: &[u8], fmt: Fmt, clip: Rect) {
        let bpp = fmt.bpp();
        let src_stride = at.w() * bpp;
        let r = at.intersect(clip).intersect(self.bounds());
        if r.is_empty() || src.len() < src_stride * at.h() {
            return;
        }
        for y in r.t..r.b {
            let s0 = (y - at.t) * src_stride + (r.l - at.l) * bpp;
            let src_row = &src[s0..s0 + r.w() * bpp];
            let d0 = (y * self.w + r.l) * 4;
            let dst_row = &mut self.px[d0..d0 + r.w() * 4];
            match fmt {
                Fmt::Rgba => {
                    dst_row.copy_from_slice(src_row);
                    for px in dst_row.as_chunks_mut::<4>().0 {
                        px[3] = 255;
                    }
                }
                Fmt::Bgra => {
                    for (d, s) in dst_row
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(src_row.as_chunks::<4>().0)
                    {
                        *d = [s[2], s[1], s[0], 255];
                    }
                }
                Fmt::Rgb => {
                    for (d, s) in dst_row
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(src_row.as_chunks::<3>().0)
                    {
                        *d = [s[0], s[1], s[2], 255];
                    }
                }
            }
        }
        self.mark(r);
    }

    fn fill(&mut self, r: Rect, rgba: [u8; 4]) {
        let r = r.intersect(self.bounds());
        if r.is_empty() {
            return;
        }
        for y in r.t..r.b {
            let d0 = (y * self.w + r.l) * 4;
            for px in self.px[d0..d0 + r.w() * 4].as_chunks_mut::<4>().0 {
                *px = rgba;
            }
        }
        self.mark(r);
    }

    /// Пиксели прямоугольника (обрезанного по поверхности) и сам прямоугольник.
    fn read(&self, r: Rect) -> (Rect, Vec<u8>) {
        let r = r.intersect(self.bounds());
        let mut out = Vec::with_capacity(r.w() * r.h() * 4);
        if !r.is_empty() {
            for y in r.t..r.b {
                let s0 = (y * self.w + r.l) * 4;
                out.extend_from_slice(&self.px[s0..s0 + r.w() * 4]);
            }
        }
        (r, out)
    }
}

struct CacheEntry {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

struct Handler {
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
    /// Размер «экрана» из ResetGraphics.
    output: (usize, usize),
    surfaces: BTreeMap<u16, Surface>,
    cache: HashMap<u16, CacheEntry>,
    progressive: ProgressiveDecoder,
    clear: ClearCodecDecoder,
    planar: BitmapStreamDecoder,
    /// О чём уже предупредили в логе (чтобы не спамить каждый кадр).
    warned: HashSet<String>,
}

impl Handler {
    fn new(shared: Arc<Mutex<Shared>>, ctx: egui::Context) -> Self {
        Self {
            shared,
            ctx,
            output: (0, 0),
            surfaces: BTreeMap::new(),
            cache: HashMap::new(),
            progressive: {
                let mut p = ProgressiveDecoder::new();
                p.begin_frame();
                p
            },
            clear: ClearCodecDecoder::new(),
            planar: BitmapStreamDecoder::default(),
            warned: HashSet::new(),
        }
    }

    fn warn_once(&mut self, key: impl Into<String>, msg: impl std::fmt::Display) {
        if self.warned.insert(key.into()) {
            tracing::warn!("GFX: {msg}");
        }
    }

    fn wire_to_surface1(&mut self, pdu: &WireToSurface1Pdu) {
        let at = Rect::from_pdu(&pdu.destination_rectangle);
        let Some(surface) = self.surfaces.get_mut(&pdu.surface_id) else {
            return;
        };
        let (Ok(w), Ok(h)) = (u16::try_from(at.w()), u16::try_from(at.h())) else {
            return;
        };
        if at.is_empty() {
            return;
        }
        let clip = surface.bounds();
        let result = match pdu.codec_id {
            Codec1Type::ClearCodec => match self.clear.decode(&pdu.bitmap_data, w, h) {
                Ok(bgra) => {
                    surface.put(at, &bgra, Fmt::Bgra, clip);
                    Ok(())
                }
                Err(e) => Err(format!("ClearCodec: {e}")),
            },
            Codec1Type::Planar => {
                let mut rgb = Vec::new();
                match self.planar.decode_bitmap_stream_to_rgb24(
                    &pdu.bitmap_data,
                    &mut rgb,
                    at.w(),
                    at.h(),
                ) {
                    Ok(()) => {
                        surface.put(at, &rgb, Fmt::Rgb, clip);
                        Ok(())
                    }
                    Err(e) => Err(format!("Planar: {e}")),
                }
            }
            other => {
                let other = format!("{other:?}");
                self.warn_once(
                    format!("codec {other}"),
                    format!("кодек {other} не поддерживается"),
                );
                return;
            }
        };
        if let Err(e) = result {
            self.warn_once(e.clone(), e);
        }
    }

    fn wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        // Единственный кодек второго типа — RemoteFX Progressive.
        let Codec2Type::RemoteFxProgressive = pdu.codec_id;
        let Some(surface) = self.surfaces.get_mut(&pdu.surface_id) else {
            return;
        };
        let (Ok(sw), Ok(sh)) = (u16::try_from(surface.w), u16::try_from(surface.h)) else {
            return;
        };
        match self.progressive.decode_bitmap(
            pdu.surface_id,
            pdu.codec_context_id,
            sw,
            sh,
            &pdu.bitmap_data,
        ) {
            Ok(tiles) => {
                for tile in &tiles {
                    let at = Rect::new(
                        usize::from(tile.x_idx) * 64,
                        usize::from(tile.y_idx) * 64,
                        64,
                        64,
                    );
                    // Декодер уже обрезал плитку по прямоугольникам региона.
                    for r in &tile.update_rectangles {
                        let clip = Rect {
                            l: usize::from(r.left),
                            t: usize::from(r.top),
                            r: usize::from(r.right),
                            b: usize::from(r.bottom),
                        };
                        surface.put(at, &tile.pixels, Fmt::Rgba, clip);
                    }
                }
            }
            Err(e) => {
                let e = format!("Progressive: {e}");
                self.warn_once(e.clone(), e);
            }
        }
    }

    /// Кадр готов: изменённые области выведенных поверхностей — в `Shared`.
    fn present(&mut self) {
        let (ow, oh) = self.output;
        let screen = Rect::new(0, 0, ow, oh);
        let mut sh = self.shared.lock().unwrap();
        for s in self.surfaces.values_mut() {
            let (Some((ox, oy)), Some(d)) = (s.origin, s.dirty.take()) else {
                continue;
            };
            let on_screen = Rect::new(ox + d.l, oy + d.t, d.w(), d.h()).intersect(screen);
            if on_screen.is_empty() {
                continue;
            }
            let local = Rect::new(
                on_screen.l - ox,
                on_screen.t - oy,
                on_screen.w(),
                on_screen.h(),
            );
            let (r, px) = s.read(local);
            if r.is_empty() {
                continue;
            }
            sh.apply_region([ow, oh], ox + r.l, oy + r.t, r.w(), r.h(), px);
        }
        sh.frame_end();
        if sh.state == Some(State::Connecting) {
            sh.state = Some(State::Connected);
        }
        drop(sh);
        self.ctx.request_repaint();
    }
}

impl GraphicsPipelineHandler for Handler {
    fn capabilities(&self) -> Vec<CapabilitySet> {
        // Только версии без H.264: тогда сервер берёт Progressive/ClearCodec/Planar.
        let v104 = CapabilitiesV104Flags::SMALL_CACHE | CapabilitiesV104Flags::AVC_DISABLED;
        vec![
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE
                    | CapabilitiesV107Flags::AVC_DISABLED
                    | CapabilitiesV107Flags::SCALEDMAP_DISABLE,
            },
            CapabilitySet::V10_6 { flags: v104 },
            CapabilitySet::V10_5 { flags: v104 },
            CapabilitySet::V10_4 { flags: v104 },
            CapabilitySet::V10_3 {
                flags: CapabilitiesV103Flags::AVC_DISABLED,
            },
            CapabilitySet::V10_2 {
                flags: CapabilitiesV10Flags::SMALL_CACHE | CapabilitiesV10Flags::AVC_DISABLED,
            },
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::SMALL_CACHE | CapabilitiesV10Flags::AVC_DISABLED,
            },
        ]
    }

    fn on_capabilities_confirmed(&mut self, caps: &CapabilitySet) {
        tracing::info!(?caps, "GFX включён");
    }

    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        self.output = (width as usize, height as usize);
        for id in std::mem::take(&mut self.surfaces).into_keys() {
            self.progressive.delete_surface(id);
        }
        self.progressive.reset();
    }

    fn on_surface_created(&mut self, s: &GfxSurface) {
        self.surfaces.insert(
            s.id,
            Surface::new(usize::from(s.width), usize::from(s.height)),
        );
    }

    fn on_surface_deleted(&mut self, id: u16) {
        self.surfaces.remove(&id);
        self.progressive.delete_surface(id);
    }

    fn on_surface_mapped(&mut self, id: u16, x: u32, y: u32) {
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.origin = Some((x as usize, y as usize));
            let all = s.bounds();
            s.mark(all);
        }
    }

    fn on_bitmap_updated(&mut self, u: &BitmapUpdate) {
        // Сюда приходит только несжатый формат (H.264 мы не объявляем).
        if let Some(s) = self.surfaces.get_mut(&u.surface_id) {
            let at = Rect::from_pdu(&u.destination_rectangle);
            let clip = s.bounds();
            s.put(at, &u.data, Fmt::Rgba, clip);
        }
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {
        // egfx 0.3 не сообщает о StartFrame: следующий кадр начинаем сразу.
        self.progressive.end_frame();
        self.progressive.begin_frame();
        self.present();
    }

    fn on_solid_fill(&mut self, pdu: &SolidFillPdu) {
        let c = &pdu.fill_pixel;
        if let Some(s) = self.surfaces.get_mut(&pdu.surface_id) {
            for r in &pdu.rectangles {
                s.fill(Rect::from_pdu(r), [c.r, c.g, c.b, 255]);
            }
        }
    }

    fn on_surface_to_surface(&mut self, pdu: &SurfaceToSurfacePdu) {
        let Some(src) = self.surfaces.get(&pdu.source_surface_id) else {
            return;
        };
        let (r, px) = src.read(Rect::from_pdu(&pdu.source_rectangle));
        if r.is_empty() {
            return;
        }
        if let Some(dst) = self.surfaces.get_mut(&pdu.destination_surface_id) {
            let clip = dst.bounds();
            for p in &pdu.destination_points {
                let at = Rect::new(usize::from(p.x), usize::from(p.y), r.w(), r.h());
                dst.put(at, &px, Fmt::Rgba, clip);
            }
        }
    }

    fn on_surface_to_cache(&mut self, pdu: &SurfaceToCachePdu) {
        if let Some(s) = self.surfaces.get(&pdu.surface_id) {
            let (r, px) = s.read(Rect::from_pdu(&pdu.source_rectangle));
            self.cache.insert(
                pdu.cache_slot,
                CacheEntry {
                    w: r.w(),
                    h: r.h(),
                    px,
                },
            );
        }
    }

    fn on_cache_to_surface(&mut self, pdu: &CacheToSurfacePdu) {
        let (Some(entry), Some(s)) = (
            self.cache.get(&pdu.cache_slot),
            self.surfaces.get_mut(&pdu.surface_id),
        ) else {
            return;
        };
        let clip = s.bounds();
        for p in &pdu.destination_points {
            let at = Rect::new(usize::from(p.x), usize::from(p.y), entry.w, entry.h);
            s.put(at, &entry.px, Fmt::Rgba, clip);
        }
    }

    fn on_evict_cache_entry(&mut self, pdu: &EvictCacheEntryPdu) {
        self.cache.remove(&pdu.cache_slot);
    }

    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        self.wire_to_surface2(pdu);
    }

    fn on_delete_encoding_context(&mut self, pdu: &DeleteEncodingContextPdu) {
        self.progressive
            .delete_context(pdu.surface_id, pdu.codec_context_id);
    }

    fn on_unhandled_pdu(&mut self, pdu: &GfxPdu) {
        match pdu {
            GfxPdu::WireToSurface1(w) => self.wire_to_surface1(w),
            other => {
                let name = format!("{other:?}");
                let name = name.split(['(', ' ', '{']).next().unwrap_or("?").to_owned();
                self.warn_once(format!("pdu {name}"), format!("{name} не обрабатывается"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler() -> Handler {
        Handler::new(
            Arc::new(Mutex::new(Shared::default())),
            egui::Context::default(),
        )
    }

    #[test]
    fn put_clips_and_converts() {
        let mut s = Surface::new(4, 4);
        let bgra = [10u8, 20, 30, 0].repeat(9);
        // 3×3, выходит за край и обрезается по clip.
        s.put(
            Rect::new(2, 2, 3, 3),
            &bgra,
            Fmt::Bgra,
            Rect::new(0, 0, 4, 3),
        );
        assert_eq!(s.dirty, Some(Rect::new(2, 2, 2, 1)));
        let (_, px) = s.read(Rect::new(2, 2, 1, 1));
        assert_eq!(px, vec![30, 20, 10, 255]);
        let (_, px) = s.read(Rect::new(2, 3, 1, 1));
        assert_eq!(px, vec![0, 0, 0, 255]);
    }

    #[test]
    fn whole_frame_reaches_shared_on_end_frame() {
        let mut h = handler();
        h.shared.lock().unwrap().state = Some(State::Connecting);
        h.on_reset_graphics(8, 4);
        h.surfaces.insert(1, Surface::new(4, 4));
        h.on_surface_mapped(1, 4, 0);
        h.present();
        h.surfaces
            .get_mut(&1)
            .unwrap()
            .fill(Rect::new(0, 0, 1, 1), [255, 0, 0, 255]);
        // До EndFrame на экране ничего нового.
        assert_eq!(
            h.shared.lock().unwrap().snapshot_rgb().unwrap().1[12..15],
            [0, 0, 0]
        );
        h.present();
        let sh = h.shared.lock().unwrap();
        let (size, rgb) = sh.snapshot_rgb().unwrap();
        assert_eq!(size, [8, 4]);
        // Поверхность выведена с x=4.
        assert_eq!(rgb[4 * 3..4 * 3 + 3], [255, 0, 0]);
        assert_eq!(sh.state, Some(State::Connected));
    }

    #[test]
    fn surface_to_surface_and_cache() {
        let mut h = handler();
        h.surfaces.insert(1, Surface::new(4, 4));
        h.surfaces
            .get_mut(&1)
            .unwrap()
            .fill(Rect::new(0, 0, 2, 2), [1, 2, 3, 255]);
        let s = h.surfaces.get_mut(&1).unwrap();
        let (r, px) = s.read(Rect::new(0, 0, 2, 2));
        s.put(
            Rect::new(2, 2, r.w(), r.h()),
            &px,
            Fmt::Rgba,
            Rect::new(0, 0, 4, 4),
        );
        assert_eq!(s.read(Rect::new(3, 3, 1, 1)).1, vec![1, 2, 3, 255]);
        h.cache.insert(
            5,
            CacheEntry {
                w: 1,
                h: 1,
                px: vec![9, 9, 9, 255],
            },
        );
        let entry = h.cache.get(&5).unwrap();
        let s = h.surfaces.get_mut(&1).unwrap();
        s.put(
            Rect::new(0, 3, entry.w, entry.h),
            &entry.px.clone(),
            Fmt::Rgba,
            Rect::new(0, 0, 4, 4),
        );
        assert_eq!(s.read(Rect::new(0, 3, 1, 1)).1, vec![9, 9, 9, 255]);
    }
}
