//! Проверка и установка обновлений из GitHub Releases.
//!
//! Сеть: GET к api.github.com (последний релиз) и, по кнопке, скачивание
//! архива и SHA256SUMS.txt с github.com. Никаких данных о пользователе
//! не отправляется, кроме стандартного User-Agent `tarn/<версия>`.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, bail};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

pub const REPO: &str = "oowliq/tarn";
const SUMS_NAME: &str = "SHA256SUMS.txt";
/// Защита от неадекватно больших ответов.
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;

/// Версия этой сборки: из тега релиза (`TARN_VERSION` при сборке в CI) или из Cargo.toml.
pub fn current_version() -> semver::Version {
    option_env!("TARN_VERSION")
        .and_then(|v| semver::Version::parse(v.trim().trim_start_matches('v')).ok())
        .unwrap_or_else(|| {
            semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("valid Cargo version")
        })
}

/// Архив релиза для этой платформы.
pub fn asset_name() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("tarn-macos-universal.zip")
    } else if cfg!(windows) {
        Some("tarn-windows-x64.zip")
    } else {
        None
    }
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: semver::Version,
    pub notes: String,
    pub page_url: String,
    pub asset_url: String,
    pub asset_name: String,
    pub asset_size: u64,
    pub sums_url: String,
}

/// Разобрать ответ GitHub; `None` — релиз не новее текущей версии.
fn parse_release(
    gh: GhRelease,
    current: &semver::Version,
    asset: &str,
) -> anyhow::Result<Option<Release>> {
    if gh.draft || gh.prerelease {
        return Ok(None);
    }
    let version = semver::Version::parse(gh.tag_name.trim_start_matches('v'))
        .with_context(|| format!("непонятная версия релиза: {}", gh.tag_name))?;
    if version <= *current {
        return Ok(None);
    }
    let find = |name: &str| gh.assets.iter().find(|a| a.name == name);
    let archive =
        find(asset).with_context(|| format!("в релизе {} нет файла {asset}", gh.tag_name))?;
    let sums =
        find(SUMS_NAME).with_context(|| format!("в релизе {} нет {SUMS_NAME}", gh.tag_name))?;
    Ok(Some(Release {
        version,
        notes: gh.body.unwrap_or_default(),
        page_url: gh.html_url,
        asset_url: archive.browser_download_url.clone(),
        asset_name: archive.name.clone(),
        asset_size: archive.size,
        sums_url: sums.browser_download_url.clone(),
    }))
}

/// Найти хеш файла в формате `sha256sum`: `<hex>  <имя>`.
fn expected_hash(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent(format!("tarn/{}", current_version()))
        .build()
        .new_agent()
}

pub fn check() -> anyhow::Result<Option<Release>> {
    let asset = asset_name().context("обновления для этой платформы не публикуются")?;
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut resp = match agent()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(r) => r,
        // У GitHub /releases/latest отвечает 404, пока нет ни одного релиза.
        Err(ureq::Error::StatusCode(404)) => {
            bail!("в репозитории {REPO} пока нет опубликованных релизов")
        }
        Err(e) => return Err(e).context("не удалось связаться с GitHub"),
    };
    let gh: GhRelease = resp
        .body_mut()
        .read_json()
        .context("неожиданный ответ GitHub")?;
    parse_release(gh, &current_version(), asset)
}

/// Скачать архив, сверить SHA256 и вернуть путь к нему.
fn download(release: &Release, dir: &Path, progress: &dyn Fn(u64, u64)) -> anyhow::Result<PathBuf> {
    let agent = agent();
    let sums = agent
        .get(&release.sums_url)
        .call()?
        .body_mut()
        .read_to_string()
        .context("не удалось скачать SHA256SUMS.txt")?;
    let expected = expected_hash(&sums, &release.asset_name)
        .with_context(|| format!("в SHA256SUMS.txt нет хеша для {}", release.asset_name))?;

    let mut resp = agent.get(&release.asset_url).call()?;
    let total = resp.body().content_length().unwrap_or(release.asset_size);
    if total > MAX_DOWNLOAD {
        bail!("архив слишком большой ({total} байт)");
    }
    let mut reader = resp.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
    let path = dir.join(&release.asset_name);
    let mut file = std::fs::File::create(&path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf).context("обрыв при скачивании")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        std::io::Write::write_all(&mut file, &buf[..n])?;
        done += n as u64;
        progress(done, total);
    }
    let actual = hex(&hasher.finalize());
    if actual != expected {
        let _ = std::fs::remove_file(&path);
        bail!("контрольная сумма не совпала: ожидалась {expected}, получена {actual}");
    }
    Ok(path)
}

// ── Установка ────────────────────────────────────────────────────────────

