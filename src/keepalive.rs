//! Keep-alive: если внутри сессии нет действий пользователя (на сервер не
//! уходили ни мышь, ни клавиатура) дольше случайного интервала, на сервер
//! уходит сочетание клавиш Alt+Tab. Фокус окна клиента
//! значения не имеет.
//!
//! Работает в отдельной tokio-задаче, а не в UI-цикле: eframe не вызывает
//! `update()` у свёрнутого окна.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ironrdp_client::rdp::RdpInputEvent;
use rand::RngExt as _;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::keyboard::scancode::{LALT, Scancode, TAB};

pub const MIN_INTERVAL_SECS: u32 = 5;
/// Пауза между событиями сочетания: некоторые программы не видят слишком быстрые нажатия.
const KEY_GAP: Duration = Duration::from_millis(40);

/// Что нажимает keep-alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum KeepAliveCombo {
    #[default]
    AltTab,
}

impl KeepAliveCombo {
    pub const ALL: [Self; 1] = [Self::AltTab];

    pub fn label(self) -> &'static str {
        match self {
            Self::AltTab => "Alt+Tab",
        }
    }

    /// Клавиши в порядке нажатия.
    fn keys(self) -> [Scancode; 2] {
        match self {
            Self::AltTab => [LALT, TAB],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct KeepAliveSettings {
    pub enabled: bool,
    /// Что нажимать.
    #[serde(deserialize_with = "crate::config::lenient")]
    pub combo: KeepAliveCombo,
    /// Интервал выбирается случайно в диапазоне [min, max] секунд.
    pub min_secs: u32,
    pub max_secs: u32,
}

impl Default for KeepAliveSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            combo: KeepAliveCombo::default(),
            min_secs: 60,
            max_secs: 120,
        }
    }
}

impl KeepAliveSettings {
    pub fn sample_delay(&self, rng: &mut impl rand::Rng) -> Duration {
        let min = self.min_secs.max(MIN_INTERVAL_SECS);
        let max = self.max_secs.max(min);
        let ms = rng.random_range(u64::from(min) * 1000..=u64::from(max) * 1000);
        Duration::from_millis(ms)
    }
}

/// Время последнего действия пользователя внутри сессии (мышь или
/// клавиатура, реально ушедшие на сервер). Перетаскивание окна клиента, переключение
/// приложений и т.п. сюда не попадают.
#[derive(Clone)]
pub struct Activity(Arc<Mutex<Instant>>);

impl Default for Activity {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }
}

impl Activity {
    pub fn touch(&self) {
        *self.0.lock().unwrap() = Instant::now();
    }

    pub fn last(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    /// Текущий выбранный интервал простоя (None — keep-alive выключен).
    pub delay: Option<Duration>,
    /// Когда keep-alive сработал в последний раз (для отсчёта простоя).
    pub last_fired_at: Option<Instant>,
    pub last_fired: Option<SystemTime>,
    pub fired_count: u64,
}

impl Status {
    /// Простой отсчитывается от последнего действия пользователя или последнего срабатывания.
    fn idle_since(&self, activity: &Activity) -> Instant {
        let last = activity.last();
        self.last_fired_at.map_or(last, |t| t.max(last))
    }

    /// Когда сработает следующий keep-alive, если пользователь ничего не сделает.
    pub fn next_at(&self, activity: &Activity) -> Option<Instant> {
        Some(self.idle_since(activity) + self.delay?)
    }
}

/// Нажать сочетание: модификаторы вниз, клавиша вниз/вверх, модификаторы
/// вверх в обратном порядке. Активностью пользователя не считается.
async fn press(
    combo: KeepAliveCombo,
    input: &mpsc::UnboundedSender<RdpInputEvent>,
) -> Result<(), String> {
    let keys = combo.keys();
    let seq = keys
        .iter()
        .map(|&k| (k, true))
        .chain(keys.iter().rev().map(|&k| (k, false)));
    for (i, (key, down)) in seq.enumerate() {
        if i > 0 {
            tokio::time::sleep(KEY_GAP).await;
        }
        let events = key.events(down);
        if !events.is_empty() {
            input
                .send(RdpInputEvent::FastPath(events))
                .map_err(|_| "сессия закрыта".to_owned())?;
        }
    }
    Ok(())
}

pub async fn run(
    mut settings: watch::Receiver<KeepAliveSettings>,
    input: mpsc::UnboundedSender<RdpInputEvent>,
    activity: Activity,
    status: Arc<Mutex<Status>>,
    repaint: impl Fn() + Send + 'static,
) {
    let set_delay = |delay: Option<Duration>| {
        status.lock().unwrap().delay = delay;
        repaint();
    };

    'outer: loop {
        let s = settings.borrow_and_update().clone();
        if !s.enabled {
            set_delay(None);
            if settings.changed().await.is_err() {
                break;
            }
            continue;
        }

        // Для каждого срабатывания — свой случайный интервал простоя.
        let delay = s.sample_delay(&mut rand::rng());
        set_delay(Some(delay));
        loop {
            let deadline = status.lock().unwrap().idle_since(&activity) + delay;
            if Instant::now() >= deadline {
                break;
            }
            // Проснулись — перепроверяем: пользователь мог что-то сделать, и простой начался заново.
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {}
                r = settings.changed() => {
                    if r.is_err() {
                        break 'outer;
                    }
                    continue 'outer;
                }
            }
        }

