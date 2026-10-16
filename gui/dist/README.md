# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/wizard-gui-<version>-linux-<arch>.tar.gz` containing:

- `wizard-gui` — the binary (headed by default; `wizard-gui headless` runs the engine alone)
- `wizard-gui.desktop` — XDG desktop entry template, `StartupWMClass` matching the
  window's app id (`Exec=wizard-gui` for packagers; `install.sh` rewrites
  `Exec`, `TryExec`, and `Icon` to absolute paths under
  `~/.local/share/wizard-gui/current`, since `~/.local/bin` is often not on a
  desktop session's `PATH`)
- `wizard-gui.png` — 1024×1024 app icon
- `install.sh` — copies the package to `~/.local/share/wizard-gui/<version>`,
  points `~/.local/share/wizard-gui/current` at it, links
  `~/.local/bin/wizard-gui` through `current`, installs the desktop entry and
  icon, and removes a `zeron.desktop` left by an older package. The in-app
  updater installs later versions the same way.

The cargo binary is still `zeron`; the script installs it under the new name.

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/zeron-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Wizard GUI.app` (executable
`Contents/MacOS/wizard-gui`, Info.plist, icns), ad-hoc signs it (set
`CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a dmg with the
volume name "Wizard GUI". Output files keep the `zeron-` prefix and the release
workflow renames them to `wizard-gui-*`. The bundle identifier stays
`sh.zeron.app` so notification permission and saved preferences carry over;
the in-app updater refuses a downloaded bundle whose identifier differs. The
updater installs from the same dmg. CI runs this on tags
(`.github/workflows/release.yml`). The manual steps it automates, for reference
(run on a macOS host — gpui needs Metal; no cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p zeron --target aarch64-apple-darwin
   cargo build --release -p zeron --target x86_64-apple-darwin
   lipo -create -output zeron \
     target/aarch64-apple-darwin/release/zeron \
     target/x86_64-apple-darwin/release/zeron
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p "Wizard GUI.app"/Contents/{MacOS,Resources}
   cp zeron "Wizard GUI.app"/Contents/MacOS/wizard-gui
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > "Wizard GUI.app"/Contents/Info.plist
   ```
3. Icon: generate `wizard-gui.icns` from `dist/macos/icon-1024.png` (the macOS-shaped
   variant of the artwork — squircle mask, margins, and shadow pre-baked, since
   `sips` can't apply an alpha mask) and place it at
   `Wizard GUI.app/Contents/Resources/wizard-gui.icns`:
   ```sh
   mkdir wizard-gui.iconset && sips -z 256 256 dist/macos/icon-1024.png --out wizard-gui.iconset/icon_256x256.png
   iconutil -c icns wizard-gui.iconset -o "Wizard GUI.app"/Contents/Resources/wizard-gui.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" "Wizard GUI.app"
   xcrun notarytool submit wizard-gui.zip --keychain-profile … --wait
   xcrun stapler staple "Wizard GUI.app"
   ```
5. Ship as a `.dmg` (`hdiutil create -volname "Wizard GUI" -srcfolder "Wizard GUI.app" -ov -format UDZO wizard-gui.dmg`).
