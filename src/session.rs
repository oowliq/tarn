//! Одна RDP-сессия: движок ironrdp-client + задача keep-alive + приём кадров.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Color32, ColorImage};
use ironrdp_client::config::{ConfigBuilder, Destination};
use ironrdp_client::rdp::{RdpClient, RdpInputEvent, RdpOutputEvent};
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use smallvec::SmallVec;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};

use crate::config::Profile;
use crate::keepalive::{self, KeepAliveSettings};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Connecting,
    Connected,
    Closed(Option<String>),
}

/// Общее состояние, которое пишет фоновая задача и читает UI.
#[derive(Default)]
pub struct Shared {
    /// Полная копия удалённого экрана.
    master: Option<ColorImage>,
    /// Изменённая с прошлой выгрузки область: [x0, y0, x1, y1), в пикселях.
    dirty: Option<[usize; 4]>,
    /// Размер кадра поменялся — нужна полная выгрузка.
    resized: bool,
    /// Когда пришёл первый/последний кусок ещё не выгруженных изменений
    /// (только без маркеров кадров).
    first_region_at: Option<Instant>,
    last_region_at: Option<Instant>,
    /// Сервер размечает кадры (Frame Marker): показываем только целые кадры.
    framed: bool,
    /// Куски текущего кадра, ждут его конца; `pending_since` — первый из них.
    pending: Vec<Region>,
    pending_since: Option<Instant>,
    /// Курсор, который сейчас задал сервер.
    pub pointer: RemotePointer,
    pub state: Option<State>,
}

/// Курсор удалённой машины.
#[derive(Clone, Default, PartialEq)]
pub enum RemotePointer {
    /// Стандартная стрелка.
    #[default]
    Default,
    Hidden,
    /// Картинка курсора (RGBA без premultiply, в пикселях удалённого экрана).
    Image(egui::CustomCursorImage),
}

struct Region {
    size: [usize; 2],
    left: usize,
    top: usize,
    w: usize,
    h: usize,
    rgba: Vec<u8>,
}

/// Сервер присылает один кадр несколькими PDU подряд. Чтобы не показывать
/// полуобновлённый экран (тиринг при перетаскивании окон), выгружаем только
/// целые кадры: по маркеру конца кадра, а если сервер маркеры не шлёт — после
/// короткой паузы в потоке обновлений, но не дольше `FRAME_MAX_WAIT`.
const FRAME_QUIET: Duration = Duration::from_millis(5);
const FRAME_MAX_WAIT: Duration = Duration::from_millis(40);
/// Если конец кадра так и не пришёл, всё равно показываем, что есть.
const FRAMED_MAX_WAIT: Duration = Duration::from_millis(150);

pub enum FrameUpdate {
    Full(ColorImage),
    Partial { pos: [usize; 2], image: ColorImage },
}

impl Shared {
    /// Размер удалённого экрана (None — кадра ещё нет).
    pub fn frame_size(&self) -> Option<[usize; 2]> {
        self.master.as_ref().map(|m| m.size)
    }

    /// Текущий кадр в RGB8 (для скриншотов Claude).
    pub fn snapshot_rgb(&self) -> Option<([usize; 2], Vec<u8>)> {
        let m = self.master.as_ref()?;
        let mut buf = Vec::with_capacity(m.pixels.len() * 3);
        for p in &m.pixels {
            buf.extend_from_slice(&[p.r(), p.g(), p.b()]);
        }
        Some((m.size, buf))
    }

    pub(crate) fn apply_region(
        &mut self,
        size: [usize; 2],
        left: usize,
        top: usize,
        w: usize,
        h: usize,
        rgba: Vec<u8>,
    ) {
        let region = Region {
            size,
            left,
            top,
            w,
            h,
            rgba,
        };
        let now = Instant::now();
        if self.framed {
            self.pending.push(region);
            let since = *self.pending_since.get_or_insert(now);
            // Не копим бесконечно, если UI не опрашивает (окно свёрнуто).
            if now.saturating_duration_since(since) >= FRAMED_MAX_WAIT {
                self.commit_pending();
            }
            return;
        }
        self.blit(&region);
        self.first_region_at.get_or_insert(now);
        self.last_region_at = Some(now);
    }