        let sent = press(s.combo, &input).await;
        let mut st = status.lock().unwrap();
        st.last_fired_at = Some(Instant::now());
        match sent {
            Ok(()) => {
                st.last_fired = Some(SystemTime::now());
                st.fired_count += 1;
                tracing::info!(count = st.fired_count, combo = %s.combo.label(), "keep-alive");
            }
            Err(e) => tracing::warn!(%e, "keep-alive: сочетание не отправлено"),
        }
        drop(st);
        repaint();
        if input.is_closed() {
            break;
        }
    }
    set_delay(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_within_range() {
        let s = KeepAliveSettings {
            min_secs: 10,
            max_secs: 20,
            ..Default::default()
        };
        let mut rng = rand::rng();
        for _ in 0..1000 {
            let d = s.sample_delay(&mut rng);
            assert!(
                d >= Duration::from_secs(10) && d <= Duration::from_secs(20),
                "{d:?}"
            );
        }
    }

    #[test]
    fn delay_clamps_bad_ranges() {
        let s = KeepAliveSettings {
            min_secs: 0,
            max_secs: 0,
            ..Default::default()
        };
        assert_eq!(
            s.sample_delay(&mut rand::rng()),
            Duration::from_secs(u64::from(MIN_INTERVAL_SECS))
        );
        let reversed = KeepAliveSettings {
            min_secs: 30,
            max_secs: 10,
            ..Default::default()
        };
        assert_eq!(
            reversed.sample_delay(&mut rand::rng()),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn unknown_combo_falls_back_to_default() {
        let s: KeepAliveSettings = serde_json::from_str(
            r#"{"enabled":false,"combo":{"key":"F14"},"min_secs":30,"max_secs":40}"#,
        )
        .unwrap();
        assert!(!s.enabled);
        assert_eq!(s.combo, KeepAliveCombo::AltTab);
        assert_eq!((s.min_secs, s.max_secs), (30, 40));
        // Убранный вариант из прежних версий.
        let s: KeepAliveSettings = serde_json::from_str(r#"{"combo":"AltPageUp"}"#).unwrap();
        assert_eq!(s.combo, KeepAliveCombo::AltTab);
    }

    #[test]
    fn combo_serde() {
        let parse = |j: &str| serde_json::from_str::<KeepAliveCombo>(j).unwrap();
        for c in KeepAliveCombo::ALL {
            assert_eq!(parse(&serde_json::to_string(&c).unwrap()), c);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fires_only_after_idle_in_session() {
        let (set_tx, set_rx) = watch::channel(KeepAliveSettings {
            min_secs: 10,
            max_secs: 10,
            ..Default::default()
        });
        let activity = Activity::default();
        let status = Arc::new(Mutex::new(Status::default()));
        let (input, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(set_rx, input, activity.clone(), status.clone(), || {}));
        let fired = || status.lock().unwrap().fired_count;

        // Пользователь что-то делает каждые 5 с — keep-alive молчит.
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            activity.touch();
        }
        assert_eq!(fired(), 0);

        // 10 с без действий в сессии — срабатывает и нажимает Alt+Tab.
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert_eq!(fired(), 1);
        let mut keys = Vec::new();
        while let Ok(RdpInputEvent::FastPath(evs)) = rx.try_recv() {
            keys.extend(evs);
        }
        assert_eq!(keys.len(), 4, "{keys:?}");

        // Дальше без действий — повторяется каждые 10 с (от прошлого срабатывания).
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(fired(), 2);

        // Выключили — тишина.
        set_tx.send_modify(|s| s.enabled = false);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(fired(), 2);
        assert!(status.lock().unwrap().delay.is_none());

        drop(set_tx);
        task.await.unwrap();
    }
}
