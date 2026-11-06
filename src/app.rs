use std::time::{Duration, Instant};

use egui::{Color32, RichText, TextureHandle, TextureOptions};
use tokio::runtime::Handle;

use crate::agent::{self, AgentMode, AgentSettings, Hub};
use crate::config::{Profile, Store};
use crate::display::{DisplaySettings, Preset};
use crate::input::{InputState, Viewport};
use crate::keepalive::{KeepAliveCombo, KeepAliveSettings, MIN_INTERVAL_SECS};
use crate::keyboard::{self, CapsLockMode, GlobalStatus, Keyboard, KeyboardSettings, MacRemap};
use crate::secrets;
use crate::session::{ConnectParams, FrameUpdate, RemotePointer, Session, State};
use crate::update::{self, Updater};

const RESIZE_DEBOUNCE: Duration = Duration::from_millis(500);
const TOP_BAR_HEIGHT: f32 = 32.0;
/// Полноэкранная панель: сколько держать открытой после ухода курсора и зона у края экрана.
const BAR_HIDE_DELAY: Duration = Duration::from_millis(700);
const BAR_TRIGGER_PX: f32 = 2.0;
/// Цвет всего, что связано с Claude: рамка, метка курсора, статус.
const CLAUDE_ACCENT: Color32 = Color32::from_rgb(217, 119, 87);
const ERROR_RED: Color32 = Color32::from_rgb(220, 80, 60);
const WARN_AMBER: Color32 = Color32::from_rgb(230, 160, 40);
/// Сколько после действия Claude видны рамка вокруг экрана и метка курсора.
const AGENT_FRAME_FADE: Duration = Duration::from_millis(2500);
const AGENT_MARK_FADE: Duration = Duration::from_secs(4);

pub struct App {
    rt: Handle,
    store: Store,
    /// Ошибка/сообщение для экрана списка.
    notice: Option<String>,
    editor: Option<Editor>,
    password_prompt: Option<PasswordPrompt>,
    active: Option<Active>,
    updater: Updater,
    show_updates: bool,
    /// Доступ для Claude: общее состояние, MCP-сервер, ошибка запуска.
    hub: Hub,
    agent_server: Option<agent::Server>,
    agent_error: Option<String>,
    show_agent_log: bool,
    keyboard: Keyboard,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorTab {
    Connection,
    Display,
    KeepAlive,
}

struct Editor {
    profile: Profile,
    password: String,
    is_new: bool,
    tab: EditorTab,
}

struct PasswordPrompt {
    profile_id: String,
    password: String,
    remember: bool,
}

struct Active {
    profile_id: String,
    title: String,
    session: Session,
    texture: Option<TextureHandle>,
    texture_options: TextureOptions,
    /// Курсор сервера, отмасштабированный под окно.
    cursor: crate::cursor::Cache,
    /// Настройки экрана на момент подключения (разрешение/Retina меняются только переподключением).
    display: DisplaySettings,
    input: InputState,
    was_focused: bool,
    /// Caps/Num Lock сервера выровнены с локальными с момента получения фокуса.
    locks_synced: bool,
    show_keepalive: bool,
    /// Последний запрошенный размер (px) и момент, с которого желаемый размер стабилен.
    requested_size: (u16, u16),
    pending_size: Option<((u16, u16), Instant)>,
    /// Полноэкранный режим: панель закреплена / до какого момента её держать открытой.
    bar_pinned: bool,
    bar_visible_until: Option<Instant>,
}

/// Что пользователь нажал на панели сессии.
#[derive(Default)]
struct BarActions {
    disconnect: bool,
    toggle_fullscreen: bool,
    minimize: bool,
    display_changed: bool,
    open_updates: bool,
    agent_changed: bool,
    stop_agent: bool,
    open_agent_log: bool,
    keyboard_changed: bool,
}

impl App {
    pub fn new(rt: Handle, cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let (mut store, notice) = match Store::load() {
            Ok(s) => (s, None),
            Err(e) => (Store::default(), Some(format!("{e:#}"))),
        };
        let keyboard = Keyboard::new(cc, &store.keyboard);
        let hub = {
            let ctx = ctx.clone();
            Hub::new(move || ctx.request_repaint())
        };
        let updater = Updater::new(ctx);
        let now = unix_now();
        if store.updates.due(now) {
            updater.check();
            store.updates.last_check = now;
            // Если файл настроек не прочитался, не затираем его дефолтами.
            if notice.is_none() {
                let _ = store.save();
            }
        }
        let mut app = Self {
            rt,
            store,
            notice,
            editor: None,
            password_prompt: None,
            active: None,
            updater,
            show_updates: false,
            hub,
            agent_server: None,
            agent_error: None,
            show_agent_log: false,
            keyboard,
        };
        app.apply_agent_settings();
        app
    }

    /// Применить настройки доступа Claude: режим, токен, (пере)запуск сервера.
    fn apply_agent_settings(&mut self) {
        let s = &mut self.store.agent;
        if s.mode != AgentMode::Off && s.ensure_token() && self.notice.is_none() {
            let _ = self.store.save();
        }
        let s = &self.store.agent;
        self.hub.set_mode(s.mode);
        self.hub.set_token(&s.token);
        if s.mode == AgentMode::Off {
            self.agent_server = None;
            self.agent_error = None;
            return;
        }
        if self
            .agent_server
            .as_ref()
            .is_some_and(|srv| srv.port() == s.port)
        {
            return;
        }
        // Сначала освобождаем старый порт.
        self.agent_server = None;
        match agent::Server::start(s.port, self.hub.clone()) {
            Ok(srv) => {
                self.agent_server = Some(srv);
                self.agent_error = None;
            }
            Err(e) => {
                self.agent_error = Some(format!("Не удалось открыть 127.0.0.1:{}: {e}", s.port));
            }
        }
    }

