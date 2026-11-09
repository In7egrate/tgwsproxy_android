# WS Proxy

Локальный MTProto-прокси для Telegram на Android. WS Proxy запускает прокси на устройстве и передаёт соединения к дата-центрам Telegram через WebSocket over TLS. В зависимости от настроек можно использовать прямой маршрут или маршрут с Cloudflare.

[🇷🇺 Русский](README.md) · [🇬🇧 English](README.en.md)

[![Android 7.0+](https://img.shields.io/badge/Android-7.0%2B-3DDC84?logo=android&logoColor=white)](https://developer.android.com/about/versions/nougat)
[![Version](https://img.shields.io/badge/version-1.0.0-111111)](https://github.com/In7egrate/tgwsproxy_android/releases)
[![Kotlin](https://img.shields.io/badge/Kotlin-Jetpack%20Compose-7F52FF?logo=kotlin&logoColor=white)](https://kotlinlang.org/)
[![Rust](https://img.shields.io/badge/native-Rust-000000?logo=rust&logoColor=white)](https://www.rust-lang.org/)

- **Релизы:** [GitHub Releases](https://github.com/In7egrate/tgwsproxy_android/releases)
- **Исходный проект:** [Flowseal/tg-ws-proxy](https://github.com/Flowseal/tg-ws-proxy)
- **Сообщить об ошибке:** [Issues](https://github.com/In7egrate/tgwsproxy_android/issues)

## О проекте

WS Proxy — Android-версия локального MTProto-прокси на основе проекта [TG WS Proxy от Flowseal](https://github.com/Flowseal/tg-ws-proxy).

Приложение поднимает MTProto endpoint на Android-устройстве. Telegram подключается к локальному адресу, а Rust/Tokio core передаёт трафик дальше по WebSocket-соединению с TLS. Для маршрута доступны прямой режим и Cloudflare fallback; при необходимости можно настроить собственные Cloudflare Worker-домены.

## Возможности

- **Локальный MTProto-прокси** — стандартный порт `1443`, который можно изменить в настройках.
- **Rust/Tokio native core** — обработка сетевых соединений вынесена в нативное ядро.
- **WebSocket over TLS** — транспорт до дата-центров Telegram.
- **Прямой маршрут и Cloudflare fallback** — выбор режима в зависимости от сети.
- **Пул WebSocket-соединений** — настройка размера пула.
- **Настройка дата-центров** — адреса DC и media DC, секрет прокси и дополнительные параметры.
- **Интеграция с Telegram** — передача подключения через ссылку `tg://proxy` кнопкой «Применить в Telegram».
- **Фоновая работа** — foreground service с системным уведомлением, плитка быстрых настроек Android и автозапуск после перезагрузки.
- **Диагностика** — журнал событий и экспорт технического отчёта из приложения.
- **Обновления через GitHub** — проверка Releases при запуске и вручную из настроек.

## Как это работает

```text
Telegram для Android
        │
        ▼
Локальный MTProto-прокси
127.0.0.1:1443
        │
        ▼
Rust / Tokio native core
        │
        ├── WebSocket over TLS ──► Telegram DC
        │
        └── Cloudflare route ────► Telegram DC
             (если включён)
```

Прокси работает локально на телефоне. Отдельный VPS для обычного сценария не требуется. Фактический маршрут зависит от настроек и доступности сетевых направлений.

## Быстрый старт

1. Откройте [страницу Releases](https://github.com/In7egrate/tgwsproxy_android/releases).
2. Скачайте APK, подходящий для архитектуры устройства:
   - `app-arm64-v8a-release.apk` — для 64-битных ARM-устройств;
   - `app-armeabi-v7a-release.apk` — для 32-битных ARM-устройств;
   - `app-universal-release.apk` — универсальный вариант с обеими ARM-библиотеками.
3. Установите APK. Если Android запросит разрешение на установку из выбранного источника, разрешите его для браузера или файлового менеджера, из которого открываете файл.
4. Запустите **WS Proxy** и включите прокси.
5. Нажмите **«Применить в Telegram»**. Если найдено несколько совместимых Telegram-клиентов, выберите нужный и подтвердите подключение в самом Telegram.

> **Важно:** скачивайте APK только со страницы Releases этого репозитория. Приложение не распространяется через Google Play.

Минимальная версия Android — **7.0 (API 24)**.

## Настройки

В приложении доступны:

- режим подключения и Cloudflare fallback;
- Cloudflare Worker и список Worker-доменов;
- локальный порт;
- размер пула WebSocket;
- секрет MTProto;
- адреса обычных и media-дата-центров;
- экспериментальные параметры маршрутизации;
- автозапуск после перезагрузки;
- журнал событий и экспорт диагностики;
- ручная проверка обновлений.

Начните с автоматического профиля. Меняйте адреса DC, размер пула и дополнительные параметры только если это действительно требуется для вашей сети.

## Обновления

WS Proxy проверяет наличие новой версии в GitHub Releases при запуске приложения. Проверку также можно запустить вручную в настройках.

Если обновление доступно, приложение предлагает загрузить APK и передаёт его системному установщику Android. Перед установкой проверяются доступные данные SHA-256 из GitHub Release и соответствие сертификата подписи установленной версии.

**Не меняйте Release Keystore между версиями.** Android не разрешит обновить установленное приложение APK, подписанным другим сертификатом. В таком случае обновление будет заблокировано.

## Если возникла проблема

### Telegram не найден

Кнопка «Применить в Telegram» работает с приложениями, которые зарегистрированы в Android как обработчики ссылки `tg://proxy`. Убедитесь, что установленный Telegram-клиент поддерживает этот тип ссылки.

### Прокси не запускается

- Проверьте, не занят ли выбранный локальный порт другим приложением.
- Попробуйте перезапустить прокси.
- Проверьте системное уведомление WS Proxy: оно показывает состояние фоновой службы.
- Если проблема сохраняется, сформируйте отчёт в разделе диагностики.

### Telegram долго подключается или не загружает медиа

Сравните работу прямого режима и Cloudflare fallback. При ручной конфигурации проверьте адреса дата-центров. Поведение зависит от DNS, маршрута и сети оператора.

### Прокси останавливается в фоне

Разрешите уведомления и автозапуск WS Proxy. На некоторых прошивках Android также требуется убрать приложение из ограничений фоновой активности или оптимизации батареи. Названия этих настроек зависят от производителя устройства.

### Не устанавливается обновление

Если Android сообщает о несовпадении подписи, APK подписан другим сертификатом. Для обновлений используйте сборки, подписанные тем же Release Keystore, что и установленная версия.

Если проблема повторяется, откройте **Настройки → Диагностика → Экспорт диагностики** и приложите сформированный отчёт к [GitHub Issue](https://github.com/In7egrate/tgwsproxy_android/issues). Перед публикацией проверьте отчёт и не добавляйте к Issue пароли, Keystore или другие секреты.

## Сборка из исходников

### Требования

- JDK 17;
- Android SDK, Android Platform 35 и Build Tools 35.0.0;
- Android NDK `27.2.12479018`;
- Rustup и Cargo;
- Rust targets `aarch64-linux-android` и `armv7-linux-androideabi`.

### Windows / PowerShell

Из корня репозитория:

```powershell
Set-ExecutionPolicy -Scope Process Bypass -Force
.\build_v3.ps1 -Project (Get-Location).Path
```

По умолчанию скрипт собирает **Release**. Для Debug:

```powershell
.\build_v3.ps1 -Project (Get-Location).Path -BuildType Debug
```

Release-сборка требует локальный `android/keystore.properties` и соответствующий PKCS#12 Keystore. Создать новый ключ можно скриптом `android/create-release-keystore.ps1`. Файлы ключа и пароли нельзя добавлять в Git.

Если в корне отсутствует маркер `.tgws_v3_ready`, `build_v3.ps1` запускает однократный шаг миграции через `upgrade_v3.py`; для этого шага требуется Python 3.11. В обычной сборке подготовленного дерева этот шаг пропускается.

Готовые APK появляются в:

```text
android/app/build/outputs/apk/release/
```

## GitHub Releases

Workflow `.github/workflows/android-release.yml` запускается при отправке тега, совпадающего с `versionName`, например `v1.0.0`. Он собирает Rust-библиотеки для ARM64 и ARMv7, выполняет `assembleRelease`, проверяет APK и публикует их в GitHub Releases вместе с `SHA256SUMS.txt`.

Для подписанной CI-сборки в **Settings → Secrets and variables → Actions** должны быть заданы:

| Secret | Содержимое |
|---|---|
| `ANDROID_RELEASE_KEYSTORE_BASE64` | Base64 содержимое PKCS#12 Keystore |
| `ANDROID_RELEASE_STORE_PASSWORD` | Пароль хранилища |
| `ANDROID_RELEASE_KEY_ALIAS` | Alias ключа |
| `ANDROID_RELEASE_KEY_PASSWORD` | Пароль ключа |

Не помещайте Keystore и пароли в репозиторий, Issues или публичные сообщения.

## Благодарности и лицензии

- [Flowseal/tg-ws-proxy](https://github.com/Flowseal/tg-ws-proxy) — исходный проект и реализация TG WS Proxy.
- [amurcanov/tg-ws-proxy-android](https://github.com/amurcanov/tg-ws-proxy-android) — Android-реализация, использованная как один из источников Android-кода.
- [Science Gothic](https://fonts.google.com/specimen/Science+Gothic) — шрифт интерфейса.

WS Proxy — независимый форк и не является официальным приложением Telegram или официальным релизом Flowseal.

Информация о происхождении компонентов, авторских уведомлениях и применимых лицензиях приведена в [`NOTICE.md`](NOTICE.md), [`LICENSE`](LICENSE). Лицензии отдельных заимствованных компонентов необходимо соблюдать независимо друг от друга.
