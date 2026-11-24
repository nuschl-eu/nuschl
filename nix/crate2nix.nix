# Per-crate Nix builds (crate2nix): every crate is its own derivation, so a
# change rebuilds only the crates that depend on it, and unchanged crates come
# from the store. `nix/crate2nix/Cargo.nix` is generated from Cargo.lock; run
# `just crate2nix` after changing dependencies (the flake checks it is current).
{
  pkgs,
  toolchain,
}:
let
  inherit (pkgs) lib;
  # Read by the desktop's build script (apps/desktop/build.rs).
  emojiAssets = import ./emoji.nix { inherit pkgs; };
  pkgConfig = [ pkgs.pkg-config ];
  libaom = import ./libaom.nix { inherit pkgs; };
  # Libraries the final binaries link against (and whose -sys crates probe).
  linuxLibraries = [
    pkgs.alsa-lib
    pkgs.pipewire
    pkgs.fontconfig
    pkgs.freetype
    pkgs.libxkbcommon
    pkgs.wayland
    pkgs.libxcb
    pkgs.libx11
  ];
  withLibraries = _: {
    nativeBuildInputs = pkgConfig;
    buildInputs = linuxLibraries;
  };
  bindgen = _: {
    nativeBuildInputs = pkgConfig ++ [ pkgs.rustPlatform.bindgenHook ];
    buildInputs = [ pkgs.pipewire ];
  };
  videoCodec = _: {
    CARGO_CFG_TARGET_ARCH = pkgs.stdenv.hostPlatform.parsed.cpu.name;
    CARGO_CFG_TARGET_OS = pkgs.stdenv.hostPlatform.parsed.kernel.name;
    CARGO_ENCODED_RUSTFLAGS = "";
  };
  gst = pkgs.gst_all_1;
  gstLibraries = [
    pkgs.glib
    gst.gstreamer
    gst.gst-plugins-base
    # gstreamer-allocators-1.0.pc requires libdrm.
    pkgs.libdrm
  ];
  withGstreamer = _: {
    nativeBuildInputs = pkgConfig;
    buildInputs = gstLibraries;
  };
  overrides = pkgs.defaultCrateOverrides // {
    # These build scripts read Cargo's target environment directly.
    av-scenechange = videoCodec;
    glib-sys = withGstreamer;
    gio-sys = withGstreamer;
    gobject-sys = withGstreamer;
    gstreamer-sys = withGstreamer;
    gstreamer-base-sys = withGstreamer;
    gstreamer-app-sys = withGstreamer;
    gstreamer-video-sys = withGstreamer;
    gstreamer-allocators-sys = withGstreamer;
    rav1e = videoCodec;
    pipewire-sys = bindgen;
    libspa-sys = bindgen;
    # Generates its libva bindings with bindgen and links libva and libva-drm.
    cros-libva = _: {
      nativeBuildInputs = pkgConfig ++ [ pkgs.rustPlatform.bindgenHook ];
      buildInputs = [ pkgs.libva ];
    };
    # Generates its libaom bindings with bindgen and links libaom.
    nuschl-aom = _: {
      nativeBuildInputs = pkgConfig ++ [ pkgs.rustPlatform.bindgenHook ];
      buildInputs = [ libaom ];
    };
    # Their build scripts probe the PipeWire version with pkg-config.
    pipewire = bindgen;
    libspa = bindgen;
    yeslogic-fontconfig-sys = withLibraries;
    freetype-sys = withLibraries;
    wayland-sys = withLibraries;
    xkbcommon = withLibraries;
    # prost-build compiles the Mumble protocol definitions.
    nuschl-protocol = _: {
      nativeBuildInputs = [ pkgs.protobuf ];
      PROTOC = "${pkgs.protobuf}/bin/protoc";
    };
    # Server integration tests read the unchanged workspace import fixture.
    # Isolated crate sources do not contain the repository's tests directory.
    nuschld = _: {
      NUSCHL_TEST_MURMUR_FIXTURE = "${../tests/fixtures/murmur-1.5.json}";
    };
    # The probe embeds the actual workspace fixture; isolated crate sources do
    # not contain ../../../tests. Keep identical frame bytes as an explicit input.
    nuschl-rtc-probe = _: {
      postPatch = ''
        substituteInPlace src/main.rs --replace-fail \
          '"../../../tests/fixtures/rtc-keyframe.vp8"' \
          '"${../tests/fixtures/rtc-keyframe.vp8}"'
      '';
    };
    # The Murmur tests read the workspace fixtures; isolated crate sources do
    # not contain ../../tests.
    nuschl-storage = _: {
      postPatch = ''
        substituteInPlace src/murmur.rs --replace-fail \
          '"../../tests/fixtures"' '"${../tests/fixtures}"'
      '';
    };
    nuschl = attrs: withLibraries attrs // { NUSCHL_EMOJI_ASSETS = emojiAssets; };
    nuschl-media-worker = withLibraries;
    nuschl-video-worker = attrs: {
      # The encoder tests' screen source; isolated crate sources do not
      # contain ../../docs.
      postPatch = ''
        substituteInPlace src/encoder_tests.rs --replace-fail \
          '"../../docs/images/nuschl.png"' '"${../docs/images/nuschl.png}"'
      '';
      nativeBuildInputs = pkgConfig;
      buildInputs = [
        pkgs.pipewire
      ]
      ++ lib.optionals (builtins.elem "vaapi" (attrs.features or [ ])) (gstLibraries ++ [ pkgs.libva ]);
      NUSCHL_VIDEO_GST_PLUGINS = lib.concatStringsSep ":" [
        "${lib.getLib gst.gstreamer}/lib/gstreamer-1.0/libgstcoreelements.so"
        "${gst.gst-plugins-base}/lib/gstreamer-1.0/libgstapp.so"
        "${gst.gst-plugins-bad}/lib/gstreamer-1.0/libgstva.so"
        "${gst.gst-plugins-bad}/lib/gstreamer-1.0/libgstvideoparsersbad.so"
        "${pkgs.pipewire}/lib/gstreamer-1.0/libgstpipewire.so"
      ];
    };
    # The cli's e2ee tests exec the crypto worker beside the cli binary the
    # harness records at test-compile time (CARGO_BIN_EXE_nuschl-cli); the
    # merged dev dependency nuschld marks that compilation, and the worker
    # joins its bin directory.
    nuschl-cli =
      attrs:
      withLibraries attrs
      //
        lib.optionalAttrs
          (lib.any (d: lib.hasInfix "rust_nuschld" (d.name or (toString d))) (attrs.dependencies or [ ]))
          {
            postInstall = (attrs.postInstall or "") + ''
              cp ${project.workspaceMembers.nuschl-crypto-worker.build}/bin/nuschl-crypto-worker \
                $out/bin/nuschl-crypto-worker
            '';
          };
  };
  project = import ./crate2nix/Cargo.nix {
    inherit pkgs;
    buildRustCrateForPkgs =
      cratePkgs:
      cratePkgs.buildRustCrate.override {
        rustc = toolchain;
        cargo = toolchain;
      };
    defaultCrateOverrides = overrides;
  };
in
{
  inherit project;
  # Export the cached test ELF for VM scenarios that have their own fixtures.
  sessionTest =
    (project.workspaceMembers.nuschl-session.build.override {
      runTests = true;
      testPreRun = ''
        case "$f" in
          */nuschl_session-*)
            test ! -e "$out/bin/nuschl-session-test"
            install -Dm0555 "$f" "$out/bin/nuschl-session-test"
            ;;
        esac
        continue
      '';
    }).test.overrideAttrs
      { dontFixup = true; };
  # The programs of the desktop package (with the terminal client), each
  # built from cached crates.
  binaries = map (member: project.workspaceMembers.${member}.build) [
    "nuschl"
    "nuschl-cli"
    "nuschl-content-worker"
    "nuschl-crypto-worker"
    "nuschl-math-worker"
    "nuschl-media-worker"
    "nuschl-storage-worker"
    "nuschl-transfer-worker"
    "nuschl-tui"
    "nuschld"
    "nuschl-witness"
  ];
}
