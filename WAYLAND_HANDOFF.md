# Wayland / Hyprland Support — Hand-off Document

_Last updated: 2026-06-08. Host: CachyOS + Hyprland. Goal: usable RustDesk remote
desktop on Wayland/Hyprland, remoting in from a Steam Deck._

This document is a self-contained snapshot so work can resume in a fresh session.
There is also persistent assistant memory under
`~/.claude-dont/projects/-home-primo-Projects-rustdesk/memory/` (files:
`wayland-impl-concerns.md`, `wayland-impl-files.md`, `wayland-portal-reauth.md`).

---

## 1. Current state at a glance

- All work is committed on branch **`wayland-hyprland-work`** in BOTH the parent
  repo and the `libs/hbb_common` submodule. **Nothing is pushed** — local only.
- The Rust side **builds cleanly**. The Flutter GUI does **not** build on this
  machine due to a Flutter SDK version mismatch (see §5) — unrelated to our code.
- The Wayland fix has **not been runtime-tested yet**. A fast test path (library
  swap) is ready (see §6).

---

## 2. Git state (exact)

Host versions: installed package `rustdesk-bin 1.4.6`, dev tree `1.4.6` (same).

### Parent repo — branch `wayland-hyprland-work` (off `master` @ 27822395c)
```
4c6033c99 feat(wayland): persist per-monitor restore tokens on Hyprland (Option B)
819f58ac5 fix(wayland): keep Hyprland portal session alive across reconnects   (Option A)
652aa8796 style: rustfmt formatting churn
f758916cd feat(wayland): Hyprland capture and input support   (+ submodule bump)
27822395c (master base)
```
Working tree intentionally left dirty (NOT part of the work):
`flutter/pubspec.lock` (lockfile churn), `.codex/`, `flutter/linux/bridge_generated.h` (untracked).

### Submodule `libs/hbb_common` — branch `wayland-hyprland-work`
```
5b4f17a style: rustfmt formatting churn
dc479c8 fix(linux): prefer hyprctl for Hyprland display detection
f2a9b22 fix(linux): recover Hyprland wayland display env   (was detached HEAD before)
48c37de (upstream base)
```
Parent commit `f758916cd` bumps the submodule pointer to `5b4f17a`.

> NOTE: there is a separate `rebase-upstream-hyprland` branch in a prunable
> worktree at `/tmp/rustdesk-rebase` — a DIFFERENT effort, kept clear of this work.

---

## 3. What each commit does

- **`f758916` feat(wayland): Hyprland capture and input support** — the bulk of the
  original work (PipeWire on-demand capturers, per-monitor portal handling,
  `extend_with_additional_grants`, uinput keyboard fallback chain, Hyprland-aware
  display switching, mouse coordinate handling). Includes the submodule bump.
- **`652aa87` style** — rustfmt-only churn, separated so the functional diff is
  reviewable (enigo macos keycodes, request_portal reindent, etc.).
- **`819f58a` Option A — keep Hyprland portal session alive across reconnects** —
  one-line-ish change in `try_close_session` (libs/scrap/src/wayland/pipewire.rs):
  don't drop the portal session on disconnect on Hyprland, so you authorize once
  per launch and reconnects reuse the live session. Trade-off: capture stays
  "active" while idle; does NOT survive a RustDesk restart.
- **`4c6033c` Option B — persist per-monitor restore tokens on Hyprland** — makes
  the screen-share auth survive RustDesk restarts/reboots. See §4.

Both Option A and B are isolated, independently revertable, and compose.

---

## 4. The re-auth problem and the fix (Option A + B)

**Symptom:** On Hyprland the screen-share portal pop-up reappeared on every
reconnect even though RustDesk was never restarted — impossible to approve when
away (e.g. on holiday remoting from the Steam Deck).

**Root cause (both in `libs/scrap/src/wayland/pipewire.rs`):**
1. `try_close_session()` dropped the portal session on disconnect whenever
   `is_support_restore_token` was true (xdph reports ScreenCast portal v4).
