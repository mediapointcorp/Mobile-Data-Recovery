# Building on Windows

| Tool | Version |
|---|---|
| Windows | 10 22H2 or 11, x64 |
| Visual Studio 2022 Build Tools | "Desktop development with C++" |
| Rust | stable ≥ 1.88, `x86_64-pc-windows-msvc` |
| Node.js | 22 LTS |
| Electron / TypeScript / electron-builder | 44.x / 5.9.x / 26.x (pinned in `app/package-lock.json`) |

## 1. Prerequisites

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1
```

The script uses `winget` and asks before installing each prerequisite.

## 2. Build

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1              # build + tests
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer   # also the NSIS installer
```

Outputs:

- `target\release\photorecovery-worker.exe`: the engine and command-line tool.
- `app\release\PhotoRecovery Setup 0.1.0.exe`: the installer. It bundles the engine and requests administrator rights when the app starts.

## 3. Run during development

Open **PowerShell as administrator**, then run:

```powershell
cd app
npm start
```

## Notes

- **Signing:** builds are unsigned, so SmartScreen will warn. Sign them before distributing: set `CSC_LINK` and `CSC_KEY_PASSWORD` for electron-builder.
- **CI:** `.github/workflows/ci.yml` builds and tests on `windows-latest` and uploads the installer as an artifact.
