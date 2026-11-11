#!/bin/sh
# Prints the Homebrew formula for a published release, for the tap repository:
#   scripts/formula.sh 0.1.0 > ../homebrew-tap/Formula/agentmon.rb
set -eu
version="${1:?usage: formula.sh <version>}"
base="https://github.com/jameson-m/agentmon/releases/download/v$version"
sums="$(curl -fsSL "$base/SHA256SUMS")"
sum() { printf '%s\n' "$sums" | awk -v name="agentmon-$1" '$2 == name { print $1 }'; }

cat <<EOF
class Agentmon < Formula
  desc "Terminal system monitor for running many Claude Code sessions"
  homepage "https://github.com/jameson-m/agentmon"
  version "$version"
  license "MIT"

  on_macos do
    depends_on arch: :arm64
    url "$base/agentmon-macos-aarch64"
    sha256 "$(sum macos-aarch64)"
  end

  on_linux do
    on_intel do
      url "$base/agentmon-linux-x86_64"
      sha256 "$(sum linux-x86_64)"
    end
    on_arm do
      url "$base/agentmon-linux-aarch64"
      sha256 "$(sum linux-aarch64)"
    end
  end

  def install
    bin.install Dir["agentmon-*"].first => "agentmon"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/agentmon --version")
  end
end
EOF
