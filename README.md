# OmniRouteTray

A cross-platform (macOS-first) menu-bar tray app that supervises, monitors, and auto-updates the [OmniRoute](https://www.npmjs.com/package/omniroute) AI router. It replaces the manual workflow of starting `omniroute serve`, keeping it running across reboots, and updating it by hand.

Built with [Tauri v2](https://v2.tauri.app/) (Rust backend + web popover UI).

## Features

- **Tray-only** — a rich menu-bar popover, no dock icon.
- **Supervised server** — spawns and keeps `omniroute serve` running, adopting an already-running instance instead of spawning a duplicate.
- **Bundled Node runtime** — ships its own signed Node 24.x LTS and manages an app-owned OmniRoute install; does not depend on your global `bun`/`npm`.
- **Live usage** — provider quota bars, Claude Session/Weekly limits with reset countdowns (`% left` / `% used` toggle), and a 30-day cost breakdown.
- **Auto-update** — detects new OmniRoute releases and updates in place (staged install, atomic swap, rollback on failure).
- **Start on login** — optional launch at login.
- **Doctor & logs** — one-click diagnostics and server log access.

## Install

### Homebrew (recommended)

```sh
brew install --cask zoispag/tap/omniroute-tray
```

Homebrew clears the quarantine flag automatically, so the app launches cleanly.

### Manual (.dmg)

Download the latest `.dmg` from [Releases](https://github.com/zoispag/omniroute-tray/releases), open it, and drag the app to Applications.

Because public builds are **ad-hoc signed** (not notarized — that requires a paid Apple Developer account), Gatekeeper blocks the first launch. Clear the quarantine flag once:

```sh
xattr -dr com.apple.quarantine /Applications/OmniRouteTray.app
```

Or right-click the app → **Open** → **Open**.

## How it works

- On first launch the app installs OmniRoute into `~/Library/Application Support/dev.omniroute.tray/omniroute-prefix/` using its bundled Node runtime. This requires a network connection on first launch only.
- It shares your existing `~/.omniroute/` config and database, so it is continuous with any OmniRoute you already run.
- Server data (quotas, cost) is read via the OmniRoute CLI (`--output json`) and HTTP API on `127.0.0.1:20128`.

## Development

Prerequisites: Rust (stable), Node 22+, Xcode command-line tools.

```sh
npm install
bash scripts/fetch-node.sh        # download the bundled Node runtime
npm run tauri dev                 # run in development
npm run tauri build               # produce a release bundle
```

Run the Rust test suite:

```sh
cargo test --manifest-path src-tauri/Cargo.toml
```

## Signing & notarization

Release builds are ad-hoc signed by default. If you have an Apple Developer ID, set these repository secrets and the release workflow will Developer-ID-sign and notarize automatically:

- `APPLE_SIGNING_IDENTITY`
- `APPLE_ID`, `APPLE_APP_SPECIFIC_PASSWORD`, `APPLE_TEAM_ID`

## Homebrew tap automation

On each tagged release the workflow updates the Cask in the `homebrew-tap` repo (new version + DMG checksum). Cross-repo pushes use a GitHub App token (short-lived, scoped to the tap), not a personal token.

To enable it:

1. Create a GitHub App (Settings → Developer settings → GitHub Apps) with **Repository permissions → Contents: Read and write**.
2. Install the App on the `homebrew-tap` repository.
3. Generate a private key for the App.
4. In the `omniroute-tray` repo, add secrets:
   - `TAP_APP_ID` = the App's ID.
   - `TAP_APP_PRIVATE_KEY` = the generated `.pem` contents.

If these are absent, the release still publishes; only the Cask auto-update step is skipped.

## License

MIT
