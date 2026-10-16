# Wizard GUI

The desktop app for Wizard. It runs Wizard, Pi and Claude Code side by side across your projects, and on machines you reach over SSH.

Wizard GUI is a fork of [Zeron](https://github.com/zeronsh/zeron), the open-source agent controller built by the Zeron team ([zeron.sh](https://zeron.sh)). Nearly all of it is their work: the engine, sync, transcript, composer and gpui interface. Thank you to Zeron's creators. It stays under their MIT license (see [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)).

`wizard gui` opens this app once it is installed.

What the fork changes:

- Only Wizard, Pi and Claude Code are offered, and Wizard is the default.
- First-run onboarding installs the agents and lets Wizard reuse a Codex (ChatGPT) or Grok CLI sign-in it finds on disk.
- SSH devices: Settings, Devices, Add SSH device. It uses your SSH keys, installs the engine and Wizard on the remote, can share this machine's Wizard sign-in, and starts the engine there. The machine then shows up in the device menu above the message box.
- Plugins: Settings, Plugins browses the Pi package gallery and installs a package for Pi, for Wizard, or both. Wizard support is in beta: skills and prompts work, extensions don't yet. Before installing, the page shows which parts of the package work in Wizard. It runs `wizard plugins` on the device, so remote devices work too (see [docs/pi-plugins.md](../docs/pi-plugins.md)).
- No update check against Zeron's release feed, since that would replace this build with upstream Zeron.
- Voice calls run on Grok Voice with Wizard's xAI sign-in (see [Voice](#voice)).

## Keyboard

Settings, General, Keyboard has two switches, both off by default and independent of each other.

**Power user mode** drives the whole app from the keyboard with vim-style keys. Esc leaves the message box (or a settings page) for navigation, a NAV tag in the corner says which pane has the cursor, and a ring marks the pane and the row. `i` goes back to typing. Keys never fire inside a text field. `?` lists every key, and after a prefix like Space or `g` a hint shows what can follow. The slash menu also gains the app's own commands (`/agent`, `/effort`, `/device`, `/archive`, `/export`, `/theme`, `/keys`, `/sidebar`, `/panel`, `/next`, `/prev`, `/vim`), argument hints, and fuzzy matching, next to whatever the agent advertises.

| Keys | Does |
| --- | --- |
| `j` `k`, `gg` `G`, Enter | Move through sessions or messages, open |
| `h` `l`, `Ctrl-w h/j/k/l/w` | Move between sidebar, transcript, message box, right panel |
| `o`, `y`, `Ctrl-d` `Ctrl-u` | Expand a tool card, copy a message or code block, half page |
| `x` or `dd`, `D` | Archive or delete a session (asks first) |
| `gt` `gT`, `1`-`9` | Right panel tabs |
| `c` or `Space n`, `Space a/m/e/d` | New chat, switch agent, model, effort, device |
| `Space ,`, `:`, `/`, `?` | Settings, command palette, search sessions, all keys |

**Vim editing in the message box** gives the composer normal, insert, visual and visual-line modes: motions, counts, operators with text objects (`ciw`, `da(`, `yi"`), `p`, `u`, `Ctrl-r`, `.` and `>>`. A tag above the box shows the mode and any pending keys. In insert mode Enter works as before; in normal mode Enter sends. With both switches on, Esc in normal mode leaves the box for navigation.

Screenshots are in [docs/screenshots/power-user](docs/screenshots/power-user).

## Upstream sources

gui/ follows Zeron upstream as a whole, including the parts Wizard does not ship: the iOS app (`apps/ios`) and its Rust core (`crates/mobile`, `crates/client`, `crates/text`, built by `scripts/ios`), the zeron.sh landing and redirect sites (`apps/landing`, `apps/www-redirect`), the Cloudflare edge worker (`edge`), Zeron's own CI workflows (`gui/.github`, which GitHub does not run from here), the translated READMEs, and the doc screenshots and media. They stay as upstream wrote them, so each sync is a plain merge. Wizard's releases build only the desktop app.

## Voice

When Wizard is signed in to xAI on this machine (`wizard --login xai`), the microphone button in the sidebar footer starts a call with Grok Voice 3.0. You talk; it answers out loud and works your chats for you: it lists and reads them, starts new ones (Wizard unless you name another agent), sends them messages and checks back on them. Esc hides the call screen without ending the call, M mutes, and the red button hangs up. Settings, Voice picks its voice (Eve, Ara, Rex, Sal or Leo).

Audio stays on this machine: only the call itself goes to api.x.ai, with the token `wizard xai-token` hands out (Wizard keeps refreshing it, so the GUI never touches the refresh token). Talk over an answer and it stops at once. While it speaks, the microphone only sends sound clearly louder than the echo it measures from your speakers, so it doesn't interrupt itself; with headphones there is no echo and any speech gets through. On Linux it skips a monitor source if that is the default input and records from your real microphone. A dropped connection resumes the same conversation.

Dictation (the on-device Parakeet model) is separate and lives on the same settings page.

## Install

Every Wizard release from 3.5.0 on carries the app:

- Linux: `wizard-gui-<version>-linux-<arch>.tar.gz`. Unpack it and run `./install.sh`, which installs it under `~/.local/share/wizard-gui`, links `wizard-gui` into `~/.local/bin` and adds a Wizard GUI desktop entry.
- macOS (Apple silicon): `wizard-gui-<version>-macos-arm64.dmg`. It is not signed or notarized unless the release had signing credentials, so the first launch needs right click, Open. Drag `Wizard GUI.app` into Applications; `wizard gui` finds it there.
- Windows: `wizard-gui-<version>-windows-x86_64.zip`. Unpack it and run `zeron.exe`.

## Updating

The app checks Wizard's GitHub releases when it starts and every six hours after. When there is a newer version, a banner at the bottom of the sidebar offers **Update** and **What's new**. Update downloads it with a progress bar; once it is ready, **Restart to update** swaps it in and reopens the app. That button only shows while no agent is running and no terminal is open. A downloaded update also installs whenever you quit.

Settings → About shows the version, where the app is installed, **Check for updates** with the time of the last check, and **Install updates automatically** (off by default). With it on, updates download in the background and install the next time the app quits or restarts. The command palette has Check for updates, Update Wizard GUI and Restart to update, and `wizard-gui update` does the same from a terminal (`--check` only reports, exiting 1 when an update is out).

Nothing installs unless it verifies: `gui-checksums.txt` has to carry a minisign signature from the key in [`wizard-release.pub`](../wizard-release.pub), the same key `wizard update` trusts, and the download has to match its checksum there. The previous version is kept: macOS leaves it next to the app as `.Wizard GUI.app.previous`, Linux keeps its directory under `~/.local/share/wizard-gui`.

What can update itself:

- macOS: `Wizard GUI.app` in a folder your account can write, such as `/Applications` for an admin or `~/Applications`.
- Linux: an install from `install.sh`. One from an older `install.sh` (a plain file in `~/.local/bin`) moves to the new layout on its first update.
- Windows: the unpacked portable zip.

Anything else (a read-only `/Applications`, the app still inside the disk image, an AppImage, a Nix or system package, a source build) shows **Download** with the reason, and gets updated the way it was installed.

`WIZARD_GUI_RELEASES_URL` points the updater at a mirror laid out like GitHub's releases (`latest/download/…`, `download/v<version>/…`). It has to be HTTPS, and the signature check does not change.

## Build from source

```bash
cd gui
cargo run --release -p zeron
```

Linux needs the gpui libraries: xkbcommon, wayland, x11/xcb, fontconfig, freetype, alsa, vulkan and webkit2gtk-4.1 (the list the release workflow installs is in `.github/workflows/release.yml`). The sidebar browser also needs the [Linux browser runtime](docs/reference/linux-browser.md). Windows source builds are covered in [docs/reference/windows-development.md](docs/reference/windows-development.md).

Day to day (a source build's binary is `target/release/zeron`; the Linux package installs it as `wizard-gui`):

```bash
wizard-gui status
wizard-gui daemon start|stop|restart|status
```

Everything is local by default. The optional multi-device sync (`wizard-gui login`) goes through Zeron's hosted service.

How the pieces fit is in [ARCHITECTURE.md](ARCHITECTURE.md).