    /// ⏹: Claude остаётся только смотреть.
    fn stop_agent(&mut self) {
        if self.store.agent.mode == AgentMode::Control {
            self.store.agent.mode = AgentMode::View;
            self.hub.log("Пользователь остановил управление", false);
            self.apply_agent_settings();
            self.save_store();
        }
    }

    fn save_store(&mut self) {
        if let Err(e) = self.store.save() {
            self.notice = Some(format!("Не удалось сохранить настройки: {e:#}"));
        }
    }

    fn profile(&self, id: &str) -> Option<&Profile> {
        self.store.profiles.iter().find(|p| p.id == id)
    }

    fn connect(&mut self, ctx: &egui::Context, profile_id: &str, password: String) {
        let Some(profile) = self.profile(profile_id).cloned() else {
            return;
        };
        let d = profile.display.clone();
        let ppp = effective_ppp(ctx, &d);
        let params = if d.auto_resize {
            let size = ctx.content_rect().size();
            let (width, height) = to_px(egui::vec2(size.x, size.y - TOP_BAR_HEIGHT), ppp);
            ConnectParams {
                width,
                height,
                scale_factor: scale_factor(ppp),
            }
        } else {
            ConnectParams {
                width: d.fixed_width.clamp(200, 8192) & !1,
                height: d.fixed_height.clamp(200, 8192),
                scale_factor: 100,
            }
        };
        let requested_size = (params.width, params.height);
        match Session::start(&self.rt, &profile, password, params, ctx.clone()) {
            Ok(session) => {
                self.notice = None;
                self.hub.set_target(Some(session.agent_target()));
                self.keyboard.attach(Some(session.keyboard_link()));
                self.active = Some(Active {
                    profile_id: profile.id.clone(),
                    title: profile.title(),
                    session,
                    texture: None,
                    texture_options: TextureOptions::LINEAR,
                    cursor: Default::default(),
                    display: d,
                    input: InputState::default(),
                    was_focused: true,
                    locks_synced: false,
                    show_keepalive: false,
                    requested_size,
                    pending_size: None,
                    bar_pinned: false,
                    bar_visible_until: None,
                });
            }
            Err(e) => self.notice = Some(format!("{e:#}")),
        }
    }

    fn start_connect(&mut self, ctx: &egui::Context, profile_id: String) {
        match secrets::load(&profile_id) {
            Some(pw) => self.connect(ctx, &profile_id, pw),
            None => {
                self.password_prompt = Some(PasswordPrompt {
                    profile_id,
                    password: String::new(),
                    remember: true,
                })
            }
        }
    }

    // ── Экран списка подключений ─────────────────────────────────────────