/// Удалить остатки прошлого обновления (вызывать при старте).
pub fn cleanup_previous() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }
    #[cfg(target_os = "macos")]
    if let Some(parent) = macos_bundle().and_then(|b| b.parent().map(Path::to_path_buf)) {
        let _ = std::fs::remove_dir_all(parent.join(".tarn-old.app"));
        let _ = std::fs::remove_dir_all(parent.join(".tarn-update"));
    }
}

/// Путь к `tarn.app`, если запущены из бандла.
#[cfg(target_os = "macos")]
fn macos_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    let bundle = contents.parent()?;
    (macos_dir.ends_with("MacOS")
        && contents.ends_with("Contents")
        && bundle.extension().is_some_and(|e| e == "app"))
    .then(|| bundle.to_path_buf())
}

#[cfg(target_os = "macos")]
fn install(zip: &Path, relaunch: bool) -> anyhow::Result<()> {
    use std::process::Command;

    let exe = std::env::current_exe()?.canonicalize()?;
    let bundle = macos_bundle();
    let parent = match &bundle {
        Some(b) => b
            .parent()
            .context("нет родительского каталога")?
            .to_path_buf(),
        None => exe
            .parent()
            .context("нет каталога программы")?
            .to_path_buf(),
    };
    // Распаковываем рядом, чтобы замена была переименованием в пределах одного диска.
    let staging = parent.join(".tarn-update");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).context("нет прав на запись рядом с программой")?;
    let status = Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(zip)
        .arg(&staging)
        .status()?;
    if !status.success() {
        bail!("не удалось распаковать архив");
    }
    let new_app = staging.join("tarn.app");
    if !new_app.join("Contents/MacOS/tarn").is_file() {
        bail!("в архиве нет tarn.app");
    }

    match bundle {
        Some(bundle) => {
            let old = parent.join(".tarn-old.app");
            let _ = std::fs::remove_dir_all(&old);
            std::fs::rename(&bundle, &old).context("не удалось заменить приложение (нет прав?)")?;
            if let Err(e) = std::fs::rename(&new_app, &bundle) {
                let _ = std::fs::rename(&old, &bundle);
                return Err(e).context("не удалось установить новую версию");
            }
            let _ = std::fs::remove_dir_all(&old);
            let _ = std::fs::remove_dir_all(&staging);
            if relaunch {
                Command::new("/usr/bin/open")
                    .arg("-n")
                    .arg(&bundle)
                    .spawn()?;
            }
        }
        None => {
            // Запущен «голый» бинарник (не из .app) — меняем только его.
            std::fs::rename(new_app.join("Contents/MacOS/tarn"), &exe)?;
            let _ = std::fs::remove_dir_all(&staging);
            if relaunch {
                Command::new(&exe).spawn()?;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn install(zip: &Path, relaunch: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let new = exe.with_extension("exe.new");
    {
        let mut archive = zip::ZipArchive::new(std::fs::File::open(zip)?)?;
        let mut entry = archive
            .by_name("tarn.exe")
            .context("в архиве нет tarn.exe")?;
        let mut out =
            std::fs::File::create(&new).context("нет прав на запись рядом с программой")?;
        std::io::copy(&mut entry, &mut out)?;
    }
    // Запущенный exe нельзя перезаписать, но можно переименовать.
    let old = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).context("не удалось заменить программу")?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(e).context("не удалось установить новую версию");
    }
    if relaunch {
        std::process::Command::new(&exe).spawn()?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn install(_zip: &Path, _relaunch: bool) -> anyhow::Result<()> {
    bail!("автообновление не поддерживается на этой платформе")
}

/// `tarn --self-update`: проверить, скачать и установить без окна.
pub fn run_cli() -> i32 {
    let result = (|| -> anyhow::Result<String> {
        let Some(release) = check()? else {
            return Ok(format!(
                "Установлена последняя версия {}",
                current_version()
            ));
        };
        println!("Скачиваю {} …", release.version);
        let dir = std::env::temp_dir().join(format!("tarn-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let zip = download(&release, &dir, &|_, _| {})?;
        let r = install(&zip, false);
        let _ = std::fs::remove_dir_all(&dir);
        r?;
        Ok(format!(
            "Обновлено: {} → {}",
            current_version(),
            release.version
        ))
    })();
    match result {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("Ошибка обновления: {e:#}");
            1
        }
    }
}

// ── Состояние для UI ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading {
        done: u64,
        total: u64,
    },
    Installing,
    /// Новая версия запущена — текущий процесс нужно закрыть.
    Restart,
    Error(String),
}

#[derive(Clone)]
pub struct Updater {
    status: Arc<Mutex<Status>>,
    ctx: egui::Context,
}

impl Updater {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            status: Arc::new(Mutex::new(Status::Idle)),
            ctx,
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn set(&self, s: Status) {
        *self.status.lock().unwrap() = s;
        self.ctx.request_repaint();
    }

    pub fn busy(&self) -> bool {
        matches!(
            self.status(),
            Status::Checking | Status::Downloading { .. } | Status::Installing | Status::Restart
        )
    }

    pub fn check(&self) {
        if self.busy() {
            return;
        }
        self.set(Status::Checking);
        let this = self.clone();
        std::thread::spawn(move || {
            let s = match check() {
                Ok(Some(r)) => Status::Available(r),
                Ok(None) => Status::UpToDate,
                Err(e) => Status::Error(format!("{e:#}")),
            };
            this.set(s);
        });
    }

    pub fn install(&self, release: Release) {
        if self.busy() {
            return;
        }
        self.set(Status::Downloading {
            done: 0,
            total: release.asset_size,
        });
        let this = self.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let dir = std::env::temp_dir().join(format!("tarn-update-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir)?;
                let progress = |done, total| {
                    *this.status.lock().unwrap() = Status::Downloading { done, total };
                    this.ctx.request_repaint_after(Duration::from_millis(100));
                };
                let zip = download(&release, &dir, &progress)?;
                this.set(Status::Installing);
                let r = install(&zip, true);
                let _ = std::fs::remove_dir_all(&dir);
                r
            })();
            match result {
                Ok(()) => {
                    tracing::info!(version = %release.version, "обновление установлено, перезапуск");
                    this.set(Status::Restart);
                }
                Err(e) => this.set(Status::Error(format!("{e:#}"))),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh(tag: &str, assets: &[&str]) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            body: Some("notes".into()),
            html_url: "https://example/release".into(),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|n| GhAsset {
                    name: (*n).into(),
                    browser_download_url: format!("https://example/{n}"),
                    size: 10,
                })
                .collect(),
        }
    }

    const ZIP: &str = "tarn-windows-x64.zip";

    #[test]
    fn newer_release_is_offered() {
        let cur = semver::Version::new(0, 1, 0);
        let r = parse_release(gh("v0.2.0", &[ZIP, SUMS_NAME]), &cur, ZIP)
            .unwrap()
            .unwrap();
        assert_eq!(r.version, semver::Version::new(0, 2, 0));
        assert_eq!(r.asset_url, format!("https://example/{ZIP}"));
        assert!(r.sums_url.ends_with(SUMS_NAME));
    }

    #[test]
    fn same_or_older_release_is_ignored() {
        let cur = semver::Version::new(0, 2, 0);
        assert!(
            parse_release(gh("v0.2.0", &[ZIP, SUMS_NAME]), &cur, ZIP)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_release(gh("v0.1.9", &[ZIP, SUMS_NAME]), &cur, ZIP)
                .unwrap()
                .is_none()
        );
        let mut pre = gh("v9.0.0", &[ZIP, SUMS_NAME]);
        pre.prerelease = true;
        assert!(parse_release(pre, &cur, ZIP).unwrap().is_none());
    }

    #[test]
    fn missing_assets_are_errors() {
        let cur = semver::Version::new(0, 1, 0);
        assert!(parse_release(gh("v0.2.0", &[SUMS_NAME]), &cur, ZIP).is_err());
        assert!(parse_release(gh("v0.2.0", &[ZIP]), &cur, ZIP).is_err());
        assert!(parse_release(gh("latest", &[ZIP, SUMS_NAME]), &cur, ZIP).is_err());
    }

    #[test]
    fn parses_sha256sums() {
        let h = "a".repeat(64);
        let sums = format!(
            "{h}  tarn-windows-x64.zip\n{}  tarn-macos-universal.zip\n",
            "b".repeat(64)
        );
        assert_eq!(expected_hash(&sums, "tarn-windows-x64.zip"), Some(h));
        assert_eq!(
            expected_hash(&sums, "tarn-macos-universal.zip"),
            Some("b".repeat(64))
        );
        assert_eq!(expected_hash(&sums, "other.zip"), None);
        assert_eq!(
            expected_hash("zz  tarn-windows-x64.zip", "tarn-windows-x64.zip"),
            None
        );
    }

    /// Сетевой тест на настоящем релизе: `cargo test -- --ignored live_`.
    #[test]
    #[ignore]
    fn live_check_and_download() {
        let asset = asset_name().unwrap();
        let gh: GhRelease = agent()
            .get(&format!(
                "https://api.github.com/repos/{REPO}/releases/latest"
            ))
            .call()
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap();
        let r = parse_release(gh, &semver::Version::new(0, 0, 0), asset)
            .unwrap()
            .unwrap();
        let dir = std::env::temp_dir().join("tarn-live-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = download(&r, &dir, &|_, _| {}).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), r.asset_size);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_encoding() {
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
