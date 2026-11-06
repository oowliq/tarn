# AGENTS.md

Памятка для ИИ-агентов, работающих с этим репозиторием. Пользовательская документация — в `README.md`.

## Что это

Tarn — десктопный RDP-клиент (macOS + Windows) на Rust: egui/eframe + IronRDP. Сверх базового RDP: GFX (MS-RDPEGFX), клавиатура скан-кодами с перехватом системных сочетаний и ремапом ⌘, keep-alive, встроенный MCP-сервер для внешних агентов, автообновление из GitHub Releases.

Имя: в текстах для людей — «Tarn», в идентификаторах — `tarn` (крейт, бинарник, `tarn.app`, bundle id `dev.tarn.tarn`, сервис keyring, каталог конфигурации, `TARN_VERSION`, имена ассетов, `update::REPO` = `oowliq/tarn`, имя MCP-сервера). Идентификаторы без явной просьбы не менять: сломаются сохранённые профили, пароли и автообновление.

## Команды

```bash
cargo build                                  # dev-сборка
cargo test --locked                          # все тесты (юнит, в модулях)
cargo clippy --locked --all-targets -- -D warnings   # CI падает на любом warning
cargo fmt
packaging/macos/build-app.sh [версия]        # universal .app + zip в dist/ (TARN_SIGN_IDENTITY — постоянная подпись)
packaging/macos/make-signing-cert.sh         # один раз: самоподписанный сертификат подписи
```

CI (`.github/workflows/build.yml`) гоняет clippy `-D warnings`, тесты и release-сборку на `windows-latest` и `macos-latest`. Перед тем как считать задачу готовой — clippy и тесты должны проходить. Windows-код (`#[cfg(windows)]`, `keyboard/windows.rs`, ветки в `update.rs`) на macOS не компилируется, а весь проект под `x86_64-pc-windows-msvc` с macOS не собрать (aws-lc-sys нужен `windows.h`). Проверить его можно мини-крейтом: `#[path = ".../src/keyboard/mod.rs"] mod keyboard;` плюс заглушки `ironrdp-client` (`rdp::RdpInputEvent::FastPath`), `eframe` (`CreationContext`, `Context::request_repaint`) и `keepalive::Activity`, затем `cargo clippy --target x86_64-pc-windows-msvc -- -D warnings`. Тесты Windows-части (`keyboard::windows::tests`) выполняются только в CI на Windows.

Тулчейн на машине разработчика — Homebrew-rustup (keg-only): если `cargo` не находится, добавь в PATH `/opt/homebrew/opt/rustup/bin`. В сэндбоксе агента `~/.cargo` не пишется — используй временный `CARGO_HOME` (например, в `$TMPDIR`), а для crates.io нужен доступ к `index.crates.io` и `static.crates.io`. Тест `agent::http::tests::serves_over_tcp_and_stops` слушает порт на `127.0.0.1` и в сэндбоксе без разрешения на локальный bind падает с `EPERM`: это сэндбокс, а не баг.

Сетевой тест `update::tests::live_check_and_download` помечен `#[ignore]`, запуск: `cargo test -- --ignored live_`.

## Карта кода

| Файл | Ответственность |
|---|---|
| `main.rs` | rustls-провайдер (aws-lc-rs), лог в stderr и `tarn.log`, tokio runtime на 2 потока, `--self-update`, запуск eframe |
| `app.rs` | весь UI (≈1600 строк): профили, редактор, пароль, панель сессии, меню, полноэкранный режим, оверлей Claude, окна настроек |
| `session.rs` | `Session`: конфиг ironrdp, поток движка (свой current-thread runtime — future не `Send`), приём кадров в `Shared`, сборка целых кадров, `FrameStats` |
| `gfx.rs` | `GraphicsPipelineHandler`: поверхности, Progressive/ClearCodec/Planar/uncompressed, solid fill, surface↔surface, кэш; кадр отдаётся в `Shared` по EndFrame |
| `display.rs` | `DisplaySettings`, пресеты, `PerformanceFlags` |
| `config.rs` | `Store`/`Profile` → `profiles.json` (атомарная запись через `.tmp` + rename), путь лога, `lenient` для полей-перечислений |
| `secrets.rs` | keyring, сервис `tarn`, ключ — `profile.id` |
| `keepalive.rs` | tokio-задача keep-alive: нажимает `KeepAliveCombo` (сейчас только Alt+Tab) после простоя; `Activity` — время последнего ввода, реально ушедшего на сервер |
| `input.rs` | egui-события → fast-path (мышь, колесо) |
| `keyboard/mod.rs` | `Keyboard` (API для UI), `KeyboardSettings`/`MacRemap`, выбор платформенного бэкенда |
| `keyboard/router.rs` | `Router`: гейт захвата, нажатые клавиши, счётчики на сервере, хоткеи Ctrl+Alt+Enter/Home/End |
| `keyboard/remap.rs` | чем становится ⌘ на Mac (умно / Win / Ctrl) |
| `keyboard/scancode.rs` | `Scancode`, имена клавиш, Unicode-события, Lock-синхронизация |
| `keyboard/macos.rs`, `mac_keymap.rs` | монитор NSEvent + CGEventTap (нужен «Универсальный доступ»), коды kVK → скан-коды |
| `keyboard/windows.rs` | сабкласс окна + `WH_KEYBOARD_LL` в своём потоке; снимает raw input клавиатуры, который регистрирует winit (иначе Windows не зовёт хук, пока окно Tarn в фокусе) |
| `cursor.rs` | серверный курсор → `egui::CustomCursorImage` с учётом масштаба |
| `update.rs` | GitHub Releases: проверка, загрузка, SHA-256, замена бинарника/.app, перезапуск |
| `agent/mod.rs` | `AgentSettings`, `Hub` (общее состояние UI ↔ MCP), режимы `Off/View/Control`, журнал |
| `agent/http.rs` | самописный HTTP/1.1-сервер, только `127.0.0.1`, поток на соединение, `Connection: close` |
| `agent/mcp.rs` | JSON-RPC/MCP: modern `2026-07-28` + legacy `2025-*`; проверки Host/Origin/Bearer; `INSTRUCTIONS` для модели |
| `agent/tools.rs` | инструменты `screenshot/zoom/click/move_mouse/drag/scroll/type_text/press_keys/wait` |

