# Repository Instructions

- After any file modification in this repository, create a git commit that includes the change.
- Keep generated dependencies and build outputs out of git unless explicitly requested.
- Prefer small, focused commits with clear messages.
- After changes that affect the user-facing app, update the ignored `release/` build so the user can test via `release\Mikan下载助手\Mikan下载助手.exe`.
- When rebuilding `release/`, preserve and restore runtime data such as `release\Mikan下载助手\data` where possible.
- Any user-facing functional or behavioral change must bump the application version before release.
- Follow semantic versioning: patch for fixes, minor for backward-compatible features, and major for incompatible changes.
- Keep the version synchronized in `package.json`, `package-lock.json`, `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`, and `src-tauri/tauri.conf.json`.
- Name release installers as `Mikan下载助手-vX.Y.Z.exe`; do not use `安装程序` or `Setup` in the published filename.