    fn ui_list(&mut self, root: &mut egui::Ui) {
        let ctx = &root.ctx().clone();
        egui::CentralPanel::default().show(root, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Подключения");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("➕ Добавить").clicked() {
                        self.editor = Some(Editor {
                            profile: Profile::default(),
                            password: String::new(),
                            is_new: true,
                            tab: EditorTab::Connection,
                        });
                    }
                    if ui
                        .selectable_label(self.show_updates, "⬆ Обновления")
                        .clicked()
                    {
                        self.show_updates = !self.show_updates;
                    }
                });
            });
            if let Some(n) = &self.notice {
                ui.colored_label(Color32::from_rgb(220, 80, 60), n);
            }
            if let update::Status::Available(r) = self.updater.status() {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        Color32::from_rgb(90, 170, 90),
                        format!("Доступна новая версия {}", r.version),
                    );
                    if ui.button("Подробнее").clicked() {
                        self.show_updates = true;
                    }
                });
            }
            ui.separator();

            if self.store.profiles.is_empty() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label("Пока нет сохранённых подключений.");
                    ui.label("Нажмите «Добавить», чтобы создать первое.");
                });
                return;
            }

            let mut connect = None;
            let mut edit = None;
            let mut delete = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for p in &self.store.profiles {
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::Button::new(RichText::new(p.title()).strong()).frame(false))
                            .double_clicked()
                        {
                            connect = Some(p.id.clone());
                        }
                        let mut addr = format!("{}@{}:{}", p.username, p.host, p.port);
                        if let Some(preset) = p.display.matching_preset() {
                            addr += &format!("  ·  {}", preset.label());
                        }
                        if p.keepalive.enabled {
                            addr += &format!(
                                "  ·  keep-alive каждые {}–{} с",
                                p.keepalive.min_secs, p.keepalive.max_secs
                            );
                        }
                        ui.weak(addr);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("🗑").on_hover_text("Удалить").clicked() {
                                delete = Some(p.id.clone());
                            }
                            if ui.button("✏").on_hover_text("Изменить").clicked() {
                                edit = Some(p.id.clone());
                            }
                            if ui.button("Подключиться").clicked() {
                                connect = Some(p.id.clone());
                            }
                        });
                    });
                    ui.separator();
                }
            });

            if let Some(id) = connect {
                self.start_connect(ctx, id);
            }
            if let Some(id) = edit
                && let Some(p) = self.profile(&id).cloned()
            {
                self.editor = Some(Editor {
                    password: secrets::load(&id).unwrap_or_default(),
                    profile: p,
                    is_new: false,
                    tab: EditorTab::Connection,
                });
            }
            if let Some(id) = delete {
                self.store.profiles.retain(|p| p.id != id);
                secrets::delete(&id);
                self.save_store();
            }
        });
    }

    fn ui_editor(&mut self, ctx: &egui::Context) {
        let Some(ed) = &mut self.editor else { return };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new(if ed.is_new {
            "Новое подключение"
        } else {
            "Подключение"
        })
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut ed.tab, EditorTab::Connection, "Подключение");
                ui.selectable_value(&mut ed.tab, EditorTab::Display, "Экран и скорость");
                ui.selectable_value(&mut ed.tab, EditorTab::KeepAlive, "Keep-alive");
            });
            ui.separator();
            match ed.tab {
                EditorTab::Connection => connection_ui(ui, &mut ed.profile, &mut ed.password),
                EditorTab::Display => display_ui(ui, &mut ed.profile.display),
                EditorTab::KeepAlive => keepalive_ui(ui, &mut ed.profile.keepalive),
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let valid =
                    !ed.profile.host.trim().is_empty() && !ed.profile.username.trim().is_empty();
                if ui
                    .add_enabled(valid, egui::Button::new("Сохранить"))
                    .clicked()
                {
                    save = true;
                }
                if ui.button("Отмена").clicked() {
                    cancel = true;
                }
            });
        });

        if save {
            let ed = self.editor.take().unwrap();
            let id = ed.profile.id.clone();
            match self.store.profiles.iter_mut().find(|p| p.id == id) {
                Some(p) => *p = ed.profile,
                None => self.store.profiles.push(ed.profile),
            }
            if ed.password.is_empty() {
                secrets::delete(&id);
            } else if let Err(e) = secrets::save(&id, &ed.password) {
                self.notice = Some(format!("Пароль не сохранён в хранилище: {e:#}"));
            }
            self.save_store();
        } else if cancel || !open {
            self.editor = None;
        }
    }

    fn ui_password_prompt(&mut self, ctx: &egui::Context) {
        let Some(pp) = &mut self.password_prompt else {
            return;
        };
        let mut go = false;
        let mut cancel = false;
        egui::Window::new("Пароль")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut pp.password).password(true));
                r.request_focus();
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    go = true;
                }
                ui.checkbox(&mut pp.remember, "Запомнить в системном хранилище");
                ui.horizontal(|ui| {
                    go |= ui.button("Подключиться").clicked();
                    cancel = ui.button("Отмена").clicked();
                });
            });
        if go {
            let pp = self.password_prompt.take().unwrap();
            if pp.remember
                && let Err(e) = secrets::save(&pp.profile_id, &pp.password)
            {
                self.notice = Some(format!("Пароль не сохранён: {e:#}"));
            }
            self.connect(ctx, &pp.profile_id, pp.password);
        } else if cancel {
            self.password_prompt = None;
        }
    }

    fn ui_updates(&mut self, ctx: &egui::Context) {
        if self.updater.status() == update::Status::Restart {
            // Новая версия уже запущена: аккуратно закрываем сессию и выходим.
            self.active = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if !self.show_updates {
            return;
        }
        let mut open = true;
        let mut auto_changed = false;
        let in_session = self.active.is_some();
        egui::Window::new("Обновления")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(440.0);
                ui.label(format!("Установлена версия {}", update::current_version()));
                let before = self.store.updates.auto_check;
                ui.checkbox(
                    &mut self.store.updates.auto_check,
                    "Проверять при запуске (не чаще раза в сутки)",
                );
                auto_changed = before != self.store.updates.auto_check;
                ui.weak(format!(
                    "Проверка — один запрос к api.github.com/repos/{}; ничего о вас не отправляется.",
                    update::REPO
                ));
                ui.separator();

                let status = self.updater.status();
                match &status {
                    update::Status::Idle => {}
                    update::Status::Checking => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Проверяю…");
                        });
                    }
                    update::Status::UpToDate => {
                        ui.label("У вас последняя версия.");
                    }
                    update::Status::Available(r) => {
                        ui.strong(format!("Доступна версия {}", r.version));
                        if !r.notes.trim().is_empty() {
                            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                                ui.label(r.notes.trim());
                            });
                        }
                        ui.hyperlink_to("Открыть страницу релиза", &r.page_url);
                        ui.add_space(4.0);
                        let label = if in_session {
                            "Скачать и перезапустить (сессия будет отключена)"
                        } else {
                            "Скачать и перезапустить"
                        };
                        if ui.button(label).clicked() {
                            self.updater.install(r.clone());
                        }
                        if cfg!(target_os = "macos") {
                            ui.weak(
                                "После обновления macOS может снова попросить разрешение \
                                 «Универсальный доступ» для перехвата клавиш.",
                            );
                        }
                    }
                    update::Status::Downloading { done, total } => {
                        let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
                        ui.label("Скачиваю…");
                        ui.add(egui::ProgressBar::new(frac).show_percentage());
                    }
                    update::Status::Installing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Проверяю контрольную сумму и устанавливаю…");
                        });
                    }
                    update::Status::Restart => {}
                    update::Status::Error(e) => {
                        ui.colored_label(Color32::from_rgb(220, 80, 60), e);
                    }
                }
                ui.add_space(4.0);
                if ui
                    .add_enabled(!self.updater.busy(), egui::Button::new("Проверить сейчас"))
                    .clicked()
                {
                    self.updater.check();
                }
            });
        self.show_updates = open;
        if auto_changed {
            self.save_store();
        }
    }

    fn ui_agent_log(&mut self, ctx: &egui::Context) {
        if !self.show_agent_log {
            return;
        }
        let mut open = true;
        egui::Window::new("Журнал Claude")
            .open(&mut open)
            .collapsible(false)
            .default_size([480.0, 360.0])
            .show(ctx, |ui| {
                let entries = self.hub.log_entries();
                ui.horizontal(|ui| {
                    ui.weak(format!("Записей: {}", entries.len()));
                    if ui.button("Очистить").clicked() {
                        self.hub.clear_log();
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        if entries.is_empty() {
                            ui.weak("Пока пусто: здесь появится всё, что сделает Claude.");
                        }
                        for e in &entries {
                            let ago = e.at.elapsed().map(|d| d.as_secs()).unwrap_or(0);
                            ui.horizontal_wrapped(|ui| {
                                ui.weak(format_ago(ago));
                                if e.error {
                                    ui.colored_label(ERROR_RED, &e.text);
                                } else {
                                    ui.label(&e.text);
                                }
                            });
                        }
                    });
            });
        self.show_agent_log = open;
    }

    // ── Экран сессии ────────────────────────────────────────────────────

    fn ui_session(&mut self, root: &mut egui::Ui) {
        let ctx = &root.ctx().clone();
        let Some(active) = &mut self.active else {
            return;
        };

        let smooth = self
            .store
            .profiles
            .iter()
            .find(|p| p.id == active.profile_id)
            .is_none_or(|p| p.display.smooth_scaling);
        let options = if smooth {
            TextureOptions::LINEAR
        } else {
            TextureOptions::NEAREST
        };
        let need_full = active.texture.is_none() || options != active.texture_options;
        let (update, state, pointer) = {
            let mut sh = active.session.shared.lock().unwrap();
            let (update, wake) = sh.take_frame(Instant::now(), need_full);
            if let Some(wait) = wake {
                ctx.request_repaint_after(wait);
            }
            (update, sh.state.clone(), sh.pointer.clone())
        };
        match update {
            Some(FrameUpdate::Full(img)) => {
                active.texture_options = options;
                match &mut active.texture {
                    Some(t) => t.set(img, options),
                    None => active.texture = Some(ctx.load_texture("rdp", img, options)),
                }
            }
            Some(FrameUpdate::Partial { pos, image }) => {
                if let Some(t) = &mut active.texture {
                    t.set_partial(pos, image, options);
                }
            }
            None => {}
        }
        if let Some(State::Closed(msg)) = state {
            self.notice = msg;
            self.active = None;
            return;
        }

        // Свёрнутое окно тоже считается «не в фокусе»: отпускаем зажатые кнопки мыши.
        let focused = ctx.input(|i| i.focused && i.viewport().minimized != Some(true));
        if active.was_focused && !focused {
            active.session.send(active.input.release_all());
        }
        active.was_focused = focused;
        // Клавиатура у сессии, пока окно в фокусе и egui не ждёт ввода текста
        // (поле в меню и т.п.). Снятие отпускает всё нажатое на сервере.
        let owns_keyboard =
            focused && state == Some(State::Connected) && !ctx.egui_wants_keyboard_input();
        self.keyboard.set_active(owns_keyboard);
        if !focused {
            active.locks_synced = false;
        } else if owns_keyboard && !active.locks_synced {
            self.keyboard.sync_locks();
            active.locks_synced = true;
        }
        let mut ka_changed = None;
        let mut actions = BarActions::default();
        let update_version = match self.updater.status() {
            update::Status::Available(r) => Some(r.version),
            _ => None,
        };
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        if fullscreen {
            // Панель прячется и выезжает, когда курсор у верхнего края.
            let now = Instant::now();
            let at_top =
                ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| p.y <= BAR_TRIGGER_PX));
            if at_top || active.bar_pinned || egui::Popup::is_any_open(ctx) {
                active.bar_visible_until = Some(now + BAR_HIDE_DELAY);
            }
            let show = active.bar_visible_until.is_some_and(|t| t > now);
            if let Some(t) = active.bar_visible_until.filter(|_| show) {
                ctx.request_repaint_after(t - now);
            }
            let t = ctx.animate_bool_with_time(egui::Id::unique("fs_bar"), show, 0.15);
            if t > 0.0 {
                let screen = ctx.content_rect();
                let width = (screen.width() - 32.0).min(980.0);
                let height = TOP_BAR_HEIGHT + 8.0;
                let pos = egui::pos2(
                    screen.center().x - width / 2.0,
                    screen.top() - height * (1.0 - t),
                );
                let area = egui::Area::new(egui::Id::unique("fs_bar_area"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(pos)
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style())
                            .corner_radius(egui::CornerRadius {
                                nw: 0,
                                ne: 0,
                                sw: 8,
                                se: 8,
                            })
                            .inner_margin(egui::Margin::symmetric(10, 4))
                            .show(ui, |ui| {
                                ui.set_width(width - 20.0);
                                ui.set_height(TOP_BAR_HEIGHT);
                                session_bar(
                                    ui,
                                    active,
                                    &mut self.store,
                                    &state,
                                    true,
                                    update_version.as_ref(),
                                    (&self.hub, self.agent_error.as_deref()),
                                    &self.keyboard,
                                    &mut actions,
                                );
                            });
                    });
                if area.response.contains_pointer() {
                    active.bar_visible_until = Some(now + BAR_HIDE_DELAY);
                }
            }
        } else {
            egui::Panel::top("bar")
                .exact_size(TOP_BAR_HEIGHT)
                .show(root, |ui| {
                    session_bar(
                        ui,
                        active,
                        &mut self.store,
                        &state,
                        false,
                        update_version.as_ref(),
                        (&self.hub, self.agent_error.as_deref()),
                        &self.keyboard,
                        &mut actions,
                    );
                });
        }
        if actions.toggle_fullscreen {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
            active.bar_visible_until = None;
        }
        if actions.minimize {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        if actions.open_updates {
            self.show_updates = true;
        }
        if actions.open_agent_log {
            self.show_agent_log = true;
        }
        let disconnect = actions.disconnect;
        let agent_changed = actions.agent_changed;
        let stop_agent = actions.stop_agent;
        let display_changed = actions.display_changed;
        let keyboard_changed = actions.keyboard_changed;

        if active.show_keepalive
            && let Some(p) = self
                .store
                .profiles
                .iter_mut()
                .find(|p| p.id == active.profile_id)
        {
            let mut s = p.keepalive.clone();
            let mut open = true;
            egui::Window::new("Keep-alive")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::RIGHT_TOP, [-8.0, TOP_BAR_HEIGHT + 8.0])
                .show(ctx, |ui| keepalive_ui(ui, &mut s));
            active.show_keepalive = open;
            if s != p.keepalive {
                p.keepalive = s.clone();
                ka_changed = Some(s);
            }
        }
        if let Some(s) = ka_changed {
            active.session.set_keepalive(s);
            self.save_store();
        }
        if display_changed {
            self.save_store();
        }
        if keyboard_changed {
            self.keyboard.apply(&self.store.keyboard);
            self.save_store();
        }
        if stop_agent {
            self.stop_agent();
        } else if agent_changed {
            self.apply_agent_settings();
            self.save_store();
        }
        let hub = self.hub.clone();
        let keyboard = &self.keyboard;
        let Some(active) = &mut self.active else {
            return;
        };

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(root, |ui| {
                let avail = ui.available_rect_before_wrap();
                let ppp = effective_ppp(ctx, &active.display);

                // Динамическое разрешение: ждём, пока размер окна устоится.
                let want = to_px(avail.size(), ppp);
                if !active.display.auto_resize {
                    active.pending_size = None;
                } else if want != active.requested_size {
                    match active.pending_size {
                        Some((size, since)) if size == want => {
                            if since.elapsed() >= RESIZE_DEBOUNCE {
                                active.session.resize(want.0, want.1, scale_factor(ppp));
                                active.requested_size = want;
                                active.pending_size = None;
                            } else {
                                ctx.request_repaint_after(RESIZE_DEBOUNCE);
                            }
                        }
                        _ => {
                            active.pending_size = Some((want, Instant::now()));
                            ctx.request_repaint_after(RESIZE_DEBOUNCE);
                        }
                    }
                } else {
                    active.pending_size = None;
                }

                let Some(tex) = &active.texture else {
                    ui.centered_and_justified(|ui| ui.spinner());
                    return;
                };
                let [w, h] = tex.size();
                let img_pts = egui::vec2(w as f32, h as f32) / ppp;
                let mut scale = (avail.width() / img_pts.x).min(avail.height() / img_pts.y);
                // Почти 1:1 — рисуем пиксель в пиксель, без размытия.
                if (scale - 1.0).abs() < 0.02 {
                    scale = 1.0;
                }
                let rect = egui::Rect::from_center_size(avail.center(), img_pts * scale);
                let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                ui.painter().image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
                agent_overlay(ui, rect, [w, h], &hub);

                // Курсор сервера (I-beam, стрелки ресайза…) как системный курсор.
                if response.hovered() || response.dragged() {
                    match &pointer {
                        RemotePointer::Default => {}
                        RemotePointer::Hidden => ctx.set_cursor_icon(egui::CursorIcon::None),
                        RemotePointer::Image(img) => {
                            let factor =
                                crate::cursor::os_scale(scale / ppp, ctx.pixels_per_point());
                            ctx.set_cursor_image(Some(active.cursor.get(img, factor)));
                        }
                    }
                }

                if !focused {
                    return;
                }
                let vp = Viewport {
                    rect,
                    remote_size: (w as u16, h as u16),
                };
                let pointer_over = response.hovered() || response.dragged();
                // Щелчок по экрану возвращает клавиатуру сессии после Ctrl+Alt+Home.
                if pointer_over && ctx.input(|i| i.pointer.any_pressed()) {
                    keyboard.resume();
                }
                let events = ctx.input(|i| i.events.clone());
                let out = active.input.translate(&events, &vp, pointer_over);
                active.session.send_user(out);
            });

        if disconnect {
            self.active = None;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn session_bar(
    ui: &mut egui::Ui,
    active: &mut Active,
    store: &mut Store,
    state: &Option<State>,
    fullscreen: bool,
    update: Option<&semver::Version>,
    (hub, agent_error): (&Hub, Option<&str>),
    keyboard: &Keyboard,
    actions: &mut BarActions,
) {
    ui.horizontal_centered(|ui| {
        ui.strong(&active.title);
        if *state == Some(State::Connecting) {
            ui.spinner();
            ui.weak("подключение…");
        }
        ui.separator();
        let profile = store.profiles.iter().find(|p| p.id == active.profile_id);
        ui.label(keepalive_status(active, profile));
        if keyboard.suspended() {
            ui.separator();
            ui.colored_label(WARN_AMBER, "⌨ клавиатура отпущена — щёлкните по экрану");
        }
        match store.agent.mode {
            AgentMode::Control => {
                ui.separator();
                let busy = hub
                    .last_action()
                    .is_some_and(|t| t.elapsed() < AGENT_FRAME_FADE);
                let label = if busy {
                    "🤖 Claude управляет"
                } else {
                    "🤖 Claude может управлять"
                };
                ui.colored_label(CLAUDE_ACCENT, label);
                if ui
                    .button(RichText::new("⏹ Стоп").color(ERROR_RED))
                    .on_hover_text(
                        "Запретить Claude управлять мышью и клавиатурой — останется только смотреть",
                    )
                    .clicked()
                {
                    actions.stop_agent = true;
                }
            }
            AgentMode::View => {
                ui.separator();
                ui.weak("🤖 Claude только смотрит");
            }
            AgentMode::Off => {}
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            actions.disconnect = ui.button("Отключиться").clicked();
            if let Some(v) = update
                && ui
                    .button(format!("⬆ {v}"))
                    .on_hover_text("Доступна новая версия")
                    .clicked()
            {
                actions.open_updates = true;
            }
            if fullscreen {
                if ui
                    .button("🗗")
                    .on_hover_text("Выйти из полноэкранного режима")
                    .clicked()
                {
                    actions.toggle_fullscreen = true;
                }
                if ui.button("🗕").on_hover_text("Свернуть").clicked() {
                    actions.minimize = true;
                }
                let pin = if active.bar_pinned {
                    "📌 Закреплено"
                } else {
                    "📌"
                };
                if ui
                    .selectable_label(active.bar_pinned, pin)
                    .on_hover_text("Не прятать панель")
                    .clicked()
                {
                    active.bar_pinned = !active.bar_pinned;
                }
            } else if ui.button("⛶").on_hover_text("Во весь экран").clicked() {
                actions.toggle_fullscreen = true;
            }
            ui.separator();
            if ui
                .selectable_label(active.show_keepalive, "⚙ Keep-alive")
                .clicked()
            {
                active.show_keepalive = !active.show_keepalive;
            }
            ui.menu_button("⌨ Клавиатура", |ui| {
                keyboard_ui(ui, &mut store.keyboard, keyboard, actions);
            });
            ui.menu_button("🤖 Claude", |ui| {
                agent_ui(ui, &mut store.agent, hub, agent_error, actions);
            });
            ui.menu_button("🖥 Экран", |ui| {
                if let Some(p) = store
                    .profiles
                    .iter_mut()
                    .find(|p| p.id == active.profile_id)
                {
                    ui.set_min_width(360.0);
                    let before = p.display.clone();
                    display_ui(ui, &mut p.display);
                    ui.separator();
                    ui.weak(
                        "Сглаживание применяется сразу, остальное — при следующем подключении.",
                    );
                    actions.display_changed |= before != p.display;
                }
            });
        });
    });
}

