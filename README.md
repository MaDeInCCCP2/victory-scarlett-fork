# Victory-Scarlett-Fork (`feat/unified-memory-vram`)

[English](#english) | [Русский](#русский)

---

<a id="english"></a>
## English

An optimized fork of **Victory-Scarlett** (`Op-lite/victory-scarlett`) focused on NVIDIA 12 GB VRAM stability, low-core CPU offloads, full-color rendering fixes, and **NVIDIA DLSS 4.5 + Frame Generation** support.

> **Asset Policy:** This repository contains **no game files, code, shaders, or proprietary binaries**. You must supply your own dump folder containing `game_scarlett_master.exe` (and the official NVIDIA Streamline SDK x64 DLLs if you enable DLSS / Frame Generation).

### Key Improvements

- **NVIDIA 1M+ Descriptor Heap Bypass (`VICTORY_SLOT_REMAP=1`, 3 banks):** Virtual-to-physical descriptor remapping via Root SRV `t249` (`SLOT_REMAP`) with grace-period reclamation and clock eviction eliminates NVIDIA's 1,000,000 shader-visible view heap cap (`0` descriptor cap errors across 1.24M+ virtual title slots and 786k dynamic slots).
- **VRAM Residency & RAM Offloading:**
  - Live DXGI VRAM budget tracking (`residency.rs`), fence-aware reclamation (`VICTORY_FENCE_RECLAIM=1`), and lazy 16 MiB `xmem` chunks (`VICTORY_XMEM_CHUNKS=1`).
  - `CardStaging::CpuMem` (`VirtualAlloc`) + idle `DiscardVirtualMemory` trimmer (`VICTORY_CARD_TRIM_CPU=1`), `VICTORY_DROP_DXIL=1`, `VICTORY_UNMAP_EXACT=1`, `VICTORY_WINDOW_UNMAP=1`, and DirectStorage RAM leak fix (`dstorage.rs`).
  - Copy-back reduction via `VICTORY_WC_NO_BACK=1` + `VICTORY_BACK_SKIP_UNREAD=1` (readback sink + demand-settle): cuts GPU→CPU copy-back from ~48.7 MB/frame down to **0.2–0.3 MB/frame**.
- **CPU & Upload Pipeline Offloads:**
  - Dedicated async D3D12 copy queue (`VICTORY_UP_QUEUE=1`) for card-memory uploads.
  - AVX2 128B/iter descriptor differ, AVX2/SSE2 micro-tile texture detiler (`victory-formats`), `PORT_DESCRIPTORS` store hooks, 64-shard `PORT_JOURNAL`, `RwLock` `PORT_GUARD`, parallel shadow refresh (`VICTORY_REFRESH_SPLIT_COPY=1`), `VICTORY_READBACK_POOL=1`, and `VICTORY_BATCH_GUARDS=1`.
- **NVIDIA Rendering & Lighting Fixes:**
  - Branchless IEEE-754 `rndne_f32` and default `VICTORY_NAN_GUARD_STORES=1` in `victory-shader`.
  - `VICTORY_E5_TARGETS=r11g11b10` stand-in for `R9G9B9E5` render targets (fixes skin/wall blotches).
  - Single-bounce indirect diffuse, volumetric air, and specular stand-in (`VICTORY_AMBIENT=0.6`, `VICTORY_AMBIENT_AIR=1.0`, `VICTORY_AMBIENT_SPECULAR=0.8`, `VICTORY_AMBIENT_ONCE=1`) with D3D12 `id=646` UAV-in-SRV validation fix.
  - Safe barrier and cloud defaults (`VICTORY_NATIVE_BARRIERS=0` prevents auto-exposure whiteouts; `VICTORY_AMBIENT_MARCH=0` prevents darkened volumetric clouds).
- **DLSS 4.5 + Frame Generation + Live UI:**
  - Streamline SDK integration (`-Dlss -DlssPreset L -FrameGen`), automatic Frame Gen multiplier readout in the window title bar (`XX.X FPS (frame gen YY FPS)`), and an in-game **`F1`** settings overlay saved to `victory-settings.txt`.

### Measured Results

Tested on **Intel Core i5-11400F (6C/12T), 32 GB RAM, NVIDIA RTX 4070 SUPER 12 GB, Windows 11 (driver 621.40)** in Series X 1440p mode (clean `--release` build, RT off, DLSS 4.5 Quality Preset L + FG 2x):

| Metric | Observed |
|---|---|
| Frame rate (cutscenes & boat gameplay) | **12–17 FPS native → 24–34 FPS presented (FG 2x)** |
| VRAM usage (steady state / post-exit) | **8.8–9.5 / 10.6–11.2 GB** (`0` leaks post-exit) |
| Process RAM (Working Set) | **~15–17 GB** |
| GPU→CPU copy-back | **0.2–0.3 MB/frame** (~6.7 ms) |
| Descriptor cap errors (`SLOT_REMAP`) | **0** |
| Pre-warmed shader cache (`warm --used`) | **3,115 / 3,115** compiled (`0` failed) |

### Quick Start

1. **Prerequisites:** Install [Rust](https://rustup.rs) and Visual Studio C++ Build Tools with the Windows SDK.
2. **Build (`--release` recommended for play, `--profile diag` for profiling):**
   ```powershell
   cargo build --release --bin victory --locked
   # Optional diagnostics build:
   # cargo build --profile diag --bin victory --features diagnostics --locked
   ```
3. **Optional — Pre-warm translated shaders once (recommended):**
   ```powershell
   $env:VICTORY_SLOT_REMAP = '1'
   $env:VICTORY_NAN_GUARD_STORES = '1'
   .\target\release\victory.exe warm --used --jobs 8
   ```
4. **Optional — DLSS 4.5 & Frame Generation:** Download the official **NVIDIA Streamline SDK (x64)** and place `sl.interposer.dll`, `nvngx_dlss.dll`, `nvngx_dlssg.dll`, `sl.reflex.dll` (and companion `sl.*.dll`) into `.\streamline-sdk\bin\x64` (or pass `-StreamlineDir <path>`).
5. **Launch:**
   ```powershell
   # Series X 1440p + DLSS 4.5 Quality (Preset L) + Frame Gen 2x
   .\scripts\play-scarlett.ps1 -Game 'D:\Games\MyDump' -Build release -Console series-x -Res 1440 -Resolution 2560x1440 -Dlss -Quality quality -DlssPreset L -FrameGen

   # Series S 1080p + DLSS 4.5 Quality + Frame Gen 2x (higher FPS / lower VRAM)
   .\scripts\play-scarlett.ps1 -Game 'D:\Games\MyDump' -Build release -Console series-s -Resolution 1920x1080 -Dlss -Quality quality -DlssPreset L -FrameGen

   # Without DLSS / without memory hooks (troubleshooting)
   .\scripts\play-scarlett.ps1 -Game 'D:\Games\MyDump' -Build release
   .\scripts\play-scarlett.ps1 -Game 'D:\Games\MyDump' -Build release -NoHooks
   ```

> **Note:** On the very first launch of a new console mode (Series X or Series S), remaining scene shaders compile in the background and faces may finish baking slightly late; from the second launch onward the pipeline library is warm and faces render cleanly.

### Controls & Hotkeys

- **`F1`** — Open/close the in-game live settings menu (`↑`/`↓` select, `←`/`→` change, `Enter` apply/save to `victory-settings.txt`, `Esc` close; blocks title keyboard while open).
- **`F10`** — Toggle the translucent performance overlay.
- **Gamepad:** Any Xbox-compatible controller works natively.
- **Keyboard stand-in pad:** `W A S D` move, `I J K L` camera, `Enter` = A, `Backspace` = B, `X` / `Y`, `Z` / `C` = LT / RT, `Q` / `E` = LB / RB, Arrow keys = D-pad, `Esc` = Menu, `Tab` = View.

### Credits

Built on top of **Op-lite/victory-scarlett** (`scarlett-fox` / `cool_fox`), incorporating and adapting techniques from the Victory community (`VIctory-NVIDIA-Hybrid`, `VIctory-NVIDIA` / `foxhound`, `victory-cool_fox-willy`, `AnyXboxX`, `VIctory-DLSS-GDeflate`).

---

<a id="русский"></a>
## Русский

Оптимизированный форк **Victory-Scarlett** (`Op-lite/victory-scarlett`) с фокусом на стабильность на видеокартах NVIDIA с 12 ГБ VRAM, разгрузку CPU, исправление полноцветного рендеринга и поддержку **NVIDIA DLSS 4.5 + Frame Generation**.

> **Политика ассетов:** Репозиторий **не содержит файлов игры, кода, шейдеров или проприетарных библиотек**. Пользователь предоставляет собственную папку с дампом (`game_scarlett_master.exe`), а для DLSS / Frame Generation — официальные DLL из NVIDIA Streamline SDK x64.

### Ключевые улучшения

- **Обход лимита NVIDIA в 1 млн дескрипторов (`VICTORY_SLOT_REMAP=1`, 3 банка):** виртуально-физическая переадресация дескрипторов через Root SRV `t249` (`SLOT_REMAP`) с отложенным освобождением и clock-вытеснением полностью снимает упор в 1M-лимит NVIDIA (`0` ошибок лимита на 1.24M+ виртуальных title-слотов и 786k динамических слотов).
- **Управление VRAM и разгрузка RAM:**
  - Живой контроль бюджета VRAM через DXGI (`residency.rs`), освобождение по fence (`VICTORY_FENCE_RECLAIM=1`) и ленивые 16 MiB чанки `xmem` (`VICTORY_XMEM_CHUNKS=1`).
  - `CardStaging::CpuMem` (`VirtualAlloc`) + сброс простаивающих буферов через `DiscardVirtualMemory` (`VICTORY_CARD_TRIM_CPU=1`), `VICTORY_DROP_DXIL=1`, `VICTORY_UNMAP_EXACT=1`, `VICTORY_WINDOW_UNMAP=1` и фикс утечки RAM в DirectStorage (`dstorage.rs`).
  - Снижение GPU→CPU copy-back через `VICTORY_WC_NO_BACK=1` + `VICTORY_BACK_SKIP_UNREAD=1` (readback-перехват + ленивая догрузка по требованию): с ~48.7 МБ/кадр до **0.2–0.3 МБ/кадр**.
- **Разгрузка CPU и конвейера загрузок:**
  - Выделенная асинхронная D3D12 Copy Queue (`VICTORY_UP_QUEUE=1`) для загрузок буферов в память видеокарты.
  - AVX2-компаратор дескрипторов (128 байт/итерация), AVX2/SSE2 детайлер текстур (`victory-formats`), хуки записи дескрипторов `PORT_DESCRIPTORS`, 64-шардовый `PORT_JOURNAL`, `RwLock` `PORT_GUARD`, параллельный shadow refresh (`VICTORY_REFRESH_SPLIT_COPY=1`), `VICTORY_READBACK_POOL=1` и `VICTORY_BATCH_GUARDS=1`.
- **Графические и NVIDIA-фиксы:**
  - Безветвенный IEEE-754 `rndne_f32` и `VICTORY_NAN_GUARD_STORES=1` по умолчанию в `victory-shader`.
  - Подмена `R9G9B9E5` рендер-таргетов на `R11G11B10_FLOAT` (`VICTORY_E5_TARGETS=r11g11b10` — убирает пятна и артефакты на коже и стенах).
  - Одноотскоковый stand-in непрямого освещения, воздуха и отражений (`VICTORY_AMBIENT=0.6`, `VICTORY_AMBIENT_AIR=1.0`, `VICTORY_AMBIENT_SPECULAR=0.8`, `VICTORY_AMBIENT_ONCE=1`) с фиксом ошибки D3D12 `id=646` (UAV в SRV-слоте).
  - Безопасные настройки барьеров и облаков (`VICTORY_NATIVE_BARRIERS=0` исключает белые экраны автоэкспозиции; `VICTORY_AMBIENT_MARCH=0` исключает почернение объёмных облаков).
- **DLSS 4.5 + Frame Generation + встроенное меню:**
  - Интеграция Streamline SDK (`-Dlss -DlssPreset L -FrameGen`), автоматическое отображение итогового FPS с генерацией кадров в заголовке окна (`XX.X FPS (frame gen YY FPS)`) и живое внутриигровое меню по **`F1`** с сохранением в `victory-settings.txt`.

### Измеренные результаты

Тестовый стенд: **Intel Core i5-11400F (6C/12T), 32 ГБ RAM, NVIDIA RTX 4070 SUPER 12 ГБ, Windows 11 (драйвер 621.40)**, режим Series X 1440p (чистый `--release`, RT выкл., DLSS 4.5 Quality Preset L + FG 2x):

| Метрика | Результат |
|---|---|
| Частота кадров (катсцены и геймплей на лодке) | **12–17 FPS нативно → 24–34 FPS с FG 2x** |
| Потребление VRAM (в работе / после выхода) | **8.8–9.5 / 10.6–11.2 ГБ** (`0` утечек после выхода) |
| Потребление RAM процессом (Working Set) | **~15–17 ГБ** |
| GPU→CPU copy-back | **0.2–0.3 МБ/кадр** (~6.7 мс) |
| Ошибки лимита дескрипторов (`SLOT_REMAP`) | **0** |
| Прогрев кэша шейдеров (`warm --used`) | **3,115 / 3,115** скомпилировано (`0` ошибок) |

### Быстрый старт

1. **Инструменты:** установите [Rust](https://rustup.rs) и Visual Studio C++ Build Tools с Windows SDK.
2. **Сборка (для игры используйте `--release`, для профилирования — `--profile diag`):**
   ```powershell
   cargo build --release --bin victory --locked
   # Опциональная диагностическая сборка:
   # cargo build --profile diag --bin victory --features diagnostics --locked
   ```
3. **Опционально — разовый прогрев кэша шейдеров (рекомендуется):**
   ```powershell
   $env:VICTORY_SLOT_REMAP = '1'
   $env:VICTORY_NAN_GUARD_STORES = '1'
   .\target\release\victory.exe warm --used --jobs 8
   ```
4. **Опционально — DLSS 4.5 и Frame Generation:** скачайте официальный **NVIDIA Streamline SDK (x64)** и положите `sl.interposer.dll`, `nvngx_dlss.dll`, `nvngx_dlssg.dll`, `sl.reflex.dll` (и остальные `sl.*.dll`) в `.\streamline-sdk\bin\x64` (либо передайте путь через `-StreamlineDir`).
5. **Запуск:**
   ```powershell
   # Series X 1440p + DLSS 4.5 Quality (Preset L) + Frame Gen 2x
   .\scripts\play-scarlett.ps1 -Game 'D:\Games\MyDump' -Build release -Console series-x -Res 1440 -Resolution 2560x1440 -Dlss -Quality quality -DlssPreset L -FrameGen

   # Series S 1080p + DLSS 4.5 Quality + Frame Gen 2x (выше FPS / меньше VRAM)
   .\scripts\play-scarlett.ps1 -Game 'D:\Games\MyDump' -Build release -Console series-s -Resolution 1920x1080 -Dlss -Quality quality -DlssPreset L -FrameGen

   # Без DLSS / без хуков памяти (для диагностики)
   .\scripts\play-scarlett.ps1 -Game 'D:\Games\MyDump' -Build release
   .\scripts\play-scarlett.ps1 -Game 'D:\Games\MyDump' -Build release -NoHooks
   ```

> **Примечание:** при самом первом запуске нового режима консоли (Series X или Series S) оставшиеся пайплайны докомпилируются в фоне, из-за чего запекание текстур лиц может не успевать; со второго запуска кэш уже прогрет и лица рендерятся чисто.

### Управление и горячие клавиши

- **`F1`** — открыть/закрыть внутриигровое меню настроек (`↑`/`↓` выбор, `←`/`→` изменение, `Enter` применить/сохранить в `victory-settings.txt`, `Esc` закрыть; при открытом меню ввод клавиатуры в игру блокируется).
- **`F10`** — показать/скрыть полупрозрачный оверлей производительности.
- **Геймпад:** любой Xbox-совместимый контроллер работает напрямую.
- **Клавиатура (эмуляция геймпада):** `W A S D` движение, `I J K L` камера, `Enter` = A, `Backspace` = B, `X` / `Y`, `Z` / `C` = LT / RT, `Q` / `E` = LB / RB, стрелки = крестовина, `Esc` = Menu, `Tab` = View.

### Благодарности

Основано на **Op-lite/victory-scarlett** (`scarlett-fox` / `cool_fox`) с интеграцией и адаптацией наработок сообщества Victory (`VIctory-NVIDIA-Hybrid`, `VIctory-NVIDIA` / `foxhound`, `victory-cool_fox-willy`, `AnyXboxX`, `VIctory-DLSS-GDeflate`).