    /// Сервер закончил кадр: показываем его целиком.
    pub(crate) fn frame_end(&mut self) {
        if !self.framed {
            self.framed = true;
            self.first_region_at = None;
            self.last_region_at = None;
        }
        self.commit_pending();
    }

    fn commit_pending(&mut self) {
        for region in std::mem::take(&mut self.pending) {
            self.blit(&region);
        }
        self.pending_since = None;
    }

    fn blit(&mut self, region: &Region) {
        let &Region {
            size,
            left,
            top,
            w,
            h,
            ref rgba,
        } = region;
        if self.master.as_ref().is_none_or(|m| m.size != size) {
            self.master = Some(ColorImage::new(
                size,
                vec![Color32::BLACK; size[0] * size[1]],
            ));
            self.resized = true;
        }
        let master = self.master.as_mut().expect("just set");
        for row in 0..h {
            let dst = (top + row) * size[0] + left;
            let src = &rgba[row * w * 4..(row + 1) * w * 4];
            for (px, c) in master.pixels[dst..dst + w]
                .iter_mut()
                .zip(src.as_chunks::<4>().0)
            {
                *px = Color32::from_rgb(c[0], c[1], c[2]);
            }
        }
        let r = [left, top, left + w, top + h];
        self.dirty = Some(match self.dirty {
            Some(d) => [
                d[0].min(r[0]),
                d[1].min(r[1]),
                d[2].max(r[2]),
                d[3].max(r[3]),
            ],
            None => r,
        });
    }

    /// Готовые к показу изменения и через сколько перерисовать, если кадр
    /// ещё собирается. `full` — нужна вся картинка (новая текстура).
    pub fn take_frame(
        &mut self,
        now: Instant,
        full: bool,
    ) -> (Option<FrameUpdate>, Option<Duration>) {
        let mut wake = None;
        if let Some(since) = self.pending_since {
            let left = FRAMED_MAX_WAIT.saturating_sub(now.saturating_duration_since(since));
            if left.is_zero() {
                self.commit_pending();
            } else {
                wake = Some(left);
            }
        }
        if !full && let Some(wait) = self.wait_for_frame(now) {
            return (None, Some(wake.map_or(wait, |w| w.min(wait))));
        }
        (self.take_update(full), wake)
    }

    /// Без маркеров кадров: сколько ещё подождать, пока кадр «соберётся».
    fn wait_for_frame(&self, now: Instant) -> Option<Duration> {
        let (first, last) = (self.first_region_at?, self.last_region_at?);
        let quiet_left = FRAME_QUIET.saturating_sub(now.saturating_duration_since(last));
        let max_left = FRAME_MAX_WAIT.saturating_sub(now.saturating_duration_since(first));
        let wait = quiet_left.min(max_left);
        (!wait.is_zero()).then_some(wait)
    }

    /// Забрать накопленные изменения для текстуры. `full` — нужна вся картинка.
    fn take_update(&mut self, full: bool) -> Option<FrameUpdate> {
        self.first_region_at = None;
        self.last_region_at = None;
        let master = self.master.as_ref()?;
        if full || self.resized {
            self.resized = false;
            self.dirty = None;
            return Some(FrameUpdate::Full(master.clone()));
        }
        let [x0, y0, x1, y1] = self.dirty.take()?;
        let w = x1 - x0;
        let mut pixels = Vec::with_capacity(w * (y1 - y0));
        for y in y0..y1 {
            let start = y * master.size[0] + x0;
            pixels.extend_from_slice(&master.pixels[start..start + w]);
        }
        Some(FrameUpdate::Partial {
            pos: [x0, y0],
            image: ColorImage::new([w, y1 - y0], pixels),
        })
    }
}

