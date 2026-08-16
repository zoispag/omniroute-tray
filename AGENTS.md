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
- **`master` is protected + releases are immutable.** All changes land via PR (CI `test` + CodeQL must pass). NEVER re-tag or move a published tag; every change ships as a new version (bump `package.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, run `cargo update -p omniroute-tray --precise <v>`).
- **The app and a user's manual `omniroute` share `~/.omniroute/` (one port :20128, one pid-file).** The supervisor must ADOPT a live server, never spawn over it. The one exception is a server that provably came out of our own prefix (`belongs_to_prefix`) and is running a version other than `current` — that one is ours and gets cycled. Debugging note: running `omniroute stop` or spawning during dev disrupts the user's own server.
- **UI toggles must re-paint from cache, not refetch.** `paintRateLimits()` re-renders instantly from `rateLimitCache`; only the 5s poll calls `get_rate_limits`. Data-fetching Tauri commands are `async` + `spawn_blocking` (sync commands block the webview main thread → frozen popover).

## Build / test / run

```sh
npm install
bash scripts/fetch-node.sh                    # bundle Node 24.x LTS (checksum-verified)
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri build -- --bundles app          # build .app
open src-tauri/target/release/bundle/macos/OmniRouteTray.app
```

Toolchain: cargo in `~/.cargo/bin` (add to PATH). Node 24.18.0 (LTS) bundled; system Node is unrelated. Node 24 is required: omniroute 3.8.45 declares `engines.node >=22 <23 || >=24 <27` but empirically HANGS on all real routes under Node 26 (event loop spins on unresolved async; `/health` 404s fast while `/api/*` and SSR pages never respond). Node 24 responds instantly. Do not bump the bundled Node into the 26 range until upstream omniroute is verified working on it.

## Architecture (Rust modules in src-tauri/src/)

- `runtime.rs` — app-owned OmniRoute prefix: `versions/<v>/`, `current` symlink (atomic swap), `.install-complete` marker, discard-incomplete, rollback.
- `engine_gate.rs` — blocks OmniRoute versions whose `engines.node` the bundled Node can't satisfy (real value: `>=22 <23 || >=24 <27`). Node's `||` OR-ranges are NOT valid semver `VersionReq`; split on `||` and match any.
- `registry.rs` — npm registry: latest version + `engines.node`.
- `installer.rs` — first-run `npm install` into prefix; `repair_runtime` runs `omniroute runtime repair` (fixes native `.node` ABI bindings).
- `supervisor.rs` — ADOPT-first lifecycle. Liveness = HTTP health (`server_present` polls `/api/monitoring/health` with retries), NOT PID (omniroute double-forks a detached server; PID tracking is meaningless). `decide()`: live port → adopt/reconcile-foreign (never spawn over it); nothing alive → spawn. On spawn, prepend bundled node dir to child `PATH` (omniroute's `serve.mjs` does bare `spawn("node")` — without this the grandchild worker uses the user's global node). `wait_ready` polls the health endpoint. Singleton via `lockfile.rs` (PID+port+token).
  - **Ownership (`manages_server`) decides who may be stopped.** A server counts as ours when we spawned it, or when `reconcile` traced the live daemon back to our prefix via `belongs_to_prefix` (argv of the pid → argv of its parent → files it holds open; the daemon rewrites its own title to `omniroute (vX.Y.Z)`, so its own argv proves nothing). Resolved eagerly during `reconcile`, because the quit path runs on the UI thread and must not probe. A server that is NOT ours (the user's own `omniroute serve`) is never stopped, signalled or spawned over — the adopt-first rule still holds.
  - `stop()` (quit, non-blocking) fires `omniroute stop` for a server that is ours, even with no `Child` handle — otherwise a daemon that outlived an earlier tray run is stranded on the port forever (#34). `stop_and_wait()` (background threads only) additionally blocks until the port is genuinely free, escalating `omniroute stop` → SIGTERM → SIGKILL of the daemon's **process group** (which collects the launcher and esbuild helpers too), re-checking ownership before it signals anything.
  - **Version drift is the update bug's fingerprint.** Swapping the `current` symlink does not touch a live process, so `reconcile` compares `expected_version` (what `current` resolves to) against the version `/api/monitoring/health` reports and replaces a stale server of ours (`Reconciliation::ReplaceStale`). `apply_update` restarts the server itself for the same reason.
- `state.rs` — `ServerState` enum (Stopped/Starting/Running/UpdateAvailable/Updating/Error).
- `data.rs` — CLI-based quota (`usage quota`) + cost (`cost --group-by model`). Dedupes quota rows.
- `ratelimits.rs` — Claude Session/Weekly via HTTP (see data sources below). `short_label` derives the window tag (5h/7d/mo/wk) from the real key; UI infers duration-less `session` from the reset horizon.
- `analytics.rs` — usage trend via `GET /api/usage/analytics?period=30d` → `dailyTrend[{date,cost,totalTokens}]` + `summary`. Powers the popover sparkline + Today/Yesterday/30d.
- `health.rs` — status-strip data: aggregates `GET /api/monitoring/health` + `/api/providers` + `/api/telemetry/summary` + `/api/cache/stats` into `{active/configured providers, breakers_open, per-type provider list, p95, cache hit rate}`. Every field independently optional (partial failure still renders); empty-state flags (`cache_active`, `latency_sampled`) hide segments rather than show a misleading `0`. See verified shapes below.
- `github.rs` — tray self-update check via GitHub Releases (`releases/latest` → `tag_name`); needs `User-Agent`, fails soft.
- `apikey.rs` — resolves OmniRoute API key silently: `.env` → **shared `storage.sqlite`** (read-only `rusqlite`, table `api_keys`). **No Keychain by default** — the Keychain path triggered a macOS password prompt on every rebuild (ad-hoc signature changes) and was dropped; Keychain helpers remain `#[allow(dead_code)]` for a future mint case.
- `updater.rs` — `is_newer` + staged install/atomic-swap/rollback.
- `doctor.rs` — node/prefix/entry/version health checks.
- `logfile.rs` — rotating capture (5 MB) of server stdout/stderr.
- `paths.rs` — resolves bundled node, app data dir, `~/.omniroute/{.env,storage.sqlite}`.
- `lib.rs` — Tauri setup, tray + menu, popover toggle, bootstrap thread, commands.
  - **The popover is the app's only window, so window teardown = process exit.** tao emits `RunEvent::ExitRequested { code: None }` once the last window is destroyed, and an unprevented one exits the process — the tray icon just vanishes, with a clean `exit(0)` and NO crash report (diagnosed from `log show`: "Entering exit handler" + `termination reported by launchd (0, 0, …)`). So: `CloseRequested` → `prevent_close()` + hide; `ExitRequested` with `code: None` → `prevent_exit()`; only `code: Some(_)` (i.e. `AppHandle::exit` from the Quit item) tears down. `toggle_popover` rebuilds the window from config if it is ever missing. A system logout/shutdown arrives as `RunEvent::Exit`, never `ExitRequested`, so none of this can block it.
  - **Logging is on in release** (file target, `~/Library/Logs/dev.omniroute.tray/tray.log`); stdout is dev-only. Release builds used to log nothing, which is why the silent exit above had to be reconstructed from the unified log. Data commands (`get_quota`/`get_cost`/`get_rate_limits`/`get_usage_trend`/`get_health`) are `async` + `spawn_blocking`; `get_tray_update` checks GitHub Releases.

## Frontend (src/)

- `main.js` — popover render loop (self-scheduling `setTimeout`, not `setInterval`, to avoid poll pileup). `renderRateLimits()` fetches + caches into `rateLimitCache`; `paintRateLimits()` re-renders instantly (used by the %left/%used toggle and per-account hide, so they don't refetch). Per-account hide persists in `localStorage`.
- `icons.js` — provider brand marks (Claude/OpenAI/Gemini) as inline SVG, extracted from `@lobehub/icons` (MIT) since the app is offline (CSP forbids remote/CDN assets). Regenerate by extracting `d`/`fill`/`viewBox` from `@lobehub/icons/es/{Claude,OpenAI,Gemini}/components/{Color,Mono}.js`.

## Verified OmniRoute data sources (as of v3.8.44)

- Server: Node CLI + Next.js HTTP API on **`127.0.0.1:20128`**. Config/data in `~/.omniroute/` (`.env`, `storage.sqlite`).
- **API auth**: Bearer key. With `requireLogin` on, management endpoints also need a login session; with it off, the Bearer key suffices. The active key lives in `storage.sqlite` table `api_keys(key, is_active, revoked_at, last_used_at)` — the app reads it directly (DB is shared with the server).
- **Provider quota** (offline, no key): `omniroute usage quota --output json` → `[{provider, limit, used, remaining, resetAt, state}]`, has DUPLICATE rows to dedupe. Limits are often `null`.
- **Cost** (needs key): `omniroute cost --period 30d --group-by model --output json` → `[{group, requests, tokensIn, tokensOut, costUsd, costPct}]`. (NOT `model/cost/tokens`.)
- **Claude Session/Weekly** (needs key): `GET /api/providers` → connections with `isActive` (NOT `enabled`) + `id`/`provider`/`name`; then `GET /api/usage/{connectionId}` → `{quotas: {"session (5h)": {used,total,remaining,resetAt,remainingPercentage,unlimited}, "weekly (7d)": {...}}}`. Filter out per-model windows (gemini/gpt/claude-*/sonnet/opus/haiku) — keep only session/weekly/window_*.
- `GET /api/rate-limits` is queue/concurrency status, NOT session/weekly quota (common misdirection).