/// Рамка вокруг экрана, пока Claude действует, и метка там, куда он навёл/кликнул.
fn agent_overlay(ui: &egui::Ui, rect: egui::Rect, remote: [usize; 2], hub: &Hub) {
    let painter = ui.painter();
    let mut animating = false;
    if let Some(t) = hub.last_action() {
        let age = t.elapsed();
        if age < AGENT_FRAME_FADE {
            let a = 1.0 - age.as_secs_f32() / AGENT_FRAME_FADE.as_secs_f32();
            painter.rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(3.0, CLAUDE_ACCENT.gamma_multiply(a)),
                egui::StrokeKind::Inside,
            );
            animating = true;
        }
    }
    if let Some(p) = hub.pointer()
        && remote[0] > 0
        && remote[1] > 0
    {
        let age = p.at.elapsed();
        if age < AGENT_MARK_FADE {
            let pos = rect.min
                + egui::vec2(
                    (f32::from(p.x) + 0.5) / remote[0] as f32 * rect.width(),
                    (f32::from(p.y) + 0.5) / remote[1] as f32 * rect.height(),
                );
            let a = 1.0 - age.as_secs_f32() / AGENT_MARK_FADE.as_secs_f32();
            let color = CLAUDE_ACCENT.gamma_multiply(a);
            painter.circle_stroke(pos, 9.0, egui::Stroke::new(2.0, color));
            painter.circle_filled(pos, 2.5, color);
            // Клик — расходящееся кольцо.
            let ripple = age.as_secs_f32() / 0.6;
            if p.click && ripple < 1.0 {
                painter.circle_stroke(
                    pos,
                    9.0 + 22.0 * ripple,
                    egui::Stroke::new(2.0, CLAUDE_ACCENT.gamma_multiply(1.0 - ripple)),
                );
            }
            animating = true;
        }
    }
    if animating {
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }
}

