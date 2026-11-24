# Debian, Ubuntu and Arch packages

Nuschl publishes one `.deb` for amd64 in an APT repository at
`https://dl.nuschl.eu/apt`, and the AUR package `nuschl-bin`, which repackages
that `.deb`. [install.md](install.md#debian-and-ubuntu) shows how users install them.

## The package

`packaging/linux/default.nix` builds both from the portable binaries below
(`linux-portable`). The stage puts the programs next to each other in
`/usr/lib/nuschl/`, since helper discovery looks in the executable's directory:

```
nuschl                        nuschl-rtc-worker
nuschl-content-worker         nuschl-storage-worker
nuschl-crypto-worker          nuschl-transfer-worker
nuschl-math-worker            nuschl-update-verify-ed25519
nuschl-media-worker           nuschl-update-verify-mldsa44
nuschl-video-worker
```

These packages use software AV1 encoding and decoding for native video calls.
Screen and camera capture use the desktop session's PipeWire portals. The
Linux Nix package also includes VA-API hardware acceleration.

`/usr/bin/nuschl` links to the executable. The stage adds the desktop entry,
which registers `mumble://` and `nuschl://`, the hicolor icons, the AppStream
file, the license notices in `/usr/share/licenses/nuschl/` and the release's commit
in `/usr/share/nuschl/commit`. The in-app updater is off on Linux; APT and pacman
update the package.

The sandbox runs the `bwrap` beside the executable before the one on the `PATH`.
In the stage it is a launcher script that runs `/usr/bin/bwrap`, and the
AppArmor profile `/etc/apparmor.d/nuschl-bwrap` attaches to it: Ubuntu 23.10 and
later give user namespaces of programs without a profile no capabilities. The
profile allows user namespaces and leaves everything else unconfined
(`flags=(unconfined)`); bubblewrap and the workers inherit it, and bubblewrap's
`--disable-userns` leaves the workers no user namespace to create. It needs
AppArmor 4.0 (Debian 13, Ubuntu 24.04). The `.deb` marks it as a conffile, its
postinst loads it where `aa-enabled` reports AppArmor active, and its postrm
unloads it on purge.

`Depends` names glibc 2.17 (from the newest glibc symbol), the libraries the
desktop and its workers link or open (libxcb, libxkbcommon, Vulkan,
PipeWire 0.3.53 with its modules, ALSA), bubblewrap 0.8 and util-linux for the
sandbox, and the desktop and icon cache tools. Wayland and EGL are
`Recommends`. `nuschl-bin` depends on the matching Arch packages. The version is
the workspace version with revision `1`; packages are published from release tags.

## Portable binaries

`nix build .#linux-portable` and `nix build .#server-static` (x86_64) build with
cargo-zigbuild. Every Rust crate and C dependency, including SQLite, ring, the
compiler runtime and the unwinder, is linked statically.

- `linux-portable` has the programs above plus `nuschl-cli` and `nuschl-tui`
  side by side in `lib/nuschl/`, links to the three clients in `bin/`, the desktop
  entry, icons, licences and `share/nuschl/commit`.
- `server-static` has `nuschld` and its storage worker in `bin/`, and the music bot
  in `bin/` with its codec and content workers in `libexec/nuschl-musicbot/`.

`nuschl`, `nuschl-media-worker`, and `nuschl-video-worker` link glibc dynamically
with symbol versions up to `GLIBC_2.17` and interpreter
`/lib64/ld-linux-x86-64.so.2`. They load these
host libraries by soname:

| Program | Linked | Opened at runtime |
| --- | --- | --- |
| `nuschl` | `libxcb.so.1`, `libxkbcommon.so.0`, `libxkbcommon-x11.so.0` | `libvulkan.so.1` and its ICDs, `libwayland-client.so.0`, `libEGL.so.1` |
| `nuschl-media-worker` | `libpipewire-0.3.so.0` (0.3.53 or later), `libasound.so.2` | PipeWire, PulseAudio and ALSA plugins of those libraries |
| `nuschl-video-worker` | `libpipewire-0.3.so.0` | PipeWire plugins |

All other programs are static musl executables. The build fails when a program
needs another library, a newer glibc symbol, another interpreter, or names a Nix
store path.

Host requirements beyond glibc 2.17: PipeWire 0.3.53 for the media worker, whose
CPAL backend calls `pw_stream_get_time_n`, and a bubblewrap with `--clearenv` and
`--disable-userns` (0.8.0) for every worker. Debian 11's own bubblewrap 0.4.1 and PipeWire 0.3.19 fail these
checks and the workers fail closed; with bubblewrap 0.8.0 and PipeWire 0.3.65 from
`bullseye-backports` they start. Workers run as `/worker` in their sandbox and find
the interpreter and host libraries in its read-only `/usr/lib`, `/lib` and `/lib64`.

In `nuschld`, the CLI, the TUI, the music bot and the transfer worker, musl
resolves names from `/etc/hosts` and the servers in `/etc/resolv.conf` only, with
`search`, `ndots` and TCP retries for truncated answers. NSS sources such as mDNS,
LDAP or SSSD reach them only through a DNS server, for example systemd-resolved's
stub at `127.0.0.53`. Link previews resolve in the desktop process through the
host's glibc and NSS.

The musl programs use musl's allocator. A chat workload of eight clients costs
`nuschld` about 11% more user-space instructions than the glibc build with the
same profile.

Before publication the notices also need musl's licence and those of zig's
compiler-rt, libunwind and glibc startup objects.

## Tests

The checks `distro-debian`, `distro-ubuntu` and `distro-arch`
(`packaging/linux/tests.nix`, running `install-test.sh`) install
the packages on clean systems: Debian 13 and Ubuntu 24.04 images from nixpkgs'
`vmTools`, and an Arch root made from one day of the Arch Linux Archive. Each
installs the package, runs `nuschl --version` and starts the RTC and video
workers with the Nix store hidden, checks `xdg-mime query default` for both URL
schemes, and renders a message with
`nuschl render` as an unprivileged user, which runs the content worker in its
sandbox; the same command fails without `prlimit` on the `PATH`. In the content
worker's sandbox, `unshare --user` fails. Debian and
Ubuntu install from the signed test repository with apt, upgrade to the next
revision, remove and purge. Arch installs `nuschl-bin` with `pacman -U` and
removes it.

The Ubuntu test boots Ubuntu 24.04's own kernel (`apparmor-test.sh` as PID 1)
with Ubuntu's AppArmor policy loaded and
`kernel.apparmor_restrict_unprivileged_userns=1`, Ubuntu's default since 23.10,
and checks that an unprivileged user namespace gets no capabilities. After the
first installation it unloads the `nuschl-bwrap` profile and checks that the
sandbox then fails closed; the upgrade loads the profile again, and the purge
deletes and unloads it.

Building the AUR package also checks that makepkg refuses the `.deb` with its
signature missing, with a signature by another key, and with modified contents.

## Publishing

The APT repository and the AUR package use a dedicated OpenPGP archive key.
The operator signs the archive locally and uploads the signed files over SSH.

### The download host (once)

Point `dl.nuschl.eu` at the download host and add the Nuschl module to its
NixOS configuration:

```nix
{
  imports = [ inputs.nuschl.nixosModules.apt ];
  services.nuschl-apt.enable = true;
}
```

The host needs the usual `security.acme` account settings and incoming TCP
80 and 443 for HTTP-01 and HTTPS. The module serves `/apt/` from
`/srv/dl.nuschl.eu/apt` by default. Create the directory with write access for
the SSH upload account and read and traversal access for nginx.

### The archive key (once)

```sh
gpg --quick-generate-key 'Nuschl packages <packages@nuschl.eu>' ed25519 sign 3y
fpr=$(gpg --with-colons --list-keys packages@nuschl.eu | awk -F: '$1 == "fpr" { print $10; exit }')
gpg --armor --export "$fpr" > packaging/nuschl-archive-keyring.asc
```

Commit `packaging/nuschl-archive-keyring.asc` in a pull request and add the
fingerprint to install.md. The flake output `aur` exists once that file does.
Extending the key's expiry later keeps its fingerprint: `gpg --quick-set-expire "$fpr" 3y`,
then export and commit the public key again.

### A release

Run `just release` to stage the exact-tag Debian package with the other
artifacts, then confirm publication with `just release --publish`.

Official releases default to `root@dl.nuschl.eu:/srv/dl.nuschl.eu/apt`. Set
`NUSCHL_APT_DESTINATION` to override the destination, for example with an SSH alias.

The publisher uses the fingerprint checked into `packaging/nuschl-archive-keyring.asc`.
Its private key must be available to GnuPG in the release shell. Keep the local
archive at `${XDG_CACHE_HOME:-~/.cache}/nuschl-release/apt-archive` across releases.
The archive contains the previous packages and indices alongside the new ones.

`apt-repository` signs the Debian package and the indices for `stable`,
`trixie`, and `noble`. Rsync uploads the pool and indices addressed by hash
before uploading the signed metadata that references them. Publication checks
the remote Git tag first, then uploads APT, then creates the GitHub release.
A failed APT upload can be retried using the same signed release stage.

Official releases also update the AUR. Set up an
[AUR account and SSH key](https://wiki.archlinux.org/title/AUR_submission_guidelines#Authentication)
with access to `nuschl-bin`, and configure your Git name and email. The publisher
builds the AUR metadata from the release tag, verifies the staged package checksum
and archive key, and pushes the update after uploading APT to the host that
serves `dl.nuschl.eu`. If the AUR push fails, rerun publication with the same
signed stage. An identical AUR update is skipped on retry.

### Re-signing

Clients reject the repository once `Valid-Until` passes. At least every 30 days,
re-sign it and upload the indexes:

```sh
archive="${XDG_CACHE_HOME:-$HOME/.cache}/nuschl-release/apt-archive"
nix run .#apt-repository -- "$archive" "$fpr"
rsync -rt --delay-updates "$archive/dists" "${NUSCHL_APT_DESTINATION:-root@dl.nuschl.eu:/srv/dl.nuschl.eu/apt}/"
```
