# AGENTS.md — OmniRouteTray

Context for AI agents working on this repo. Read this before making changes.

## What this is

A macOS-first (Tauri v2) **tray-only** menu-bar app that supervises, monitors, and auto-updates the third-party `omniroute` Node CLI/server. Rust backend (`src-tauri/`), vanilla web popover UI (`index.html`, `src/`). No dock icon; no main window.

## Golden rules

- **Never `kill -9` or re-sign the app while it is running.** Modifying a running `.app`'s code signature wedges the process and can freeze the Dock/menu bar. Always `pkill -TERM -f OmniRouteTray` and wait before signing. To recover a stuck Dock: `killall Dock; killall SystemUIServer`.
- **Never read or write `~/.omniroute/.env`** (a sensitive-file guard blocks it, and it holds secrets). The app resolves the API key in Rust at runtime; you do not need to see it.
- **Do not commit `src-tauri/resources/node/` or `src-tauri/resources/seed/`** — gitignored, fetched at build time via `scripts/fetch-node.sh`.
- **Parse OmniRoute output defensively.** Its CLI prepends an ANSI-colored log line (`\x1b[2m📋 Loaded env…`). The `[` inside the ANSI escape breaks naive `find('[')` JSON extraction — use the validated `extract_json` approach (try each `[`/`{` until one parses).
- Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before finishing; CI enforces both.
- Stop the app before rebuilding the `.app`; relaunch after.

## Build / test / run

```sh
npm install
bash scripts/fetch-node.sh                    # bundle Node 26.x (checksum-verified)
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri build -- --bundles app          # build .app
open src-tauri/target/release/bundle/macos/OmniRouteTray.app
```

Toolchain: cargo in `~/.cargo/bin` (add to PATH). Node 26.4.0 bundled; system Node is unrelated.

## Architecture (Rust modules in src-tauri/src/)

- `runtime.rs` — app-owned OmniRoute prefix: `versions/<v>/`, `current` symlink (atomic swap), `.install-complete` marker, discard-incomplete, rollback.
- `engine_gate.rs` — blocks OmniRoute versions whose `engines.node` the bundled Node can't satisfy (real value: `>=22 <23 || >=24 <27`). Node's `||` OR-ranges are NOT valid semver `VersionReq`; split on `||` and match any.
- `registry.rs` — npm registry: latest version + `engines.node`.
- `installer.rs` — first-run `npm install` into prefix; `repair_runtime` runs `omniroute runtime repair` (fixes native `.node` ABI bindings).
- `supervisor.rs` — spawns `omniroute serve --no-recovery --no-tray --no-open` in its **own process group**; `stop()` kills the whole group (SIGTERM→SIGKILL) to avoid orphaned server workers. Adopt/reconcile/spawn decisions via pure `decide()`. Singleton via `lockfile.rs` (PID+port+token).
- `state.rs` — `ServerState` enum (Stopped/Starting/Running/UpdateAvailable/Updating/Error).
- `data.rs` — CLI-based quota (`usage quota`) + cost (`cost --group-by model`). Dedupes quota rows.
- `ratelimits.rs` — Claude Session/Weekly via HTTP (see data sources below).
- `apikey.rs` — resolves OmniRoute API key: Keychain → `.env` → **shared `storage.sqlite`** (read-only `rusqlite`), then caches in Keychain.
- `updater.rs` — `is_newer` + staged install/atomic-swap/rollback.
- `doctor.rs` — node/prefix/entry/version health checks.
- `logfile.rs` — rotating capture (5 MB) of server stdout/stderr.
- `paths.rs` — resolves bundled node, app data dir, `~/.omniroute/{.env,storage.sqlite}`.
- `lib.rs` — Tauri setup, tray + menu, popover toggle, bootstrap thread, commands.

## Verified OmniRoute data sources (as of v3.8.44)

- Server: Node CLI + Next.js HTTP API on **`127.0.0.1:20128`**. Config/data in `~/.omniroute/` (`.env`, `storage.sqlite`).
- **API auth**: Bearer key. With `requireLogin` on, management endpoints also need a login session; with it off, the Bearer key suffices. The active key lives in `storage.sqlite` table `api_keys(key, is_active, revoked_at, last_used_at)` — the app reads it directly (DB is shared with the server).
- **Provider quota** (offline, no key): `omniroute usage quota --output json` → `[{provider, limit, used, remaining, resetAt, state}]`, has DUPLICATE rows to dedupe. Limits are often `null`.
- **Cost** (needs key): `omniroute cost --period 30d --group-by model --output json` → `[{group, requests, tokensIn, tokensOut, costUsd, costPct}]`. (NOT `model/cost/tokens`.)
- **Claude Session/Weekly** (needs key): `GET /api/providers` → connections with `isActive` (NOT `enabled`) + `id`/`provider`/`name`; then `GET /api/usage/{connectionId}` → `{quotas: {"session (5h)": {used,total,remaining,resetAt,remainingPercentage,unlimited}, "weekly (7d)": {...}}}`. Filter out per-model windows (gemini/gpt/claude-*/sonnet/opus/haiku) — keep only session/weekly/window_*.
- `GET /api/rate-limits` is queue/concurrency status, NOT session/weekly quota (common misdirection).

## Known environment quirks

- The developer's **global** `omniroute` (`~/.bun/bin/omniroute`) has broken/ABI-mismatched `better-sqlite3` bindings. `omniroute runtime repair` fixes it but may not persist across the global install's process. The app's own clean `npm install` does not have this problem.
- Live/integration tests are `#[ignore]`-gated behind env vars (see `supervisor.rs`), run with `-- --ignored`.

## Signing & release

- `entitlements.plist` (allow-jit, allow-unsigned-executable-memory, disable-library-validation) applies to the bundled Node (hardened runtime) so native addons load.
- Default distribution: **ad-hoc signed** (free) + Homebrew Cask (`Casks/omniroute-tray.rb`, strips quarantine). Developer ID + notarization kick in automatically in `.github/workflows/release.yml` when Apple secrets are present.
- Bundle id: `dev.omniroute.tray`. App data: `~/Library/Application Support/dev.omniroute.tray/`.

## Design decisions (locked)

Bundle Node (no seed — full seed was 2.4 GB); share existing `~/.omniroute/` data; app fully owns the server lifecycle (disable OmniRoute's own tray/recovery); autostart via `tauri-plugin-autostart` (LaunchAgent); single-instance enforced. Full rationale in `.sisyphus/plans/omniroute-tray-architecture.md`.