fn agent_ui(
    ui: &mut egui::Ui,
    s: &mut AgentSettings,
    hub: &Hub,
    error: Option<&str>,
    actions: &mut BarActions,
) {
    ui.set_min_width(440.0);
    let before = s.clone();
    ui.label(
        "Claude Code подключается к Tarn по MCP и работает в этой же сессии — \
         вы видите каждое его действие.",
    );
    ui.horizontal(|ui| {
        ui.radio_value(&mut s.mode, AgentMode::Off, "Выключено");
        ui.radio_value(&mut s.mode, AgentMode::View, "Только смотреть");
        ui.radio_value(&mut s.mode, AgentMode::Control, "Управление");
    });
    if s.mode != AgentMode::Off {
        s.ensure_token();
        ui.horizontal(|ui| {
            ui.label("Порт:");
            ui.add(
                egui::DragValue::new(&mut s.port)
                    .range(1024..=65535)
                    .update_while_editing(false),
            );
            ui.weak("только 127.0.0.1");
        });
        if let Some(e) = error {
            ui.colored_label(ERROR_RED, e);
        }
        match hub.last_request() {
            Some(t) => ui.weak(format!(
                "Последний запрос от Claude: {}",
                format_ago(t.elapsed().as_secs())
            )),
            None => ui.weak("Claude ещё не подключался."),
        };
        ui.add_space(4.0);
        ui.label("Один раз выполните в терминале:");
        let cmd = s.claude_command();
        ui.add(egui::Label::new(RichText::new(&cmd).monospace().small()).wrap());
        ui.horizontal(|ui| {
            if ui.button("📋 Скопировать команду").clicked() {
                ui.ctx().copy_text(cmd.clone());
            }
            if ui
                .button("🔄 Новый токен")
                .on_hover_text(
                    "Старая команда перестанет работать — её нужно будет выполнить заново",
                )
                .clicked()
            {
                s.token = agent::new_token();
            }
        });
    }
    ui.separator();
    ui.weak(
        "Остановить: ⏹ на панели. \
         Claude останется только смотреть, пока вы снова не включите «Управление».",
    );
    if ui.button("📜 Журнал действий").clicked() {
        actions.open_agent_log = true;
        ui.close();
    }
    actions.agent_changed |= before != *s;
}

