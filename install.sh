#!/bin/sh
# Installs the latest agentmon release into ~/.local/bin, or $AGENTMON_BIN.
set -eu

repo="jameson-m/agentmon"
dir="${AGENTMON_BIN:-$HOME/.local/bin}"

case "$(uname -s)" in
    Darwin) os=macos ;;
    Linux) os=linux ;;
    *) echo "agentmon: unsupported system: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) echo "agentmon: unsupported machine: $(uname -m)" >&2; exit 1 ;;
esac
if [ "$os-$arch" = macos-x86_64 ]; then
    echo "agentmon: Intel Macs aren't supported" >&2
    exit 1
fi

url="https://github.com/$repo/releases/latest/download/agentmon-$os-$arch"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
echo "Downloading $url"
curl -fsSL "$url" -o "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp" "$dir/agentmon"
echo "Installed $("$dir/agentmon" -V) to $dir/agentmon"
case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "Add $dir to your PATH to run it as 'agentmon'." ;;
esac
