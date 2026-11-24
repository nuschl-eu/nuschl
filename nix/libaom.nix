# SPDX-License-Identifier: EUPL-1.2
# libaom for the video worker's software encoder (nuschl-aom, SECURITY.md):
# the AV1 encoder only, in real-time mode, at 8 bits, without VMAF tuning,
# examples or tools. A static library: the worker links it into its own
# executable, apart from the libaom.so.3 that PipeWire's FFmpeg plugin loads.
{ pkgs }:
(pkgs.libaom.override { enableVmaf = false; }).overrideAttrs (old: {
  cmakeFlags = old.cmakeFlags ++ [
    "-DBUILD_SHARED_LIBS=OFF"
    "-DCONFIG_AV1_DECODER=0"
    "-DCONFIG_REALTIME_ONLY=1"
    "-DCONFIG_AV1_HIGHBITDEPTH=0"
    "-DENABLE_EXAMPLES=0"
    "-DENABLE_TOOLS=0"
    "-DENABLE_DOCS=0"
  ];
  postFixup = "";
  outputs = [
    "out"
    "dev"
  ];
  meta = old.meta // {
    outputsToInstall = [ "out" ];
  };
})