/// Диагностика: как сервер присылает кадры. Раз в несколько секунд пишет в лог,
/// размечает ли сервер кадры и сколько длится «пачка» обновлений одного кадра.
#[derive(Default)]
struct FrameStats {
    since: Option<Instant>,
    regions: u32,
    bytes: usize,
    markers: u32,
    bursts: u32,
    burst_start: Option<Instant>,
    burst_regions: u32,
    last_region: Option<Instant>,
    max_burst: Duration,
    total_burst: Duration,
    max_burst_regions: u32,
}

impl FrameStats {
    /// Пауза, после которой считаем, что началась новая пачка.
    const GAP: Duration = Duration::from_millis(30);
    const PERIOD: Duration = Duration::from_secs(5);

    fn on_event(&mut self, ev: &RdpOutputEvent, now: Instant) {
        match ev {
            RdpOutputEvent::ImageRegion { rgba, .. } => {
                if self
                    .last_region
                    .is_some_and(|t| now.saturating_duration_since(t) > Self::GAP)
                {
                    self.close_burst();
                }
                self.burst_start.get_or_insert(now);
                self.burst_regions += 1;
                self.last_region = Some(now);
                self.regions += 1;
                self.bytes += rgba.len();
            }
            RdpOutputEvent::FrameEnd => self.markers += 1,
            _ => return,
        }
        let since = *self.since.get_or_insert(now);
        if now.saturating_duration_since(since) >= Self::PERIOD {
            self.close_burst();
            let avg = self.total_burst / self.bursts.max(1);
            tracing::info!(
                markers = self.markers,
                regions = self.regions,
                mb = format_args!("{:.1}", self.bytes as f64 / 1e6),
                bursts = self.bursts,
                avg_burst_ms = avg.as_millis() as u64,
                max_burst_ms = self.max_burst.as_millis() as u64,
                max_burst_regions = self.max_burst_regions,
                "кадры за {} с (markers=0 — сервер не размечает кадры)",
                Self::PERIOD.as_secs()
            );
            *self = Self::default();
        }
    }

    fn close_burst(&mut self) {
        if let (Some(start), Some(last)) = (self.burst_start.take(), self.last_region) {
            let d = last.saturating_duration_since(start);
            self.bursts += 1;
            self.total_burst += d;
            self.max_burst = self.max_burst.max(d);
            self.max_burst_regions = self.max_burst_regions.max(self.burst_regions);
        }
        self.burst_regions = 0;
    }
}

pub struct Session {
    input: mpsc::UnboundedSender<RdpInputEvent>,
    keepalive: watch::Sender<KeepAliveSettings>,
    pub shared: Arc<Mutex<Shared>>,
    pub ka_status: Arc<Mutex<keepalive::Status>>,
    pub activity: keepalive::Activity,
}

pub struct ConnectParams {
    pub width: u16,
    pub height: u16,
    pub scale_factor: u32,
}

