// В релизной сборке под Windows не показываем консольное окно.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod agent;
mod app;
mod config;
mod cursor;
mod display;
mod gfx;
mod input;
mod keepalive;
mod keyboard;
mod secrets;
mod session;
mod update;

fn main() -> eframe::Result {
    // ureq собран без своего провайдера и берёт этот же, глобальный.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    if std::env::args().any(|a| a == "--self-update") {
        std::process::exit(update::run_cli());
    }
    init_logging();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let handle = rt.handle().clone();

    update::cleanup_previous();
    tracing::info!(
        version = %update::current_version(),
        debug_build = cfg!(debug_assertions),
        "tarn"
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tarn")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([480.0, 320.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "tarn",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(handle, cc)))),
    );
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}

/// Лог в консоль и в файл текущего запуска (перезаписывается при старте).
fn init_logging() {
    use tracing_subscriber::fmt::writer::MakeWriterExt as _;
    let file = config::log_path().and_then(|path| {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::File::create(path).ok()
    });
    let builder = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false);
    match file {
        Some(f) => builder
            .with_writer(std::io::stderr.and(std::sync::Mutex::new(f)))
            .init(),
        None => builder.with_writer(std::io::stderr).init(),
    }
}

#[cfg(test)]
mod tests {
    /// ironrdp строит `ClientConfig::builder()` без явного провайдера: если в
    /// rustls включены сразу ring и aws-lc-rs, это паникует и сессия рвётся.
    #[test]
    fn rustls_has_single_crypto_provider() {
        let _ = rustls::ClientConfig::builder();
    }
}
