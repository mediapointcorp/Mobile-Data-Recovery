# Windows installation and build

Tested toolchain versions (pinned through `Cargo.lock` and `app/package-lock.json`):

| Tool | Version |
|---|---|
| Windows | 10 22H2 or 11, x64 |
| Rust | stable ≥ 1.88 (developed with 1.97), target `x86_64-pc-windows-msvc` |
| Visual Studio Build Tools | 2022, "Desktop development with C++" workload (MSVC linker + Windows SDK) |
| Node.js | 22 LTS (npm 10) |
| Electron | 44.x |
| TypeScript | 5.9.x |
| electron-builder | 26.x (NSIS installer) |
| Android SDK Platform-Tools | latest from Google (provides `adb.exe`) |

Main Rust dependencies: `image` 0.25 (jpeg/png/webp/gif/bmp decoders), `kamadak-exif` 0.6, `sha2` 0.10, `crc32fast` 1.5, `memchr` 2.7, `serde`/`serde_json` 1, `clap` 4, `uuid` 1, `time` 0.3, `thiserror` 2, `anyhow` 1. Dev/test only: `jpeg-encoder` 0.7, `tempfile` 3.

## 1. Install prerequisites

Automated (uses `winget`; it asks before installing anything):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1
```

Manual:

1. Visual Studio 2022 Build Tools with **Desktop development with C++**.
2. Rust: <https://rustup.rs>, then `rustup default stable-x86_64-pc-windows-msvc`.
3. Node.js 22 LTS: <https://nodejs.org>.
4. Android SDK Platform-Tools: download from <https://developer.android.com/tools/releases/platform-tools>, extract (for example to `C:\Android\platform-tools`) and add that folder to `PATH`. PhotoRecovery also looks in `%LOCALAPPDATA%\Android\Sdk\platform-tools`, `%ANDROID_HOME%` and `%ANDROID_SDK_ROOT%`.
   Platform-tools is **not** bundled because of its license terms. Install it yourself.
5. A USB driver for your phone: the Google USB Driver, or the manufacturer's driver if Windows does not recognise the device in ADB mode. See `USB_ADB_SETUP.md`.

## 2. Build

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # build + all tests
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -SkipTests
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # also builds NSIS installer
```

The script runs:

```powershell
cargo build --release --workspace
cargo test --workspace            # unless -SkipTests
cd app
npm ci
npm run typecheck
npm test
npm run build
npx electron-builder --win nsis --x64   # with -Installer
```

Outputs:

- `target\release\photorecovery-worker.exe`: the engine and CLI
- `target\release\pr-bench.exe`: benchmarks and fixture generator
- `app\release\PhotoRecovery Setup 0.1.0.exe`: per-user installer (with `-Installer`). It bundles the worker at `resources\bin\photorecovery-worker.exe`.

## 3. Run

- Development: `cd app; npm start`. The app finds the worker in `..\target\release` or `..\target\debug`, or at the path in `PHOTORECOVERY_WORKER`.
- Installed: Start menu → PhotoRecovery.

The app runs as a normal user (`asInvoker`). It does not need administrator rights, because ADB and reading image files you own need no elevation.

## Notes

- **Antivirus / SmartScreen:** the unsigned installer and worker may trigger SmartScreen. Sign both with your code-signing certificate before distributing (set `CSC_LINK`/`CSC_KEY_PASSWORD` for electron-builder).
- **Long paths:** device paths can be long. Rust uses extended-length paths automatically for absolute paths. Keep the case folder short anyway (for example `D:\Cases\phone1`).
- **Cross-checking from Linux:** `rustup target add x86_64-pc-windows-gnu && cargo check --workspace --target x86_64-pc-windows-gnu`. This is what CI and development used to type-check the Windows-specific code (`FileExt::seek_read`, `CREATE_NO_WINDOW`, `GetProcessMemoryInfo`).
