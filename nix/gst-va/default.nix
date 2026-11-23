# The VA plugin of gst-plugins-bad with the patches the video worker needs,
# built from the same source as nixpkgs' gst-plugins-bad (SECURITY.md).
{ pkgs }:
let
  bad = pkgs.gst_all_1.gst-plugins-bad;
in
pkgs.stdenv.mkDerivation {
  pname = "gst-plugins-bad-va";
  inherit (bad) version src;
  patches = [ ./coded-buffer.patch ];
  postPatch = "patchShebangs --build scripts";
  strictDeps = true;
  nativeBuildInputs = [
    pkgs.meson
    pkgs.ninja
    pkgs.pkg-config
    pkgs.python3
    pkgs.glib
  ];
  buildInputs = [
    pkgs.gst_all_1.gstreamer
    pkgs.gst_all_1.gst-plugins-base
    pkgs.libva
    pkgs.libdrm
    pkgs.libgudev
  ];
  mesonFlags = [
    "-Dauto_features=disabled"
    "-Dva=enabled"
  ];
}