impl Session {
    pub fn start(
        rt: &Handle,
        profile: &Profile,
        password: String,
        params: ConnectParams,
        ctx: egui::Context,
    ) -> anyhow::Result<Self> {
        let d = &profile.display;
        let (username, domain) = split_credentials(&profile.username, &profile.domain);
        let mut builder = ConfigBuilder::new()
            .with_destination(Destination::from_parts(profile.host.trim(), profile.port))
            .with_username(username)
            .with_password(password)
            .with_client_name(client_name())
            .with_client_build(0)
            .with_client_dir("C:\\Windows\\System32\\mstscax.dll")
            .with_platform(platform())
            .with_desktop_width(params.width)
            .with_desktop_height(params.height)
            .with_desktop_scale_factor(params.scale_factor)
            .with_color_depth(if d.color_depth == 16 { 16 } else { 32 })
            .with_performance_flags(d.performance_flags())
            .with_compression(d.compression);
        if let Some(domain) = domain {
            builder = builder.with_domain(domain);
        }
        let shared = Arc::new(Mutex::new(Shared {
            state: Some(State::Connecting),
            ..Shared::default()
        }));
        // GFX: сервер шлёт кадры целиком, без тиринга. Требует 32 бит.
        if d.gfx && d.color_depth != 16 {
            let (shared, ctx) = (shared.clone(), ctx.clone());
            builder = builder
                .with_gfx(true)
                .with_dvc(move |_| Some(crate::gfx::client(shared.clone(), ctx.clone())));
        }
        let config = builder.build()?;

        let (out_tx, mut out_rx) = mpsc::channel::<RdpOutputEvent>(16);
        let client = RdpClient::new(config, out_tx);
        let input = client.input_sender();

        let activity = keepalive::Activity::default();
        let ka_status = Arc::new(Mutex::new(keepalive::Status::default()));
        let (ka_tx, ka_rx) = watch::channel(profile.keepalive.clone());

        // Future движка не `Send`, поэтому у него свой поток с однопоточным runtime.
        std::thread::Builder::new()
            .name("rdp-session".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt.block_on(client.run()),
                    Err(e) => tracing::error!(%e, "не удалось создать runtime сессии"),
                }
            })?;

        // Кадры забираем в отдельной задаче: если UI не рисует (окно свёрнуто),
        // движок не должен блокироваться на канале — иначе встанет и keep-alive.
        {
            let shared = shared.clone();
            let ctx = ctx.clone();
            rt.spawn(async move {
                let mut stats = FrameStats::default();
                while let Some(ev) = out_rx.recv().await {
                    stats.on_event(&ev, Instant::now());
                    let mut sh = shared.lock().unwrap();
                    match ev {
                        RdpOutputEvent::ImageRegion {
                            width,
                            height,
                            left,
                            top,
                            region_width,
                            region_height,
                            rgba,
                        } => {
                            let size = [usize::from(width.get()), usize::from(height.get())];
                            sh.apply_region(
                                size,
                                usize::from(left),
                                usize::from(top),
                                usize::from(region_width),
                                usize::from(region_height),
                                rgba,
                            );
                            if sh.state == Some(State::Connecting) {
                                sh.state = Some(State::Connected);
                            }
                        }
                        RdpOutputEvent::FrameEnd => sh.frame_end(),
                        RdpOutputEvent::PointerDefault => sh.pointer = RemotePointer::Default,
                        RdpOutputEvent::PointerHidden => sh.pointer = RemotePointer::Hidden,
                        RdpOutputEvent::PointerBitmap(p) => {
                            let ok = p.width > 0
                                && p.height > 0
                                && p.bitmap_data.len()
                                    == usize::from(p.width) * usize::from(p.height) * 4;
                            sh.pointer = if ok {
                                RemotePointer::Image(egui::CustomCursorImage {
                                    rgba: p.bitmap_data.as_slice().into(),
                                    size: [p.width, p.height],
                                    hotspot: [
                                        p.hotspot_x.min(p.width - 1),
                                        p.hotspot_y.min(p.height - 1),
                                    ],
                                })
                            } else {
                                tracing::warn!(w = p.width, h = p.height, "странный курсор с сервера");
                                RemotePointer::Default
                            };
                        }
                        RdpOutputEvent::ConnectionFailure(e) => {
                            let report = e.report().to_string();
                            tracing::error!(error = %e.report().with_locations(), "ошибка подключения");
                            let mut msg = format!("Ошибка подключения: {report}");
                            if report.contains("CredSSP") {
                                msg += "\nПроверьте логин/пароль и домен (DOMAIN\\user или user@domain).";
                            }
                            sh.state = Some(State::Closed(Some(msg)));
                        }
                        RdpOutputEvent::Terminated(r) => {
                            let msg = match r {
                                Ok(reason) => {
                                    tracing::info!(?reason, "сессия завершена");
                                    None
                                }
                                Err(e) => Some(format!("Сессия прервана: {}", e.report())),
                            };
                            sh.state = Some(State::Closed(msg));
                        }
                        _ => {}
                    }
                    drop(sh);
                    ctx.request_repaint();
                }
                // Движок завершился, не сообщив причину (например, паника потока).
                let mut sh = shared.lock().unwrap();
                if !matches!(sh.state, Some(State::Closed(_))) {
                    sh.state = Some(State::Closed(Some("Сессия неожиданно завершилась".into())));
                }
                drop(sh);
                ctx.request_repaint();
            });
        }

        rt.spawn(keepalive::run(
            ka_rx,
            input.clone(),
            activity.clone(),
            ka_status.clone(),
            move || {
                ctx.request_repaint();
            },
        ));

        Ok(Self {
            input,
            keepalive: ka_tx,
            shared,
            ka_status,
            activity,
        })
    }

    /// Куда подключается Claude (см. [`crate::agent`]).
    pub fn agent_target(&self) -> crate::agent::Target {
        crate::agent::Target {
            shared: self.shared.clone(),
            input: self.input.clone(),
            activity: self.activity.clone(),
        }
    }

    /// Куда клавиатура отправляет нажатия (см. [`crate::keyboard`]).
    pub fn keyboard_link(&self) -> crate::keyboard::Link {
        crate::keyboard::Link {
            input: self.input.clone(),
            activity: self.activity.clone(),
        }
    }

    /// Служебные события (например, отпускание кнопок при потере фокуса) —
    /// не считаются активностью пользователя.
    pub fn send(&self, events: SmallVec<[FastPathInputEvent; 2]>) {
        if !events.is_empty() {
            let _ = self.input.send(RdpInputEvent::FastPath(events));
        }
    }

    /// Ввод от пользователя: отправить и отметить активность для keep-alive.
    pub fn send_user(&self, events: SmallVec<[FastPathInputEvent; 2]>) {
        if !events.is_empty() {
            self.activity.touch();
            self.send(events);
        }
    }

    pub fn set_keepalive(&self, s: KeepAliveSettings) {
        self.keepalive.send_if_modified(|cur| {
            let changed = *cur != s;
            *cur = s;
            changed
        });
    }

    pub fn resize(&self, width: u16, height: u16, scale_factor: u32) {
        let _ = self.input.send(RdpInputEvent::Resize {
            width,
            height,
            scale_factor,
            physical_size: None,
        });
    }

    pub fn close(&self) {
        let _ = self.input.send(RdpInputEvent::Close);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// `DOMAIN\user` → (`user`, `DOMAIN`). UPN `user@domain` оставляем как есть.
/// Явно заданный домен в профиле имеет приоритет.
fn split_credentials<'a>(username: &'a str, domain: &'a str) -> (&'a str, Option<&'a str>) {
    let username = username.trim();
    let domain = domain.trim();
    let (user, from_login) = match username.split_once('\\') {
        Some((d, u)) if !d.is_empty() && !u.is_empty() => (u, Some(d)),
        _ => (username, None),
    };
    let domain = if domain.is_empty() {
        from_login
    } else {
        Some(domain)
    };
    (user, domain)
}

