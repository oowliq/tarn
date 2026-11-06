#!/usr/bin/env bash
# Создаёт постоянный самоподписанный сертификат подписи кода для Tarn (на 10 лет),
# кладёт его в связку ключей login и сохраняет файлы для секретов GitHub.
# Запускать один раз: новый сертификат = для macOS снова «другое приложение».
#
# Использование: packaging/macos/make-signing-cert.sh ["Имя сертификата"] [каталог]
set -euo pipefail

NAME="${1:-Tarn Code Signing}"
OUT="${2:-${HOME}/tarn-signing}"

if security find-certificate -c "${NAME}" >/dev/null 2>&1; then
    echo "Сертификат «${NAME}» уже есть в связке ключей — новый не создаю." >&2
    exit 1
fi

mkdir -p "${OUT}"
chmod 700 "${OUT}"
cd "${OUT}"
PASS="$(/usr/bin/openssl rand -hex 16)"

# Системный openssl (LibreSSL) пишет .p12 в формате, который понимает `security import`.
/usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout key.pem -out cert.pem -subj "/CN=${NAME}" \
    -addext "basicConstraints=critical,CA:false" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning" 2>/dev/null
/usr/bin/openssl pkcs12 -export -inkey key.pem -in cert.pem -name "${NAME}" \
    -out tarn-signing.p12 -passout "pass:${PASS}"
rm -f key.pem

security import tarn-signing.p12 -k "${HOME}/Library/Keychains/login.keychain-db" \
    -P "${PASS}" -T /usr/bin/codesign
printf '%s' "${PASS}" > p12-password.txt
base64 -i tarn-signing.p12 > p12-base64.txt
chmod 600 tarn-signing.p12 p12-password.txt p12-base64.txt

cat <<EOF

Готово: сертификат «${NAME}» в связке ключей login, файлы — в ${OUT}.

Локальная сборка:
  TARN_SIGN_IDENTITY="${NAME}" packaging/macos/build-app.sh

Секреты для GitHub Actions (из корня репозитория):
  gh secret set MACOS_SIGN_P12 < "${OUT}/p12-base64.txt"
  gh secret set MACOS_SIGN_P12_PASSWORD < "${OUT}/p12-password.txt"
  gh secret set MACOS_SIGN_IDENTITY --body "${NAME}"

Сохраните tarn-signing.p12 и пароль (например, в менеджере паролей), затем удалите ${OUT}.
Потеряете сертификат — у всех пользователей после обновления снова будут запросы.
EOF
