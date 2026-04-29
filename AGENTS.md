# Repository Guidelines

## Project Structure & Module Organization

This repository is a Rust workspace. `upmc/` contains the main Windows updater GUI; its source lives in `upmc/src/`, with embedded files under `upmc/assets/` and build metadata in `upmc/build.rs`. `discord-voice-proxy/` provides the Discord proxy install/uninstall library. `discord-proxy-dll/` builds `dwrite.dll`, and `force-proxy-dll/` builds `force_proxy.dll`; both are release artifacts consumed by the updater. `docs/` holds design and hardening notes, `scripts/` contains release and signing helpers, and `.github/` contains CI workflows. Tests are mostly inline Rust `#[cfg(test)]` modules next to the code they cover.

## Build, Test, and Development Commands

Run commands from the repository root:

```powershell
cargo test -p upmc
cargo test -p discord-voice-proxy
cargo build --release -p discord-proxy-dll -p force-proxy-dll
cargo build --release -p upmc
cargo fmt
```

Use the DLL release build before building `upmc` when touching embedded proxy DLL behavior. Use `cargo test -p <crate>` for focused validation, and run the relevant upstream crate tests before opening a PR.

## Coding Style & Naming Conventions

Use standard Rust formatting with 4-space indentation. Follow Rust naming conventions: `snake_case` for modules, functions, and variables; `PascalCase` for types and traits; `SCREAMING_SNAKE_CASE` for constants. Prefer the repository's existing helpers and error style, including `anyhow::{Context, Result}` where applicable. Keep Windows subprocesses hidden with `CREATE_NO_WINDOW` when the command is not intentionally user-facing.

## Testing Guidelines

Add focused unit tests beside changed logic in `#[cfg(test)]` modules. Test names should describe behavior, such as `validate_download_rejects_http_url` or `parse_version_accepts_full_schema`. Cover parsing, validation, retry, path handling, and security-sensitive branches when modified. If release packaging, embedded DLLs, or `server.json` compatibility changes, include a release build in validation.

## Commit & Pull Request Guidelines

Use short imperative commit titles matching the existing history, for example `Balance secondary action buttons` or `Add SHA256 fields to Downloads schema`. PRs should summarize the behavior change, list validation commands, link related issues, and include screenshots for UI changes. Call out security, configuration, or release artifact implications explicitly.

## Security & Configuration Tips

Keep download URLs HTTPS-only and preserve SHA256 validation for bootstrap downloads. Do not weaken trusted host or suffix checks. Self-update helpers use unique `upmc-update-helper-*.exe` names, with `upmc-update-helper.exe` retained only for legacy compatibility. Be careful with zip extraction, filesystem paths, and process execution. Avoid reintroducing PowerShell-based self-update chains unless the security model is documented and reviewed.