fn client_name() -> String {
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "tarn".into());
    // RDP ограничивает имя клиента 15 символами.
    name.chars().take(15).collect()
}

fn platform() -> ironrdp_pdu::rdp::capability_sets::MajorPlatformType {
    use ironrdp_pdu::rdp::capability_sets::MajorPlatformType as P;
    if cfg!(target_os = "macos") {
        P::MACINTOSH
    } else if cfg!(windows) {
        P::WINDOWS
    } else {
        P::UNIX
    }
}

#[cfg(test)]
mod tests {
    use super::split_credentials;

    #[test]
    fn partial_updates_track_dirty_region() {
        use super::{FrameUpdate, Shared};
        let mut sh = Shared::default();
        let red = [255u8, 0, 0, 255].repeat(2 * 2);
        sh.apply_region([4, 4], 1, 1, 2, 2, red.clone());
        // Первый кадр — всегда полный.
        assert!(matches!(sh.take_update(false), Some(FrameUpdate::Full(_))));
        assert!(sh.take_update(false).is_none());
        sh.apply_region([4, 4], 0, 0, 1, 1, red[..4].to_vec());
        sh.apply_region([4, 4], 3, 2, 1, 1, red[..4].to_vec());
        match sh.take_update(false) {
            Some(FrameUpdate::Partial { pos, image }) => {
                assert_eq!(pos, [0, 0]);
                assert_eq!(image.size, [4, 3]);
                assert_eq!(image.pixels[0], egui::Color32::RED);
            }
            _ => panic!("expected partial"),
        }
    }

