#!/usr/bin/env bash
# SPDX-License-Identifier: EUPL-1.2
# Usage: apt-repository DIR KEY [DEB...]
# Adds each DEB to the APT repository in DIR, signs every .deb in its pool
# that has no .sig yet, writes the indexes and a Release valid for 30 days for
# the suites stable, trixie and noble, signs them with KEY (InRelease and
# Release.gpg) and exports the public key to DIR/nuschl-archive-keyring.gpg.
# Run it without DEBs to re-sign the repository before Valid-Until passes.
set -euo pipefail
if test -t 0; then
  GPG_TTY=$(tty)
  export GPG_TTY
fi
dir=$1
key=$(gpg --batch --with-colons --fingerprint --list-keys "$2" | awk -F: '$1 == "fpr" && !seen { print $10; seen=1 }')
shift 2
test -n "$key"
pool=$dir/pool/main/n/nuschl
mkdir -p "$pool"
for deb in "$@"; do
  # shellcheck disable=SC2016 # dpkg-deb expands the fields
  name=$(dpkg-deb --showformat '${Package}_${Version}_${Architecture}' --show "$deb")
  target=$pool/$name.deb
  if test -e "$target"; then
    cmp -s "$deb" "$target" || { echo "APT package version already exists with different bytes: $name" >&2; exit 1; }
  else
    install -m644 "$deb" "$target"
  fi
done
sign() { gpg --batch --yes --local-user "$key" --digest-algo SHA512 "$@"; }
for deb in "$pool"/*.deb; do
  if test -e "$deb.sig"; then
    gpg --batch --assert-signer "$key" --verify "$deb.sig" "$deb"
  else
    sign --detach-sign --output "$deb.sig.new" "$deb"
    mv "$deb.sig.new" "$deb.sig"
  fi
done
cd "$dir"
gpg --batch --yes --export --output nuschl-archive-keyring.gpg "$key"
packages=dists/stable/main/binary-amd64
mkdir -p "$packages"
apt-ftparchive -o APT::FTPArchive::MD5=false -o APT::FTPArchive::SHA1=false packages pool > "$packages/Packages"
gzip -9 -n -c "$packages/Packages" > "$packages/Packages.gz"
for suite in stable trixie noble; do
  indices=dists/$suite/main/binary-amd64
  mkdir -p "$indices"
  test "$suite" = stable || cp "$packages/Packages" "$packages/Packages.gz" "$indices/"
  # Keep indexed bytes available by digest while clients update across releases.
  for algorithm in SHA256 SHA512; do
    mkdir -p "$indices/by-hash/$algorithm"
    for index in "$indices/Packages" "$indices/Packages.gz"; do
      digest=$("${algorithm,,}sum" "$index")
      install -m644 "$index" "$indices/by-hash/$algorithm/${digest%% *}"
    done
  done
  rm -f "dists/$suite/Release" "dists/$suite/InRelease" "dists/$suite/Release.gpg"
  apt-ftparchive -o APT::FTPArchive::MD5=false -o APT::FTPArchive::SHA1=false \
    -o APT::FTPArchive::Release::Origin=Nuschl -o APT::FTPArchive::Release::Label=Nuschl \
    -o APT::FTPArchive::Release::Suite="$suite" -o APT::FTPArchive::Release::Codename="$suite" \
    -o APT::FTPArchive::Release::Architectures=amd64 -o APT::FTPArchive::Release::Components=main \
    -o APT::FTPArchive::Release::Acquire-By-Hash=yes \
    -o APT::FTPArchive::Release::ValidTime=2592000 release "dists/$suite" > Release
  mv Release "dists/$suite/Release"
  sign --clearsign --output "dists/$suite/InRelease" "dists/$suite/Release"
  sign --armor --detach-sign --output "dists/$suite/Release.gpg" "dists/$suite/Release"
done
