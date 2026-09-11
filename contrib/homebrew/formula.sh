#!/usr/bin/env bash
# Print the tap formula for a release: contrib/homebrew/formula.sh 3.0.1 checksums.txt
# The release workflow writes the output to Formula/wizard.rb in teddytennant/homebrew-tap.
set -euo pipefail

version="${1:?usage: formula.sh VERSION CHECKSUMS_FILE}"
checksums="${2:?usage: formula.sh VERSION CHECKSUMS_FILE}"
version="${version#v}"

digest_of() {
  local asset="$1" sum
  sum="$(awk -v a="$asset" '$2 == a { print $1 }' "$checksums")"
  [ -n "$sum" ] || { echo "formula.sh: $asset is not in $checksums" >&2; exit 1; }
  printf '%s' "$sum"
}

base="https://github.com/teddytennant/wizard/releases/download/v$version"

# Resolved before the heredoc: an exit inside $(...) only ends that subshell,
# so a missing asset must be caught here, not in the Ruby text.
mac_arm="$(digest_of wizard-aarch64-apple-darwin.tar.gz)"
mac_intel="$(digest_of wizard-x86_64-apple-darwin.tar.gz)"
linux_arm="$(digest_of wizard-aarch64-unknown-linux-gnu.tar.gz)"
linux_intel="$(digest_of wizard-x86_64-unknown-linux-gnu.tar.gz)"

cat <<RUBY
class Wizard < Formula
  desc "One line. Your sovereign agent. Self-extending. Bring any model"
  homepage "https://github.com/teddytennant/wizard"
  license all_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "$base/wizard-aarch64-apple-darwin.tar.gz"
      sha256 "$mac_arm"
    end
    on_intel do
      url "$base/wizard-x86_64-apple-darwin.tar.gz"
      sha256 "$mac_intel"
    end
  end

  on_linux do
    on_arm do
      url "$base/wizard-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$linux_arm"
    end
    on_intel do
      url "$base/wizard-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$linux_intel"
    end
  end

  def install
    bin.install "wizard"
  end

  test do
    assert_match "wizard #{version}", shell_output("#{bin}/wizard --version")
  end
end
RUBY