    #[test]
    fn waits_for_burst_to_finish() {
        use super::Shared;
        use std::time::{Duration, Instant};
        let mut sh = Shared::default();
        assert!(sh.wait_for_frame(Instant::now()).is_none());
        sh.apply_region([2, 2], 0, 0, 1, 1, vec![0, 0, 0, 255]);
        let t = sh.last_region_at.unwrap();
        assert!(sh.wait_for_frame(t).is_some());
        assert!(sh.wait_for_frame(t + Duration::from_millis(6)).is_none());
        // Непрерывный поток: не ждём дольше 40 мс с первого куска.
        sh.last_region_at = Some(t + Duration::from_millis(39));
        assert!(sh.wait_for_frame(t + Duration::from_millis(40)).is_none());
        sh.take_update(false);
        assert!(sh.wait_for_frame(t).is_none());
    }

    #[test]
    fn framed_shows_only_whole_frames() {
        use super::{FrameUpdate, Shared};
        use std::time::{Duration, Instant};
        let mut sh = Shared::default();
        let px = |v: u8| vec![v, v, v, 255];
        sh.apply_region([4, 4], 0, 0, 1, 1, px(1));
        sh.frame_end();
        assert!(matches!(
            sh.take_frame(Instant::now(), false),
            (Some(FrameUpdate::Full(_)), None)
        ));
        // Полкадра: на экран ещё не попадает, ни сразу, ни через «паузу».
        sh.apply_region([4, 4], 0, 0, 1, 1, px(2));
        let t = Instant::now();
        let (upd, wake) = sh.take_frame(t + Duration::from_millis(50), false);
        assert!(upd.is_none() && wake.is_some());
        assert_eq!(sh.snapshot_rgb().unwrap().1[0], 1);
        sh.apply_region([4, 4], 3, 3, 1, 1, px(3));
        sh.frame_end();
        match sh.take_frame(Instant::now(), false) {
            (Some(FrameUpdate::Partial { pos, image }), None) => {
                assert_eq!(pos, [0, 0]);
                assert_eq!(image.size, [4, 4]);
            }
            _ => panic!("expected whole frame"),
        }
        // Конец кадра потерялся — показываем по таймауту.
        sh.apply_region([4, 4], 1, 1, 1, 1, px(4));
        let (upd, _) = sh.take_frame(Instant::now() + Duration::from_millis(200), false);
        assert!(matches!(upd, Some(FrameUpdate::Partial { .. })));
    }

    #[test]
    fn splits_domain_from_login() {
        assert_eq!(split_credentials("CORP\\User", ""), ("User", Some("CORP")));
        assert_eq!(split_credentials("User", ""), ("User", None));
        assert_eq!(
            split_credentials("user@corp.local", ""),
            ("user@corp.local", None)
        );
        assert_eq!(split_credentials("X\\User", "CORP"), ("User", Some("CORP")));
        assert_eq!(
            split_credentials(" User ", " CORP "),
            ("User", Some("CORP"))
        );
    }
}
