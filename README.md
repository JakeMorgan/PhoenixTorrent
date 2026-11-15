# Phoenix Torrent

Быстрый и лёгкий торрент-клиент для Windows: один exe-файл около 13 МБ, без установки.

Движок — [librqbit](https://github.com/ikatson/rqbit) (Rust), окно — [Tauri 2](https://tauri.app), интерфейс — обычные HTML/CSS/JS без сборщиков.

## Скачать

Готовый `PhoenixTorrent.exe` лежит на странице [Releases](https://github.com/JakeMorgan/PhoenixTorrent/releases/latest).
Нужна Windows 10 или 11 (используется встроенный WebView2).

## Возможности

- magnet-ссылки, URL и `.torrent`-файлы: кнопкой, через Ctrl+V или перетаскиванием в окно;
- выбор файлов и папки перед загрузкой, отключение файлов на ходу;
- DHT, uTP, UPnP, возобновление раздач после перезапуска;
- лимиты скорости, папка загрузок по умолчанию;
- сворачивание в трей, ассоциация с `.torrent` и `magnet:` (кнопка в настройках);
- тёмная и светлая тема по настройке системы.

## Сборка

Нужны [Rust](https://rustup.rs) и Visual Studio Build Tools с компонентом C++.

```powershell
powershell -ExecutionPolicy Bypass -File build.ps1
```

Скрипт кладёт `PhoenixTorrent.exe` в корень проекта. Вручную то же самое:

```powershell
cd src-tauri
cargo build --release --features custom-protocol
```

## Выпуск версии

Релиз собирает GitHub Actions при появлении тега `v*`:

```powershell
git tag v0.2.0
git push origin v0.2.0
```

Перед этим поднимите версию в `src-tauri/Cargo.toml` и `src-tauri/tauri.conf.json`.

## Структура

- `src-tauri/src/main.rs` — бэкенд: сессия librqbit, команды для интерфейса, трей, ассоциации;
- `ui/` — интерфейс;
- `.github/workflows/build.yml` — сборка и публикация релизов.

## Лицензия

Apache-2.0, см. [LICENSE](LICENSE).