2. `should_use_restore_token()` returned `... && !is_hyprland_session()`, i.e.
   restore tokens were DISABLED on Hyprland, so nothing was ever saved to restore
   the session silently. → fresh prompt on every reconnect.

**Option A (`819f58a`)** keeps the session alive for the process lifetime →
"auth once per launch."

**Option B (`4c6033c`)** persists tokens so it survives restarts too:
- New per-monitor token map config key `wayland-restore-tokens` (JSON
  `{ "DP-3": "...", "HDMI-A-1": "..." }`). The legacy single key
  `wayland-restore-token` can't cover multi-monitor (portal grants ONE monitor per
  session). Non-Hyprland still uses the single key, untouched.
- `should_use_restore_token` no longer excludes Hyprland.
- `request_remote_desktop(capture_cursor, restore_token: Option<String>)` — new
  param threads a specific monitor's token into SelectSources.
- Token saved in `on_start_response`, keyed to the granted monitor via
  `monitor_name_for_streams()` which matches by stream **resolution** (Hyprland
  per-monitor streams report position (0,0), so size is the reliable identity;
  unambiguous because this host's monitors have distinct resolutions).
- `build_hyprland_sessions()` builds one session per monitor up front, restoring
  saved tokens silently and prompting once for any monitor without one, with
  dedup so out-of-order picks don't double-prompt. Wired into `get_capturables()`.

**REQUIRED user config (already done 2026-06-08):** `~/.config/hypr/xdph.conf`:
```
screencopy {
    allow_token_by_default = true
}
```
Without it xdph shows the picker with the "allow restore token" box unticked, so
restore still prompts. xdph must be restarted to pick up the config.

**Residual limitation (cannot fix from RustDesk):** xdph's restore implementation
can still re-prompt in some cases — upstream
https://github.com/hyprwm/xdg-desktop-portal-hyprland/issues/350 and #123.

Host monitors (distinct resolutions → size-based identity is safe):
- DP-3: 3440x1440 @ scale 1.0 at (0,1440)
- HDMI-A-1: 3840x2160 @ scale 1.5 at (440,0)
Workflow: views ONE monitor at a time, switches between them.

---

## 5. Build status & the Flutter blocker

- `VCPKG_ROOT=/home/primo/vcpkg cargo check -p scrap` → clean.
- `VCPKG_ROOT=/home/primo/vcpkg python3 build.py --flutter` → **Rust lib builds**
  (`target/release/liblibrustdesk.so`), but `flutter build linux --release` FAILS.

**Why Flutter fails (pre-existing, NOT our change):**
- System Flutter is **3.44.0**; RustDesk pins **3.22.3 / 3.24.5** (see
  `.github/workflows/*.yml` `FLUTTER_VERSION`).
- `google_fonts 6.2.1` uses a `const` map keyed by `FontWeight`, rejected by Dart
  in Flutter 3.44 ("does not have a primitive operator '=='"). Plus FFI/bridge
  errors in `peer_card.dart`. Classic "SDK too new for pinned deps."

**Proper long-term fix:** install Flutter **3.24.5** (e.g. via `fvm`) and build
with that. Not done yet.

---

## 6. How to test WITHOUT a Flutter build (library swap)

Installed package and dev tree are both `1.4.6`, and our change touches no FFI
signatures, so the freshly built Rust lib is compatible with the installed
Flutter GUI. Swap the lib in (root-owned bundle → needs sudo; run via `! ` in the
prompt). Reversible.

Install patched lib + restart everything (run in the prompt; the leading `! `
makes the session run it so output lands here):
```
! sudo cp /usr/share/rustdesk/lib/librustdesk.so /usr/share/rustdesk/lib/librustdesk.so.bak && sudo cp /home/primo/Projects/rustdesk/target/release/liblibrustdesk.so /usr/share/rustdesk/lib/librustdesk.so && sudo systemctl restart xdg-desktop-portal-hyprland.service 2>/dev/null; systemctl --user restart xdg-desktop-portal-hyprland.service; sudo systemctl restart rustdesk
```
That: backs up the stock lib → installs our patched lib → restarts xdph (so
`allow_token_by_default` takes effect, trying both system and user units) →
restarts the rustdesk service.

Note: the built artifact is `liblibrustdesk.so` (crate is `librustdesk`); the
bundle expects `librustdesk.so` (one fewer `lib`).

Revert to stock lib:
```
! sudo cp /usr/share/rustdesk/lib/librustdesk.so.bak /usr/share/rustdesk/lib/librustdesk.so && sudo systemctl restart rustdesk
```

**Test procedure:**
1. Rebuild the lib if code changed: `VCPKG_ROOT=/home/primo/vcpkg cargo build --features flutter --lib --release`
2. Swap it in (above) and ensure xdph was restarted after the config change.
3. Connect from Steam Deck → approve the picker once per monitor as you switch
   (the "allow restore token" box should be pre-ticked).
4. `sudo systemctl restart rustdesk` (simulate the restart that used to re-prompt).
5. Reconnect → previously-approved monitors should come up with NO prompt.

If the app misbehaves after the swap, revert — that cleanly rules our lib in/out.

---

## 7. Open items / next steps

- [ ] **Runtime-test Option A + B** via the library swap (§6). This is the
      immediate next step.
- [ ] **Push** the branches when ready (currently local-only). Order matters:
      push the `hbb_common` submodule branch to ITS remote FIRST, then the parent,
      or the submodule pointer will dangle for anyone else.
- [ ] **Long-term build fix:** install Flutter 3.24.5 (fvm) for a full GUI build.
- [ ] **Multi-monitor robustness (Option B):** identity is resolution-based; fine
      for this host (distinct resolutions) but ambiguous for same-resolution
      monitors. Generalizing would need a sturdier monitor↔token identity.
- [ ] **Decide A vs B coexistence:** with B working, Option A's keep-alive could
      be reverted to stop idle capture (the "screen is being shared" indicator
      while nobody is connected). Currently both are on.
- [ ] **Upstreamability:** large diff is heavily branched on `is_hyprland_session()`;
      would need consolidation + the rustfmt churn kept separate to merge upstream.
      See memory `wayland-impl-concerns.md`.

---

## 8. Key code references (parent repo unless noted)

- `libs/scrap/src/wayland/pipewire.rs` — capture core, restore tokens, the A & B
  fixes. Key fns: `try_close_session` (A), `should_use_restore_token`,
  `load_restore_tokens` / `save_restore_token_for` / `monitor_name_for_streams`,
  `request_remote_desktop`, `on_start_response`, `build_hyprland_sessions`,
  `get_capturables`, `extend_with_additional_grants`.
- `src/server/connection.rs` — `try_close_session` call on last-client-disconnect
  (~line 5677); Hyprland display switching; `Retina` mouse coordinate handling.
- `src/server/wayland.rs` — on-demand capturer (`get_capturer_for_display`),
  `active_display_count`.
- `src/server/uinput.rs` — `KeyboardDevice` fallback chain.
- `libs/hbb_common/src/platform/linux.rs` (submodule) — `get_wayland_displays`
  prefers `hyprctl -j monitors`; `WaylandDisplayInfo` struct.

Config keys live in `libs/hbb_common/src/config.rs` (LocalConfig):
`wayland-restore-token` (legacy single), `wayland-restore-tokens` (new per-monitor
map), `wayland-pipewire-display-offset`.

---

## 9. Build/run env notes

- `VCPKG_ROOT=/home/primo/vcpkg` is required for any scrap/rust build (libyuv etc.).
  It is NOT exported in the default shell — set it inline.
- Running RustDesk on the host is the systemd service `rustdesk.service` (enabled,
  with `/etc/systemd/system/rustdesk.service.d/wayland.conf` drop-in). It spawns
  `--server` (user session) which does the capture. Stop/restart needs sudo.
