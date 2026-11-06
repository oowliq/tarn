#!/usr/bin/env bash
# Собирает dist/tarn.app (universal: Apple Silicon + Intel) и dist/tarn-macos-universal.zip.
# Использование: packaging/macos/build-app.sh [версия]
set -euo pipefail

cd "$(dirname "$0")/../.."
VERSION="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
VERSION="${VERSION#v}"
export TARN_VERSION="${VERSION}"

rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
cargo build --release --locked --target aarch64-apple-darwin
cargo build --release --locked --target x86_64-apple-darwin

APP=dist/tarn.app
rm -rf dist
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"
lipo -create -output "${APP}/Contents/MacOS/tarn" \
    target/aarch64-apple-darwin/release/tarn \
    target/x86_64-apple-darwin/release/tarn
sed "s/__VERSION__/${VERSION}/g" packaging/macos/Info.plist > "${APP}/Contents/Info.plist"

# Подпись. С постоянным сертификатом (TARN_SIGN_IDENTITY — имя или SHA-1
# сертификата, см. packaging/macos/README.md) macOS узнаёт Tarn после
# обновлений: не спрашивает заново пароль связки ключей и не сбрасывает
# «Универсальный доступ». Без него — ad-hoc: запускается, но для macOS каждая
# сборка — новое приложение. TARN_SIGN_KEYCHAIN — связка ключей с сертификатом,
# если она не в списке поиска (CI).
if [[ -n "${TARN_SIGN_IDENTITY:-}" ]]; then
    codesign --force --deep --timestamp=none --sign "${TARN_SIGN_IDENTITY}" \
        ${TARN_SIGN_KEYCHAIN:+--keychain "${TARN_SIGN_KEYCHAIN}"} "${APP}"
else
    echo "TARN_SIGN_IDENTITY не задан — подпись ad-hoc" >&2
    codesign --force --deep --sign - "${APP}"
fi
codesign --verify --deep --strict "${APP}"
codesign --display --requirements - "${APP}" 2>&1 | sed -n 's/^designated => /Требование подписи: /p'

ditto -c -k --keepParent "${APP}" dist/tarn-macos-universal.zip
echo "Готово: ${APP}, dist/tarn-macos-universal.zip (версия ${VERSION})"
