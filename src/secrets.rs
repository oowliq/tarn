//! Пароли в системном хранилище: Keychain (macOS) / Credential Manager (Windows).

const SERVICE: &str = "tarn";

fn entry(profile_id: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, profile_id)
}

pub fn load(profile_id: &str) -> Option<String> {
    entry(profile_id).and_then(|e| e.get_password()).ok()
}

pub fn save(profile_id: &str, password: &str) -> anyhow::Result<()> {
    entry(profile_id)?.set_password(password)?;
    Ok(())
}

pub fn delete(profile_id: &str) {
    if let Ok(e) = entry(profile_id) {
        let _ = e.delete_credential();
    }
}
