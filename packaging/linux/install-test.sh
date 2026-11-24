#!/bin/sh
# SPDX-License-Identifier: EUPL-1.2
# Runs as root in a clean distribution image. Installs Nuschl with the
# distribution's package manager, checks the installation, upgrades it and
# removes it.
#   install-test.sh apt REPOSITORY SUITE VERSION NEXT-VERSION
#   install-test.sh pacman PACKAGE
set -eux
manager=$1
shift
id tester 2>/dev/null || useradd --create-home tester

# The content worker renders a message through the sandbox, as an
# unprivileged user and with the test VM's Nix store hidden. Without prlimit
# on the PATH the sandbox fails closed.
hidden='mkdir -p /nix && mount -t tmpfs none /nix && exec runuser -u tester --'
profile=/etc/apparmor.d/nuschl-bwrap

sandbox() {
  unshare --mount sh -c "$hidden nuschl render 'sandboxed **content**'" | grep -F content
  if unshare --mount sh -c "$hidden env PATH=/nonexistent /usr/bin/nuschl render text"; then exit 1; fi
}

# Whether AppArmor restricts unprivileged user namespaces, as Ubuntu does.
restricted() {
  test "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)" = 1
}

worker_starts() {
  status=0
  output=$(unshare --mount sh -c "$hidden /usr/lib/nuschl/$1" 2>&1) || status=$?
  printf '%s\n' "$output" | grep -F "$2"
  test "$status" -eq 1
}

check() {
  unshare --mount sh -c 'mkdir -p /nix && mount -t tmpfs none /nix && nuschl --version' | grep -F "$1"
  worker_starts nuschl-rtc-worker 'usage: nuschl-rtc-worker --call'
  worker_starts nuschl-video-worker 'invalid video worker role'
  for scheme in mumble nuschl; do
    test "$(runuser -u tester -- xdg-mime query default "x-scheme-handler/$scheme")" = org.nuschl.Nuschl.desktop
  done
  sandbox
}

sources() {
  rm -f /etc/apt/sources.list /etc/apt/sources.list.d/*
  cat > /etc/apt/sources.list.d/nuschl.sources <<SOURCES
Types: deb
URIs: file:$1
Suites: $2
Components: main
Signed-By: $1/nuschl-archive-keyring.gpg
SOURCES
  apt-get update
}

case $manager in
apt)
  repository=$1 suite=$2 version=$3 next=$4
  sources "$repository" "$suite"
  apt-get install --yes "nuschl=$version"
  check "${version%-*}"
  if restricted; then
    # Without the package's AppArmor profile the sandbox fails closed; the
    # upgrade loads the profile again.
    apparmor_parser --remove "$profile"
    if unshare --mount sh -c "$hidden nuschl render text"; then exit 1; fi
  fi
  apt-get install --yes nuschl
  test "$(dpkg-query --show --showformat '${Version}' nuschl)" = "$next"
  check "${next%-*}"
  apt-get remove --yes nuschl
  test ! -e /usr/lib/nuschl
  test -z "$(runuser -u tester -- xdg-mime query default x-scheme-handler/mumble)"
  apt-get purge --yes nuschl
  if dpkg-query --show nuschl; then exit 1; fi
  test ! -e "$profile"
  if restricted && grep '^nuschl-bwrap ' /sys/kernel/security/apparmor/profiles; then exit 1; fi
  ;;
pacman)
  package=$1
  pacman --upgrade --noconfirm "$package"
  check "$(pacman --query nuschl-bin | cut -d' ' -f2 | cut -d- -f1)"
  pacman --remove --noconfirm nuschl-bin
  test ! -e /usr/lib/nuschl
  test -z "$(runuser -u tester -- xdg-mime query default x-scheme-handler/mumble)"
  ;;
esac
echo "install test passed"