fn format_ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} с назад"),
        60..=3599 => format!("{} мин назад", secs / 60),
        _ => format!("{} ч назад", secs / 3600),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Сколько физических пикселей на точку используем для удалённого стола.
fn effective_ppp(ctx: &egui::Context, d: &DisplaySettings) -> f32 {
    if d.hidpi { ctx.pixels_per_point() } else { 1.0 }
}

fn to_px(size: egui::Vec2, ppp: f32) -> (u16, u16) {
    let px = |v: f32| (v * ppp).round().clamp(200.0, 8192.0) as u16;
    (px(size.x) & !1, px(size.y))
}

fn scale_factor(ppp: f32) -> u32 {
    // RDP допускает 100..=500 %.
    ((ppp * 100.0).round() as u32).clamp(100, 500)
}

fn keepalive_status(active: &Active, profile: Option<&Profile>) -> String {
    let st = active.session.ka_status.lock().unwrap().clone();
    let enabled = profile.is_some_and(|p| p.keepalive.enabled);
    let mut s = if !enabled {
        "keep-alive выключен".to_owned()
    } else if let Some(next) = st.next_at(&active.session.activity) {
        let left = next
            .saturating_duration_since(tokio::time::Instant::now())
            .as_secs();
        format!("keep-alive: через {}:{:02} простоя", left / 60, left % 60)
    } else {
        "keep-alive: ожидание".to_owned()
    };
    if st.fired_count > 0
        && let Some(ago) = st.last_fired.and_then(|t| t.elapsed().ok())
    {
        s += &format!(
            "  ·  срабатывал {} раз, последний {} с назад",
            st.fired_count,
            ago.as_secs()
        );
    }
    s
}