### Health strip sources (verified live against v3.8.45; `health.rs`)

The docs disagreed with the running instance on several shapes — these are the confirmed field paths:

- **`GET /api/monitoring/health`** (NO auth): `providerSummary.{catalogCount,configuredCount,activeCount,monitoredCount}` (counts distinct provider *types*, not connections); `circuitBreakers.{open,halfOpen,degraded,closed,total}`; per-type `providerHealth.<type>.state` (`CLOSED`/`OPEN`/`HALF_OPEN`, only present for types with traffic). Full body also has `system.*`, `activeConnections`, `rateLimitStatus`.
- **`GET /api/providers`** (needs key): flat array of per-**account** connections `{id, name, provider, isActive}` — NOT one row per provider type. `health.rs` aggregates by `provider` into types (a type is active iff any account `isActive`). On the reference instance 11 accounts collapse to 5 types.
- **`GET /api/telemetry/summary`** (needs key): top-level aggregate `{count, avg, p50, p95, p99, errorRate, totalRequests}` — p95 is **global across providers**, NOT per-provider as the docs imply. `count==0` means no samples yet (hide the latency segment).
- **`GET /api/cache/stats`** (needs key): FLAT `{size, maxSize, bytes, maxBytes, hits, misses, evictions, hitRate}` — NOT nested under `semanticCache` as the docs claim. `hitRate` is 0..1. `hits+misses==0` means cold cache (hide the cache segment).

