# The .deb: the stage plus a control file. Its maintainer scripts load the
# AppArmor profile and unload it on purge; the dpkg triggers of
# desktop-file-utils and hicolor-icon-theme refresh the caches.
# The portable build checks each program's libraries, interpreter and glibc
# symbols; libc6 comes from the newest glibc symbol.
{
  pkgs,
  stage,
  version,
  maintainer,
  depends,
  recommends,
}:
pkgs.runCommand "nuschl_${version}_amd64.deb"
  {
    nativeBuildInputs = [
      pkgs.dpkg
      pkgs.binutils
    ];
  }
  ''
    cp -R ${stage} root
    chmod -R u+w root
    glibc=$(readelf -VW root/usr/lib/nuschl/nuschl* | grep -o 'GLIBC_2\.[0-9.]*' | sort -uV | tail -n 1)
    mkdir root/DEBIAN
    install -m755 ${./postinst} root/DEBIAN/postinst
    install -m755 ${./postrm} root/DEBIAN/postrm
    echo /etc/apparmor.d/nuschl-bwrap > root/DEBIAN/conffiles
    cat > root/DEBIAN/control <<EOF
    Package: nuschl
    Version: ${version}
    Architecture: amd64
    Maintainer: ${maintainer}
    Installed-Size: $(du -sk root | cut -f1)
    Depends: libc6 (>= ''${glibc#GLIBC_}), ${pkgs.lib.concatStringsSep ", " depends}
    Recommends: ${pkgs.lib.concatStringsSep ", " recommends}
    Section: net
    Priority: optional
    Homepage: https://github.com/nuschl-eu/nuschl
    Description: Voice and text chat for Mumble servers
     Desktop client with sandboxed helpers for content, cryptography, storage and media.
    EOF
    dpkg-deb --root-owner-group -Zxz --build root "$out"
  ''
