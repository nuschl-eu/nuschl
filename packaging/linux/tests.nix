# Clean-system tests of the packages. Debian 13 and Ubuntu 24.04 install the
# .deb from the signed test repository with apt, upgrade, remove and purge it;
# Ubuntu runs on its own kernel with its AppArmor policy and user namespace
# restriction. Arch installs nuschl-bin with pacman into a root made from
# packages of the Arch Linux Archive, and removes it.
{
  pkgs,
  aptRepository,
  aur,
  archDepends,
  version,
  revision,
  depends,
}:
let
  inherit (pkgs) lib vmTools;
  # The installed and the upgraded package version.
  versions = "${version}-${revision} ${version}-${toString (lib.toInt revision + 1)}";
  image =
    distro:
    vmTools.diskImageFuns.${distro} {
      size = 8192;
      # The package's dependencies come preinstalled, the test repository holds only Nuschl.
      extraPackages =
        lib.optionals (distro == "ubuntu2404x86_64") ubuntuKernel
        ++ map (dependency: lib.head (lib.splitString " " dependency)) depends
        ++ [
          "apt"
          "xdg-utils"
        ];
    };
  # Ubuntu's kernel restricts unprivileged user namespaces through AppArmor.
  # vmTools boots the nixpkgs kernel, so the Ubuntu test boots Ubuntu's, with
  # the test repository in the image.
  ubuntuKernel = [
    "linux-image-virtual"
    "initramfs-tools"
    "apparmor"
  ];
  ubuntuImage = vmTools.runInLinuxImage (
    pkgs.runCommand "nuschl-ubuntu-image"
      {
        diskImage = image "ubuntu2404x86_64";
        memSize = 4096;
        postVM = ''mv disk-image.qcow2 "$out/"'';
      }
      ''
        cp -R ${aptRepository.test} /repository
        install -m755 ${./apparmor-test.sh} /apparmor-test
        install -m644 ${./install-test.sh} /install-test.sh
        mkdir "$out"
        cp /boot/vmlinuz-* "$out/vmlinuz"
        cp /boot/initrd.img-* "$out/initrd"
      ''
  );
  ubuntu =
    pkgs.runCommand "nuschl-install-ubuntu2404x86_64"
      {
        nativeBuildInputs = [ pkgs.qemu_kvm ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        qemu-img create -f qcow2 -F qcow2 -b ${ubuntuImage}/disk-image.qcow2 disk.qcow2
        # The arguments after -- go to apparmor-test.sh, which passes them to install-test.sh.
        timeout 900 qemu-kvm -m 4096 -smp 2 -nographic -no-reboot -drive file=disk.qcow2,if=virtio \
          -kernel ${ubuntuImage}/vmlinuz -initrd ${ubuntuImage}/initrd \
          -append "root=/dev/vda rw console=ttyS0 init=/apparmor-test panic=-1 -- apt /repository noble ${versions}" \
          | tee log
        grep -q "^install test passed" log
        touch "$out"
      '';
  apt =
    distro: suite:
    vmTools.runInLinuxImage (
      pkgs.runCommand "nuschl-install-${distro}"
        {
          diskImage = image distro;
          memSize = 4096;
        }
        ''
          env -i PATH=$PATH sh ${./install-test.sh} apt ${aptRepository.test} ${suite} ${versions}
          touch "$out"
        ''
    );
  # nuschl-bin's dependencies and xdg-utils from one day of the archive; the
  # output hash pins the packages.
  archPackages =
    pkgs.runCommand "nuschl-arch-packages"
      {
        nativeBuildInputs = [
          pkgs.pacman
          pkgs.fakeroot
        ];
        NIX_SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        outputHashMode = "recursive";
        outputHash = "sha256-40L+RLLTi1N11KhVMy2vwhPj424jtf/iKTTEwy3c4XM=";
      }
      ''
        cat > pacman.conf <<'EOF'
        [options]
        Architecture = x86_64
        SigLevel = Never
        [core]
        Server = https://archive.archlinux.org/repos/2026/11/15/$repo/os/$arch
        [extra]
        Server = https://archive.archlinux.org/repos/2026/11/15/$repo/os/$arch
        EOF
        mkdir -p db root "$out"
        fakeroot pacman --config pacman.conf --dbpath db --root root --cachedir "$out" --logfile pacman.log \
          --sync --refresh --downloadonly --noconfirm base xdg-utils \
          ${lib.escapeShellArgs archDepends}
      '';
  # A disk image: the kernel refuses user namespaces in a chroot.
  archImage = vmTools.runInLinuxVM (
    pkgs.runCommand "arch-image"
      {
        nativeBuildInputs = [ pkgs.pacman ];
        preVM = vmTools.createEmptyImage {
          size = 4096;
          fullName = "Arch Linux";
        };
        memSize = 2048;
      }
      ''
        ${vmTools.defaultCreateRootFS}
        # pacman's hooks run in the Arch root with this environment.
        export PATH=$PATH:/usr/bin
        unset XDG_DATA_DIRS
        mkdir -p /mnt/var/lib/pacman
        printf '%s\n' '[options]' 'Architecture = x86_64' 'SigLevel = Never' > pacman.conf
        pacman --config pacman.conf --root /mnt --dbpath /mnt/var/lib/pacman --upgrade --noconfirm \
          ${archPackages}/*.pkg.tar.zst
        rm /mnt/.debug
      ''
  );
  arch = vmTools.runInLinuxImage (
    pkgs.runCommand "nuschl-install-arch"
      {
        diskImage = archImage;
        memSize = 4096;
      }
      ''
        env -i PATH=$PATH sh ${./install-test.sh} pacman ${aur}/nuschl-bin-*.pkg.tar.zst
        touch "$out"
      ''
  );
in
{
  inherit arch ubuntu;
  debian = apt "debian13x86_64" "stable";
}
