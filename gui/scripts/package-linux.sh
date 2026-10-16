#!/usr/bin/env bash
# Linux packaging: build the release binary and produce
#   target/package/wizard-gui-<version>-linux-<arch>.tar.gz
# containing the binary (as wizard-gui), the .desktop entry, and the icon, plus
# an install.sh that installs them under ~/.local/share/wizard-gui/<version>
# (the layout the in-app updater swaps) and links ~/.local/bin/wizard-gui.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/wizard-gui-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p zeron
  BIN="$ROOT/target/release/zeron"
else
  cargo build -p zeron
  BIN="$ROOT/target/debug/zeron"
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/wizard-gui"
install -m 644 "$ROOT/dist/wizard-gui.desktop" "$STAGE/wizard-gui.desktop"
install -m 644 "$ROOT/dist/wizard-gui.png" "$STAGE/wizard-gui.png"
install -m 644 "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_NOTICES.md" "$STAGE/"
mkdir -p "$STAGE/licenses/fonts"
cp "$ROOT/crates/ui/assets/fonts/licenses/"* "$STAGE/licenses/fonts/"
cp "$ROOT/crates/voice/NOTICE.md" "$STAGE/licenses/parakeet-v3.txt"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Wizard GUI into ~/.local (no root needed). Each version gets its own
# directory under ~/.local/share/wizard-gui, with a `current` symlink that
# ~/.local/bin/wizard-gui points through. The in-app updater installs the next
# version the same way and keeps the previous one.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSION="__VERSION__"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
case "$DATA" in /*) ;; *) DATA="$HOME/.local/share" ;; esac
ROOT="$DATA/wizard-gui"
APPS="$DATA/applications"
ICONS="$DATA/icons/hicolor/1024x1024/apps"
mkdir -p "$ROOT" "$HOME/.local/bin"
# Copy beside the final name, then rename: an interrupted install never
# leaves a half-copied version the updater would trust.
STAGE="$ROOT/.install-$VERSION-$$"
rm -rf "$STAGE"
cp -a "$HERE/." "$STAGE"
if ! "$STAGE/wizard-gui" --version >/dev/null; then
  rm -rf "$STAGE"
  echo "Wizard GUI could not start; see the loader error above. Install the missing runtime libraries (including ALSA, libasound.so.2), then retry." >&2
  exit 1
fi
rm -rf "${ROOT:?}/$VERSION"
mv "$STAGE" "$ROOT/$VERSION"
ln -sfn "$VERSION" "$ROOT/.current-$$"
mv -Tf "$ROOT/.current-$$" "$ROOT/current"
# Replaces the plain copy older packages put here, too.
ln -sfn "$ROOT/current/wizard-gui" "$HOME/.local/bin/.wizard-gui-$$"
mv -Tf "$HOME/.local/bin/.wizard-gui-$$" "$HOME/.local/bin/wizard-gui"

# Launchers list Wizard GUI through a per-user .desktop entry. The one in the
# tarball says `Exec=wizard-gui` and `TryExec=wizard-gui`, which only resolve
# when ~/.local/bin is on the PATH of the desktop session (often not, e.g. a
# bare Wayland + fuzzel setup), and TryExec then hides the entry outright. So
# write it with absolute paths through the `current` symlink, which keeps
# working across updates. The icon is referenced by path too: the only
# artwork is 1024x1024, a size the hicolor theme doesn't index.
bin="$ROOT/current/wizard-gui"
icon="$ROOT/current/wizard-gui.png"
# Desktop Entry `Exec` quoting: double-quote an argument with reserved
# characters, backslash-escape ", `, $ and \ inside, then double every
# backslash again for the file's own string escaping. `%` must be `%%`.
case "$bin" in
  *[!A-Za-z0-9_./-]*)
    exec_bin="\"$(printf '%s' "$bin" | sed -e 's/\\/\\\\\\\\/g' -e 's/["`$]/\\\\&/g' -e 's/%/%%/g')\""
    ;;
  *) exec_bin="$bin" ;;
esac
try_bin="$(printf '%s' "$bin" | sed 's/\\/\\\\/g')"
icon_val="$(printf '%s' "$icon" | sed 's/\\/\\\\/g')"
mkdir -p "$APPS" "$ICONS"
# Write beside the final name, then rename, so a launcher watching the
# directory never reads a half-written entry (a leading dot is ignored).
entry_tmp="$APPS/.wizard-gui.desktop.$$"
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in
    Exec=*) printf 'Exec=%s %%u\n' "$exec_bin" ;;
    TryExec=*) printf 'TryExec=%s\n' "$try_bin" ;;
    Icon=*) printf 'Icon=%s\n' "$icon_val" ;;
    *) printf '%s\n' "$line" ;;
  esac
done <"$HERE/wizard-gui.desktop" >"$entry_tmp"
mv -f "$entry_tmp" "$APPS/wizard-gui.desktop"
cp "$HERE/wizard-gui.png" "$ICONS/.wizard-gui.png.$$"
mv -f "$ICONS/.wizard-gui.png.$$" "$ICONS/wizard-gui.png"
# Earlier packages installed the same app as zeron. Drop that launcher entry
# (only if it is ours) so the menu does not list Wizard GUI twice.
if [ -f "$APPS/zeron.desktop" ] && grep -q '^Name=Wizard GUI$' "$APPS/zeron.desktop"; then
  rm -f "$APPS/zeron.desktop" "$ICONS/zeron.png"
  if [ -e "$HOME/.local/bin/zeron" ]; then
    echo "An older copy is still at ~/.local/bin/zeron; remove it if nothing else uses it."
  fi
fi
# Best-effort cache refresh; both tools are optional. The icon cache is only
# refreshed, never created: a user-level hicolor cache nobody else maintains
# would hide icons other apps later install there.
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$APPS" >/dev/null 2>&1 || true
[ -f "$DATA/icons/hicolor/icon-theme.cache" ] \
  && command -v gtk-update-icon-cache >/dev/null 2>&1 \
  && gtk-update-icon-cache -q -t -f "$DATA/icons/hicolor" >/dev/null 2>&1 || true
echo "Installed Wizard GUI $VERSION. Open it from your app menu, or run wizard-gui."
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo "Add ~/.local/bin to your PATH to run \`wizard-gui\` from a terminal." ;;
esac
INSTALL
sed -i "s/__VERSION__/$VERSION/" "$STAGE/install.sh"
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