## Vendored IronRDP

`vendor/ironrdp-client`, `vendor/ironrdp-connector` подменяются через `[patch.crates-io]`; `vendor/ironrdp-egfx` — неизменённая копия. Все локальные правки помечены комментарием `[tarn]` и перечислены в `vendor/*/README.md`. Если меняешь vendor — добавь метку и строку в этот README.

Важно: в проекте одновременно живут **две версии** ironrdp-стека:
- `ironrdp-pdu` 0.9 / движок клиента (старый стек, на нём vendor);
- `ironrdp-graphics-next` / `ironrdp-pdu-next` 0.11 — только декодеры GFX (в 0.9 они ломались на потоках Windows). Им передаются только байты, типы между стеками не смешивать.

## Инварианты и подводные камни

- **Один crypto-провайдер rustls.** В процессе должен быть только aws-lc-rs. `ureq` собран без своего провайдера. Не включай `ring` ни в какой зависимости — rustls запаникует на TLS-рукопожатии RDP (есть тест `rustls_has_single_crypto_provider`).
- **Keep-alive не в UI-цикле.** eframe не вызывает `update()` у свёрнутого окна, поэтому всё, что должно работать в фоне (keep-alive, приём кадров), живёт в tokio-задачах. Кадры не должны блокировать движок, если UI не рисует.
- **`Activity::touch()` — только для ввода, реально ушедшего на сервер.** Иначе keep-alive перестанет срабатывать.
- **Зажатые кнопки и клавиши.** При потере фокуса вызывается `InputState::release_all()`, а `Keyboard::set_active(false)` отпускает всё нажатое на сервере. Любая новая отправка ввода должна оставлять сервер в сбалансированном состоянии (press/release парами); в MCP отпускания идут через `Ctl::release`/`holding` даже после «Стоп».
- **Windows: raw input клавиатуры снят.** winit при создании цикла событий регистрирует raw input мыши и клавиатуры; пока он зарегистрирован, Windows не вызывает наш `WH_KEYBOARD_LL`, если окно Tarn в фокусе, — Win и Alt+Tab уходят в локальную систему. `windows::Backend::start` снимает регистрацию клавиатуры (`unregister_raw_keyboard`, тест `raw_keyboard_registration_is_removed`). Не регистрируй raw input клавиатуры и не включай `DeviceEvents` для неё.
- **Клавиатура: два перехватчика на ОС.** Локальный (NSEvent-монитор / сабкласс окна) и глобальный (CGEventTap / `WH_KEYBOARD_LL`) зовут один `Router`. Что забрал глобальный, локальный не видит. Нажатие уходит на сервер только при `Gate::capturing()`, отпускание клавиши, нажатой на сервере, — всегда. Колбэки ОС должны быть быстрыми: никакой блокирующей работы, только `Router::handle`.
- **GFX только при 32 битах.** `d.gfx && color_depth != 16`.
- **MCP-безопасность.** Не ослабляй: bind на `127.0.0.1`, Bearer-токен с constant-time сравнением, проверки `Host`/`Origin`, режим `View` блокирует любое управление, `Hub::exec_lock` сериализует действия. Ответ на управляющий инструмент — только после `RdpInputEvent::Flush` (ввод записан в сокет).
- **Координаты агента** — в пикселях уменьшенного скриншота (`shot_size`), это чистая функция размера экрана; не добавляй состояние между вызовами.
- **Конфиг обратно совместим.** Все структуры с `#[serde(default)]`; новые поля добавляй с дефолтами, старые файлы должны читаться (тест в `config.rs`).
- **Версия** берётся из `TARN_VERSION` при сборке, иначе из `Cargo.toml`.

## Стиль

- Комментарии, doc-комментарии, тексты UI, сообщения об ошибках и логи — **на русском**. Сохраняй этот стиль.
- Ошибки: `anyhow` с `.context(...)`; в инструментах MCP — `Result<_, String>` с понятным для модели текстом.
- Тесты — юнит-тесты в `#[cfg(test)] mod tests` того же файла; для async — `#[tokio::test(start_paused = true)]` с виртуальным временем.
- Новых зависимостей без нужды не добавлять (HTTP-сервер MCP написан вручную именно поэтому).
- Коммиты делать только по просьбе пользователя.
