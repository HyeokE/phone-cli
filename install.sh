#!/bin/sh
# Installs phone-cli on a Mac from the latest GitHub release, with only the tools macOS
# ships (curl, tar, shasum). No Rust needed.
#
#   curl -fsSL https://raw.githubusercontent.com/HyeokE/phone-cli/main/install.sh | sh
#
# PHONE_VERSION=v0.1.0 picks a release instead of the latest one.
# PHONE_INSTALL_DIR picks the folder instead of ~/.local/bin.
# The pre-rename IPH_VERSION / IPH_INSTALL_DIR are still read when unset.
set -eu

repo="HyeokE/phone-cli"
asset="phone-macos-universal.tar.gz"
version="${PHONE_VERSION:-${IPH_VERSION:-}}"
dir="${PHONE_INSTALL_DIR:-${IPH_INSTALL_DIR:-$HOME/.local/bin}}"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "install.sh is for macOS. On Windows, download phone-windows-x86_64.zip from https://github.com/$repo/releases" >&2
  exit 1
fi

if [ -n "$version" ]; then
  base="https://github.com/$repo/releases/download/$version"
else
  base="https://github.com/$repo/releases/latest/download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $base/$asset"
curl -fsSL -o "$tmp/$asset" "$base/$asset"
curl -fsSL -o "$tmp/$asset.sha256" "$base/$asset.sha256"
(cd "$tmp" && shasum -a 256 -c "$asset.sha256" >/dev/null) || {
  echo "Checksum mismatch for $asset" >&2
  exit 1
}

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
# Replace rather than overwrite in place: a running phone-cli keeps its old file.
cp "$tmp/phone/phone" "$dir/phone.new"
chmod 755 "$dir/phone.new"
mv -f "$dir/phone.new" "$dir/phone"
xattr -d com.apple.quarantine "$dir/phone" 2>/dev/null || true

echo "Installed $("$dir/phone" --version) to $dir/phone"
case ":$PATH:" in
  *":$dir:"*) ;;
  *)
    echo
    echo "$dir is not on your PATH. Add it, for example:"
    echo "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc && exec zsh"
    ;;
esac
echo
echo "Next: plug the iPhone in by USB, tap Trust on it, and run: phone setup"