fn connection_ui(ui: &mut egui::Ui, p: &mut Profile, password: &mut String) {
    egui::Grid::new("profile")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label("Название");
            ui.text_edit_singleline(&mut p.name);
            ui.end_row();
            ui.label("Сервер");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut p.host)
                        .hint_text("host или IP")
                        .desired_width(160.0),
                );
                ui.label(":");
                ui.add(egui::DragValue::new(&mut p.port).range(1..=65535));
            });
            ui.end_row();
            ui.label("Пользователь");
            ui.add(
                egui::TextEdit::singleline(&mut p.username)
                    .hint_text("user, DOMAIN\\user или user@domain"),
            );
            ui.end_row();
            ui.label("Домен");
            ui.add(egui::TextEdit::singleline(&mut p.domain).hint_text("необязательно"));
            ui.end_row();
            ui.label("Пароль");
            ui.add(
                egui::TextEdit::singleline(password)
                    .password(true)
                    .hint_text("пусто — спрашивать при подключении"),
            );
            ui.end_row();
        });
}

fn display_ui(ui: &mut egui::Ui, d: &mut DisplaySettings) {
    ui.horizontal(|ui| {
        ui.label("Профиль:");
        let current = d.matching_preset();
        for p in Preset::ALL {
            if ui
                .selectable_label(current == Some(p), p.label())
                .on_hover_text(p.hint())
                .clicked()
            {
                d.apply_preset(p);
            }
        }
        if current.is_none() {
            ui.weak("(свой)");
        }
    });
    ui.add_space(4.0);

    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        ui.strong("Оформление Windows");
        ui.checkbox(&mut d.font_smoothing, "Сглаживание шрифтов (ClearType)");
        ui.checkbox(&mut d.themes, "Темы оформления");
        ui.checkbox(&mut d.wallpaper, "Обои рабочего стола");
        ui.checkbox(&mut d.desktop_composition, "Прозрачность и эффекты (DWM)");
        ui.checkbox(
            &mut d.full_window_drag,
            "Содержимое окна при перетаскивании",
        );
        ui.checkbox(&mut d.menu_animations, "Анимация меню и окон");
        ui.checkbox(&mut d.cursor_shadow, "Тень курсора");

        let ui = &mut cols[1];
        ui.strong("Качество картинки");
        ui.horizontal(|ui| {
            ui.label("Цвет:");
            ui.radio_value(&mut d.color_depth, 32, "32 бит");
            ui.radio_value(&mut d.color_depth, 16, "16 бит");
        });
        ui.checkbox(&mut d.hidpi, "Retina / HiDPI (чётко, но больше трафика)");
        ui.checkbox(&mut d.smooth_scaling, "Сглаживать при масштабировании");
        ui.checkbox(&mut d.compression, "Сжатие трафика (для медленного канала)");
        ui.add_enabled_ui(d.color_depth != 16, |ui| {
            ui.checkbox(&mut d.gfx, "Графический конвейер (GFX)")
                .on_hover_text(
                    "Кадры приходят целиком — без разрывов при перетаскивании окон. \
                     Только для 32 бит. Выключите, если картинка ломается.",
                );
        });
        ui.add_space(4.0);
        ui.strong("Разрешение");
        ui.radio_value(&mut d.auto_resize, true, "По размеру окна");
        ui.horizontal(|ui| {
            ui.radio_value(&mut d.auto_resize, false, "Фиксированное");
            ui.add_enabled_ui(!d.auto_resize, |ui| {
                ui.add(egui::DragValue::new(&mut d.fixed_width).range(640..=8192));
                ui.label("×");
                ui.add(egui::DragValue::new(&mut d.fixed_height).range(480..=8192));
            });
        });
    });
}

