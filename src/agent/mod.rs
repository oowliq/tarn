//! Доступ для Claude: локальный MCP-сервер, через который Claude видит экран
//! RDP-сессии и (если пользователь разрешил) управляет мышью и клавиатурой.
//!
//! Claude работает в той же сессии, что и пользователь, поэтому каждое его
//! действие сразу видно в окне Tarn. Сервер слушает только 127.0.0.1 и
//! требует токен; по умолчанию доступ выключен.

mod http;
mod mcp;
mod tools;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime};

use ironrdp_client::rdp::RdpInputEvent;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub use http::Server;

use crate::keepalive::Activity;
use crate::session::Shared;

pub const DEFAULT_PORT: u16 = 47800;
const LOG_LIMIT: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AgentMode {
    /// Сервер выключен.
    #[default]
    Off,
    /// Claude видит экран, но не может ничего нажимать.
    View,
    /// Claude видит экран и управляет мышью и клавиатурой.
    Control,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    pub mode: AgentMode,
    pub port: u16,
    /// Токен для заголовка `Authorization: Bearer …`.
    pub token: String,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            mode: AgentMode::Off,
            port: DEFAULT_PORT,
            token: String::new(),
        }
    }
}

impl AgentSettings {
    /// Сгенерировать токен, если его ещё нет. true — токен изменился.
    pub fn ensure_token(&mut self) -> bool {
        if self.token.is_empty() {
            self.token = new_token();
            true
        } else {
            false
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// Команда, которая регистрирует сервер в Claude Code.
    pub fn claude_command(&self) -> String {
        format!(
            "claude mcp add --scope user --transport http tarn {} --header \"Authorization: Bearer {}\"",
            self.url(),
            self.token
        )
    }
}

pub fn new_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// К какой сессии подключён Claude: кадр экрана и канал ввода.
#[derive(Clone)]
pub struct Target {
    pub shared: Arc<Mutex<Shared>>,
    pub input: mpsc::UnboundedSender<RdpInputEvent>,
    pub activity: Activity,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: SystemTime,
    pub text: String,
    pub error: bool,
}

/// Куда Claude последний раз навёл/кликнул (пиксели удалённого экрана).
#[derive(Debug, Clone, Copy)]
pub struct Pointer {
    pub x: u16,
    pub y: u16,
    pub at: Instant,
    pub click: bool,
}

#[derive(Default)]
struct HubState {
    mode: AgentMode,
    token: String,
    target: Option<Target>,
    log: VecDeque<LogEntry>,
    last_action: Option<Instant>,
    last_request: Option<Instant>,
    pointer: Option<Pointer>,
}

struct HubInner {
    state: Mutex<HubState>,
    /// Действия Claude выполняются по одному, даже если он шлёт их параллельно.
    exec: Mutex<()>,
    repaint: Box<dyn Fn() + Send + Sync>,
}

/// Общее состояние между UI и MCP-сервером.
#[derive(Clone)]
pub struct Hub(Arc<HubInner>);

impl Hub {
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(HubInner {
            state: Mutex::new(HubState::default()),
            exec: Mutex::new(()),
            repaint: Box::new(repaint),
        }))
    }

    fn state(&self) -> MutexGuard<'_, HubState> {
        self.0.state.lock().unwrap()
    }

    fn repaint(&self) {
        (self.0.repaint)();
    }

    pub fn mode(&self) -> AgentMode {
        self.state().mode
    }

    pub fn set_mode(&self, mode: AgentMode) {
        self.state().mode = mode;
        self.repaint();
    }

    pub fn control_allowed(&self) -> bool {
        self.mode() == AgentMode::Control
    }

    pub fn set_token(&self, token: &str) {
        self.state().token = token.to_owned();
    }

    /// Сравнение без раннего выхода, чтобы токен нельзя было подобрать по времени ответа.
    pub fn token_matches(&self, candidate: &str) -> bool {
        let st = self.state();
        let (a, b) = (st.token.as_bytes(), candidate.as_bytes());
        !a.is_empty()
            && a.len() == b.len()
            && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }

    pub fn set_target(&self, target: Option<Target>) {
        let mut st = self.state();
        if target.is_none() && st.target.is_none() {
            return;
        }
        st.target = target;
        st.pointer = None;
    }

    pub fn target(&self) -> Option<Target> {
        self.state().target.clone()
    }

    pub fn log(&self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error {
            tracing::warn!(%text, "claude");
        } else {
            tracing::info!(%text, "claude");
        }
        let mut st = self.state();
        if st.log.len() >= LOG_LIMIT {
            st.log.pop_front();
        }
        st.log.push_back(LogEntry {
            at: SystemTime::now(),
            text,
            error,
        });
        drop(st);
        self.repaint();
    }

    /// Новые записи — первыми.
    pub fn log_entries(&self) -> Vec<LogEntry> {
        self.state().log.iter().rev().cloned().collect()
    }

    pub fn clear_log(&self) {
        self.state().log.clear();
    }

    fn note_request(&self) {
        self.state().last_request = Some(Instant::now());
    }

    pub fn last_request(&self) -> Option<Instant> {
        self.state().last_request
    }

    /// Claude что-то сделал в сессии (для рамки и метки курсора в UI).
    fn note_action(&self, pointer: Option<(u16, u16, bool)>) {
        let now = Instant::now();
        let mut st = self.state();
        st.last_action = Some(now);
        if let Some((x, y, click)) = pointer {
            st.pointer = Some(Pointer {
                x,
                y,
                at: now,
                click,
            });
        }
        drop(st);
        self.repaint();
    }

    pub fn last_action(&self) -> Option<Instant> {
        self.state().last_action
    }

    pub fn pointer(&self) -> Option<Pointer> {
        self.state().pointer
    }

    fn exec_lock(&self) -> MutexGuard<'_, ()> {
        self.0.exec.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_compare() {
        let hub = Hub::new(|| {});
        assert!(!hub.token_matches(""));
        hub.set_token("abc");
        assert!(hub.token_matches("abc"));
        assert!(!hub.token_matches("abd"));
        assert!(!hub.token_matches("ab"));
    }

    #[test]
    fn settings_defaults_and_command() {
        let mut s: AgentSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.mode, AgentMode::Off);
        assert_eq!(s.port, DEFAULT_PORT);
        assert!(s.ensure_token());
        assert_eq!(s.token.len(), 32);
        assert!(!s.ensure_token());
        assert!(s.claude_command().contains(&format!("Bearer {}", s.token)));
    }

    #[test]
    fn log_is_bounded_and_newest_first() {
        let hub = Hub::new(|| {});
        for i in 0..LOG_LIMIT + 5 {
            hub.log(i.to_string(), false);
        }
        let entries = hub.log_entries();
        assert_eq!(entries.len(), LOG_LIMIT);
        assert_eq!(entries[0].text, (LOG_LIMIT + 4).to_string());
    }
}
