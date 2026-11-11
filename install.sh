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

# Check the download against the release's checksums, where a tool exists.
sums="$(curl -fsSL "https://github.com/$repo/releases/latest/download/SHA256SUMS" || true)"
want="$(printf '%s\n' "$sums" | awk -v name="agentmon-$os-$arch" '$2 == name { print $1 }')"
if command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$tmp" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
    got="$(shasum -a 256 "$tmp" | awk '{ print $1 }')"
else
    got=""
fi
if [ -n "$want" ] && [ -n "$got" ] && [ "$want" != "$got" ]; then
    echo "agentmon: checksum mismatch, not installing" >&2
    exit 1
fi
mkdir -p "$dir"
install -m 755 "$tmp" "$dir/agentmon"
echo "Installed $("$dir/agentmon" -V) to $dir/agentmon"
case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "Add $dir to your PATH to run it as 'agentmon'." ;;
esac
