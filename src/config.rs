//! Сохранённые подключения: профили в JSON в каталоге конфигурации ОС.
//! Пароли здесь не хранятся — см. [`crate::secrets`].

use std::path::PathBuf;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::AgentSettings;
use crate::display::DisplaySettings;
use crate::keepalive::KeepAliveSettings;
use crate::keyboard::KeyboardSettings;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Profile {
    /// Стабильный идентификатор (используется как ключ в хранилище паролей).
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub domain: String,
    pub keepalive: KeepAliveSettings,
    pub display: DisplaySettings,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: String::new(),
            host: String::new(),
            port: 3389,
            username: String::new(),
            domain: String::new(),
            keepalive: KeepAliveSettings::default(),
            display: DisplaySettings::default(),
        }
    }
}

impl Profile {
    pub fn title(&self) -> String {
        if self.name.trim().is_empty() {
            self.host.clone()
        } else {
            self.name.clone()
        }
    }
}

fn new_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("p{nanos:x}{:04x}", rand::random::<u16>())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub profiles: Vec<Profile>,
    pub updates: UpdateSettings,
    /// Доступ для Claude (MCP) — общий для всех подключений.
    pub agent: AgentSettings,
    /// Клавиатура — общая для всех подключений.
    pub keyboard: KeyboardSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateSettings {
    /// Проверять обновления на GitHub при запуске (не чаще раза в сутки).
    pub auto_check: bool,
    /// Время последней автопроверки, секунды Unix.
    pub last_check: u64,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            auto_check: true,
            last_check: 0,
        }
    }
}

impl UpdateSettings {
    pub fn due(&self, now: u64) -> bool {
        self.auto_check && now.saturating_sub(self.last_check) >= 24 * 60 * 60
    }
}

fn store_path() -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "tarn", "tarn")
        .context("не удалось определить каталог конфигурации")?;
    Ok(dirs.config_dir().join("profiles.json"))
}

/// Для `#[serde(deserialize_with)]`: неизвестное значение (из другой версии)
/// превращается в значение по умолчанию, чтобы файл настроек всё равно читался.
pub fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::de::DeserializeOwned,
{
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_default())
}

/// Лог текущего запуска (рядом с профилями): в Windows-сборке нет консоли.
pub fn log_path() -> Option<PathBuf> {
    Some(store_path().ok()?.with_file_name("tarn.log"))
}

impl Store {
    pub fn load() -> anyhow::Result<Self> {
        let path = store_path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("ошибка чтения {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("ошибка чтения {}", path.display())),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = store_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_missing_fields() {
        let mut s = Store::default();
        s.profiles.push(Profile {
            name: "srv".into(),
            host: "10.0.0.1".into(),
            ..Default::default()
        });
        let json = serde_json::to_string(&s).unwrap();
        let back: Store = serde_json::from_str(&json).unwrap();
        assert_eq!(back.profiles, s.profiles);

        // Старые/неполные файлы должны читаться с дефолтами.
        let partial: Store = serde_json::from_str(r#"{"profiles":[{"host":"h"}]}"#).unwrap();
        assert_eq!(partial.profiles[0].port, 3389);
        assert!(partial.profiles[0].keepalive.enabled);

        // Настройки клавиатуры читаются, неизвестные поля игнорируются.
        let old: Store = serde_json::from_str(
            r#"{"keyboard":{"grab_system_keys":false,"mac_remap":"Smart"},"profiles":[],"x":1}"#,
        )
        .unwrap();
        assert!(old.profiles.is_empty());
        assert!(!old.keyboard.grab_system_keys);
        assert!(partial.keyboard.grab_system_keys);
    }
}