fn keepalive_ui(ui: &mut egui::Ui, s: &mut KeepAliveSettings) {
    ui.checkbox(
        &mut s.enabled,
        "Нажимать клавишу, если в сессии нет никаких действий",
    );
    ui.add_enabled_ui(s.enabled, |ui| {
        ui.horizontal(|ui| {
            ui.label("Нажимать:");
            egui::ComboBox::from_id_salt("keepalive_combo")
                .selected_text(s.combo.label())
                .show_ui(ui, |ui| {
                    for c in KeepAliveCombo::ALL {
                        ui.selectable_value(&mut s.combo, c, c.label());
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label("Интервал от");
            ui.add(
                egui::DragValue::new(&mut s.min_secs)
                    .range(MIN_INTERVAL_SECS..=86_400)
                    .suffix(" с"),
            );
            ui.label("до");
            ui.add(
                egui::DragValue::new(&mut s.max_secs)
                    .range(MIN_INTERVAL_SECS..=86_400)
                    .suffix(" с"),
            );
        });
        if s.max_secs < s.min_secs {
            s.max_secs = s.min_secs;
        }
        ui.weak(format!(
            "Нажимает {} после {}–{} с без мыши и клавиатуры в сессии (случайно). \
             Перетаскивание и переключение окон клиента активностью не считаются.",
            s.combo.label(),
            s.min_secs,
            s.max_secs
        ));
    });
}

fn keyboard_ui(
    ui: &mut egui::Ui,
    s: &mut KeyboardSettings,
    kb: &Keyboard,
    actions: &mut BarActions,
) {
    ui.set_min_width(400.0);
    let before = s.clone();
    let mac = cfg!(target_os = "macos");
    let system = if mac {
        "Передавать системные сочетания (⌘Tab, ⌘Space, Mission Control…)"
    } else {
        "Передавать системные сочетания (Win, Alt+Tab, Alt+Esc…)"
    };
    ui.checkbox(&mut s.grab_system_keys, system)
        .on_hover_text("Пока окно Tarn активно, сочетания уходят на сервер, а не в эту систему");
    if s.grab_system_keys {
        match kb.global_status() {
            GlobalStatus::NeedsPermission => {
                ui.colored_label(
                    WARN_AMBER,
                    "Нужен доступ «Универсальный доступ» в Системных настройках → \
                     Конфиденциальность и безопасность. Без него работают только обычные клавиши.",
                );
                if ui.button("Запросить доступ").clicked() {
                    kb.request_permission();
                }
                ui.weak("Перехват включится сам, как только доступ будет выдан.");
            }
            GlobalStatus::Failed => {
                ui.colored_label(ERROR_RED, "Не удалось включить перехват.");
                if let Some(path) = crate::config::log_path() {
                    ui.weak(format!("Подробности — в {}", path.display()));
                }
            }
            GlobalStatus::Starting => {
                ui.weak("Перехват запускается…");
            }
            GlobalStatus::Active | GlobalStatus::Off => {}
        }
    }
    if mac {
        ui.add_space(4.0);
        ui.label("⌘ Command на сервере:");
        for m in MacRemap::ALL {
            ui.radio_value(&mut s.mac_remap, m, m.label());
        }
        ui.weak("⌥ Option — это Alt, правый ⌥ — AltGr.");
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Caps Lock на сервере:");
        ui.radio_value(&mut s.caps_lock, CapsLockMode::CapsLock, "Caps Lock");
        ui.radio_value(&mut s.caps_lock, CapsLockMode::CtrlShift, "Ctrl+Shift")
            .on_hover_text(if mac {
                "Нажатие Caps Lock уходит на сервер как Ctrl+Shift — переключает язык в Windows. \
                 Локальный Caps Lock при этом выключается."
            } else {
                "Caps Lock работает как зажатые Ctrl+Shift — например, для переключения языка."
            });
    });
    ui.separator();
    ui.strong("Сочетания Tarn (на сервер не уходят)");
    let ctrl_alt = if mac { "⌃⌥" } else { "Ctrl+Alt+" };
    egui::Grid::new("kbd_hotkeys")
        .num_columns(2)
        .spacing([12.0, 2.0])
        .show(ui, |ui| {
            for (key, what) in [
                ("Enter", "полноэкранный режим"),
                ("Home", "отпустить клавиатуру (вернуть — щелчком по экрану)"),
                ("End", "Ctrl+Alt+Del на сервере"),
            ] {
                ui.monospace(format!("{ctrl_alt}{key}"));
                ui.label(what);
                ui.end_row();
            }
        });
    if ui.button("Отправить Ctrl+Alt+Del").clicked() {
        kb.send_ctrl_alt_del();
        ui.close();
    }
    ui.weak("Символы печатает раскладка сервера: переключайте язык на удалённой машине.");
    actions.keyboard_changed |= before != *s;
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        while let Some(cmd) = self.keyboard.poll() {
            match cmd {
                keyboard::Command::ToggleFullscreen if self.active.is_some() => {
                    let fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
                }
                keyboard::Command::ToggleFullscreen | keyboard::Command::CaptureReleased => {}
            }
        }
        if self.active.is_some() {
            self.ui_session(ui);
            // Обновляем обратный отсчёт keep-alive раз в секунду.
            ctx.request_repaint_after(Duration::from_secs(1));
        } else {
            self.hub.set_target(None);
            self.keyboard.attach(None);
            self.ui_list(ui);
            self.ui_editor(&ctx);
            self.ui_password_prompt(&ctx);
        }
        self.ui_updates(&ctx);
        self.ui_agent_log(&ctx);
    }
}