### Tray self-update source (`github.rs`)

- **`GET https://api.github.com/repos/zoispag/omniroute-tray/releases/latest`** → `{tag_name}` (e.g. `v0.1.8`). REQUIRES a `User-Agent` header (GitHub returns 403 without it). Strip the leading `v`, compare via `updater::is_newer`. Fails soft (no update shown) on network error / rate-limit.

## Known environment quirks

- The developer's **global** `omniroute` (`~/.bun/bin/omniroute`) has broken/ABI-mismatched `better-sqlite3` bindings. `omniroute runtime repair` fixes it but may not persist across the global install's process. The app's own clean `npm install` does not have this problem.
- omniroute's `serve.mjs` launcher **double-forks a detached server** via bare `spawn("node")`, so the real listener is a grandchild with a different PID (resolves `node` from PATH). Never track the launcher PID as "the server"; use the HTTP health endpoint. The PATH fix (bundled node first) makes the grandchild use our node.
- `code=0`/`code=-1` restart churn in the server log almost always means TWO supervisors are fighting over :20128 (the app spawned over the user's already-running server). Fix = adopt, don't spawn.
- Live/integration tests are `#[ignore]`-gated behind env vars (see `supervisor.rs`), run with `-- --ignored`.

## CI / release / distribution

- `.github/workflows/ci.yml` — on PR + push to `master`: `npm ci && npm run build` + `fetch-node.sh` (tauri-build validates `frontendDist` and `resources/node` at build-script time, so both must exist), then `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test`.
- `.github/workflows/codeql.yml` — CodeQL for `javascript-typescript` and `rust`, both `build-mode: none` (Rust does NOT support autobuild), on ubuntu.
- `.github/workflows/release.yml` — on `v*` tag: build → sign → attach DMG to Release (needs `permissions: contents: write`) → mint a **GitHub App token** (scoped to `homebrew-tap`, via `actions/create-github-app-token@v3`; needs `TAP_APP_ID` + `TAP_APP_PRIVATE_KEY` secrets) → render `.github/cask.rb.tmpl` with version+sha256 → push to the tap. Tauri signs during build, so pass `APPLE_SIGNING_IDENTITY` (ad-hoc `-` fallback); secrets can't be used in `if:` so they're hoisted to job `env`.
- `.github/dependabot.yml` — weekly cargo/npm/github-actions updates.
- `entitlements.plist` (allow-jit, allow-unsigned-executable-memory, disable-library-validation) applies to the bundled Node (hardened runtime) so native addons load.
- Default distribution: **ad-hoc signed** (free) + Homebrew Cask (`Casks/omniroute-tray.rb` mirrors `.github/cask.rb.tmpl`; `depends_on macos: :ventura`, strips quarantine). Developer ID + notarization kick in automatically when Apple secrets are present.
- Bundle id: `dev.omniroute.tray`. App data: `~/Library/Application Support/dev.omniroute.tray/`.
- The `glib` Dependabot alert is Linux/GTK-only (transitive via Tauri's GTK stack) and does not affect the macOS-only build — dismissed as not-affected.

## Design decisions (locked)

Bundle Node (no seed — full seed was 2.4 GB); share existing `~/.omniroute/` data; app fully owns the server lifecycle (disable OmniRoute's own tray/recovery); autostart via `tauri-plugin-autostart` (LaunchAgent); single-instance enforced. Full rationale in `.sisyphus/plans/omniroute-tray-architecture.md`.
