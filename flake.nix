{
  description = "Nuschl development, desktop, server, and worker builds";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
    # Only for checking the Home Manager module; it does not use this input,
    # so consumers bring their own Home Manager.
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Older stock servers for interoperability tests only (legacy UDP format).
    nixpkgs-mumble14.url = "github:NixOS/nixpkgs/nixos-22.11";
    nixpkgs-mumble13.url = "github:NixOS/nixpkgs/nixos-21.05";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      advisory-db,
      home-manager,
      nixpkgs-mumble14,
      nixpkgs-mumble13,
      ...
    }:
    {
      nixosModules.default = import ./nix/module.nix { inherit self; };
      # The web host: web client, share links and the tunnel relay (docs/web.md).
      nixosModules.web = import ./nix/web-module.nix { inherit self; };
      nixosModules.apt = ./nix/apt-module.nix;
      # The key-transparency auditor (docs/witness.md).
      nixosModules.witness = import ./nix/witness-module.nix { inherit self; };
      # The Mumble music bot for Subsonic servers (docs/musicbot.md).
      nixosModules.musicbot = import ./nix/musicbot-module.nix { inherit self; };
      # Screen sharing: the TURN relay and the stream host (#143).
      nixosModules.turn = ./nix/turn-module.nix;
      nixosModules.stream = import ./nix/stream-module.nix { inherit self; };
      homeModules.default = import ./nix/home-manager.nix { inherit self; };
      homeManagerModules.default = self.homeModules.default;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        inherit (pkgs) lib;
        # flake.lock pins rust-overlay, including what it considers latest stable.
        toolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" ];
          targets = [ "wasm32-unknown-unknown" ];
        };
        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
        # Only for the libFuzzer targets in fuzz/ (`nix develop .#fuzz`, `just fuzz`):
        # the newest nightly in the pinned rust-overlay that has these components.
        # llvm-tools are for `cargo fuzz coverage`. Builds and checks stay on stable.
        fuzzToolchain = pkgs.rust-bin.selectLatestNightlyWith (
          toolchain:
          toolchain.minimal.override {
            extensions = [
              "rust-src"
              "llvm-tools-preview"
            ];
          }
        );
        # Miri for nuschl-sandbox-windows' unsafe code (docs/windows-sandbox.md).
        miriToolchain = pkgs.rust-bin.selectLatestNightlyWith (
          toolchain:
          toolchain.minimal.override {
            extensions = [
              "rust-src"
              "miri"
            ];
          }
        );
        src = lib.cleanSourceWith {
          src = ./.;
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            # Trusted precompiled syntax bundle; no runtime grammar files.
            || lib.hasSuffix "/crates/content/assets/default-nix.packdump" path
            # The content-WASM integration test's fixed, tracked PNG.
            || lib.hasSuffix "/crates/content-wasm/tests/fixtures/rgba-2x2.png" path
            || lib.hasSuffix ".proto" path
            || lib.hasSuffix ".sql" path
            || lib.hasSuffix ".json" path
            || lib.hasSuffix ".vp8" path
            || lib.hasSuffix "/apps/rtc-worker/tests/fixtures/screen.av1" path
            # The SFU's replay tests read recorded Chromium streams.
            || lib.hasInfix "/apps/stream-sfu/tests/fixtures/" path
            || lib.hasSuffix ".svg" path
            # The Windows icon and the resource script embed-resource compiles.
            || lib.hasSuffix ".ico" path
            || lib.hasSuffix ".rc" path
            # The math worker's bundled font (crates/math/fonts).
            || lib.hasSuffix ".otf" path
            # Frozen identity vectors are compiled into the all-target tests.
            || lib.hasInfix "/test-vectors/" path
            # Vendored GPUI crates keep their shaders, headers and README.
            || lib.hasInfix "/vendor/gpui-pre" path
            # Vendored libraries also include their README in rustdoc. Cargo's
            # source filter does not retain mixed-case names such as Readme.md.
            || (lib.hasInfix "/vendor/" path && lib.hasSuffix ".md" path)
            || lib.hasPrefix "LICENSE" (builtins.baseNameOf path);
        };
        nativeBuildInputs = [
          pkgs.pkg-config
          pkgs.protobuf
          pkgs.cmake
          pkgs.clang
          pkgs.makeWrapper
        ];
        buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
          pkgs.alsa-lib
          pkgs.pipewire
          pkgs.libpulseaudio
          pkgs.fontconfig
          pkgs.freetype
          pkgs.libxkbcommon
          pkgs.wayland
          pkgs.vulkan-loader
          pkgs.libGL
          pkgs.libx11
          pkgs.libxcursor
          pkgs.libxi
          pkgs.libxrandr
          pkgs.libxcb
          # The video worker's VA-API encoder (feature vaapi).
          pkgs.libva
          # The video worker's software encoder (nuschl-aom, feature aom).
          (import ./nix/libaom.nix { inherit pkgs; })
        ];
        # Twemoji images and emojibase names for both clients; the sources
        # are pinned together in nix/emoji.nix.
        emojiAssets = import ./nix/emoji.nix { inherit pkgs; };
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
        # The commit the shipped packages record for their programs' version
        # (crates/version): `7884c04`, or `7884c04-dirty` for a dirty tree.
        commit = self.shortRev or self.dirtyShortRev or null;
        # A copy of a package with that commit in `directory`/commit. Only these
        # copies change with each commit; checks and VM tests use the packages
        # without the file.
        withCommit =
          directory: drv:
          if commit == null then
            drv
          else
            pkgs.runCommand drv.name { meta = drv.meta or { }; } ''
              cp -R ${drv} $out
              chmod -R u+w $out
              # Wrappers name the package they are in.
              for file in $(grep -rlIF ${drv} $out || true); do
                substituteInPlace "$file" --replace-fail ${drv} $out
              done
              mkdir -p "$out/${directory}"
              echo ${commit} > "$out/${directory}/commit"
            '';
        common = {
          pname = "nuschl";
          inherit version;
          inherit src nativeBuildInputs buildInputs;
          strictDeps = true;
          cargoExtraArgs = "--workspace";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          # Compiled into the desktop by apps/desktop/build.rs.
          NUSCHL_EMOJI_ASSETS = emojiAssets;
        };
        # Registry CPAL uses this patched path dependency. Its real API must
        # survive Crane's dummy-source pass so CPAL can compile against it.
        pulseSource = lib.cleanSourceWith {
          src = ./vendor/pulseaudio;
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            || lib.hasPrefix "LICENSE" (builtins.baseNameOf path)
            || lib.hasSuffix ".md" path;
        };
        dummySrc = craneLib.mkDummySrc {
          inherit src;
          extraDummyScript = ''
            rm -rf "$out/vendor/pulseaudio"
            cp -R ${pulseSource} "$out/vendor/pulseaudio"
            chmod -R u+w "$out/vendor/pulseaudio"
            # GPUI compiles against these patched crates during the dependency pass.
            for crate in gpui-pre gpui-pre-windows gpui-pre-apple gpui-pre-wgpu gpui-pre-linux str0m-rust-crypto ratatui-image; do
              rm -rf "$out/vendor/$crate"
              cp -R ${./vendor}/$crate "$out/vendor/$crate"
              chmod -R u+w "$out/vendor/$crate"
            done
          '';
        };
        # Artifact-only installation never executes these final-product hooks.
        # Keeping their source/notice paths in the dependency derivation would
        # rebuild dependencies after an otherwise harmless application edit.
        dependencyArgs =
          args:
          builtins.removeAttrs args [
            "src"
            "preInstall"
            "postInstall"
            "installPhase"
            "installPhaseCommand"
            # Only the desktop's own build script reads it.
            "NUSCHL_EMOJI_ASSETS"
          ];
        buildDeps = args: craneLib.buildDepsOnly (dependencyArgs args // { inherit dummySrc; });
        buildCrossPackage =
          builder: args:
          builder.buildPackage (
            args
            // {
              cargoArtifacts = builder.buildDepsOnly (
                dependencyArgs args
                // {
                  inherit dummySrc;
                  # Match Crane's implicit dependency pass; final installation still
                  # uses every hook from args. Compile/check/preBuild inputs survive.
                  installPhase = "prepareAndInstallCargoArtifactsDir";
                }
              );
            }
          );
        cargoArtifacts = buildDeps common;
        musicbotWeb = import ./nix/musicbot-web.nix { inherit pkgs lib; };
        webClient = import ./nix/web-client.nix {
          inherit
            pkgs
            lib
            toolchain
            emojiAssets
            ;
          releaseNotices = releaseNotices.web.bundle;
        };
        releaseNotices = import ./nix/release-notices.nix {
          inherit pkgs toolchain craneLib;
          projectRoot = ./.;
          webRuntimeNotices = "${webClient.rawApp}/licenses";
          musicbotRuntimeNotices = "${musicbotWeb}/licenses/npm";
        };
        nativeNoticeScope =
          if pkgs.stdenv.hostPlatform.isDarwin then
            "macos-universal"
          else if pkgs.stdenv.hostPlatform.rust.rustcTarget == "aarch64-unknown-linux-gnu" then
            "linux-aarch64-nix"
          else
            "linux-nix";
        nativeNoticeBundle = (releaseNotices.forScope nativeNoticeScope).bundle;
        # Omit unsupported artifact names without evaluating a Linux builder
        # (or forcing pkgs for platforms that nixpkgs no longer supports).
        distroOutput = name: if system == "x86_64-linux" then name else null;
        # Debian and Arch packages: the shipped ones from linux-portable with
        # its commit file, the checks' ones from linux-portable without it.
        linuxDistroWith =
          binaries:
          import ./packaging/linux/default.nix {
            inherit pkgs binaries;
            projectRoot = ./.;
            nativeNotices = releaseNotices.portable.bundle;
            desktopFiles = (import ./nix/desktop-files.nix { inherit pkgs; }).linux;
            appstreamXML = ./packaging/flatpak/org.nuschl.Nuschl.metainfo.xml;
          };
        linuxDistro = linuxDistroWith (withCommit "share/nuschl" linuxPortable);
        distroChecks = linuxDistroWith linuxPortable;
        installNotices = bundle: ''
          mkdir -p "$out/share/licenses/nuschl"
          cp -R ${bundle}/share/licenses/nuschl/. "$out/share/licenses/nuschl/"
        '';
        installNativeNotices = installNotices nativeNoticeBundle;
        witnessPackage =
          pkgs.runCommand "nuschl-witness-${version}" { meta.mainProgram = "nuschl-witness"; }
            ''
              install -Dm755 ${crates.project.workspaceMembers.nuschl-witness.build}/bin/nuschl-witness "$out/bin/nuschl-witness"
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
              ${installNativeNotices}
            '';
        musicbotNoticeScope = "musicbot-${pkgs.stdenv.hostPlatform.rust.rustcTarget}";
        musicbotNotices = releaseNotices.forScope musicbotNoticeScope;
        musicbotPackage =
          pkgs.runCommand "nuschl-musicbot-${version}"
            {
              nativeBuildInputs = [ pkgs.makeWrapper ];
              meta.mainProgram = "nuschl-musicbot";
            }
            ''
              install -Dm755 ${crates.project.workspaceMembers.nuschl-musicbot.build}/bin/nuschl-musicbot "$out/bin/nuschl-musicbot"
              install -Dm755 ${crates.project.workspaceMembers.nuschl-musicbot-codec-worker.build}/bin/nuschl-musicbot-codec-worker "$out/libexec/nuschl-musicbot/nuschl-musicbot-codec-worker"
              install -Dm755 ${crates.project.workspaceMembers.nuschl-content-worker.build}/bin/nuschl-content-worker "$out/libexec/nuschl-musicbot/nuschl-content-worker"
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
              install -Dm644 ${./crates/protocol/proto/LICENSE.mumble} "$out/share/licenses/nuschl/Mumble-BSD"
              mkdir -p "$out/share/licenses/nuschl"
              cp -R ${musicbotNotices.bundle}/share/licenses/nuschl/. "$out/share/licenses/nuschl/"
              wrapProgram "$out/bin/nuschl-musicbot" --set-default NUSCHL_MUSICBOT_WEB ${musicbotWeb} \
                ${lib.optionalString pkgs.stdenv.hostPlatform.isLinux "--prefix PATH : ${
                  lib.makeBinPath [
                    pkgs.bubblewrap
                    pkgs.util-linux
                  ]
                }"}
            '';
        rtcNoticeScope = "rtc-probe-${pkgs.stdenv.hostPlatform.rust.rustcTarget}";
        rtcNotices = releaseNotices.forScope rtcNoticeScope;
        rtcPackage =
          pkgs.runCommand "nuschl-rtc-probe-${common.version}"
            {
              meta.mainProgram = "nuschl-rtc-probe";
            }
            ''
              install -Dm755 ${crates.project.workspaceMembers.nuschl-rtc-probe.build}/bin/nuschl-rtc-probe "$out/bin/nuschl-rtc-probe"
              mkdir -p "$out/share/licenses/nuschl"
              cp -R ${rtcNotices.bundle}/share/licenses/nuschl/. "$out/share/licenses/nuschl/"
            '';
        videoWorker = crates.project.workspaceMembers.nuschl-video-worker.build.override {
          features = [
            "vaapi"
            "aom"
          ];
        };
        videoProbeNoticeScope = "video-probe-${pkgs.stdenv.hostPlatform.rust.rustcTarget}";
        videoProbe =
          pkgs.runCommand "nuschl-video-probe"
            {
              nativeBuildInputs = [ pkgs.makeWrapper ];
              meta.mainProgram = "nuschl-video-probe";
            }
            ''
              mkdir -p "$out/bin"
              cp ${videoWorker}/bin/nuschl-video-{worker,probe} "$out/bin/"
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
              ${installNotices (releaseNotices.forScope videoProbeNoticeScope).bundle}
              wrapProgram "$out/bin/nuschl-video-probe" --prefix PATH : ${
                lib.makeBinPath [
                  pkgs.bubblewrap
                  pkgs.util-linux
                ]
              }
            '';
        serverPackage =
          pkgs.runCommand "nuschld-${common.version}"
            {
              meta.mainProgram = "nuschld";
            }
            ''
              # current_exe() locates the sibling storage worker. Symlinking the
              # server to its original crate output would lose that adjacency.
              install -Dm755 ${crates.project.workspaceMembers.nuschld.build}/bin/nuschld "$out/bin/nuschld"
              install -Dm755 ${crates.project.workspaceMembers.nuschl-storage-worker.build}/bin/nuschl-storage-worker "$out/bin/nuschl-storage-worker"
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
              install -Dm644 ${./crates/protocol/proto/LICENSE.mumble} "$out/share/licenses/nuschl/Mumble-BSD"
              ${installNativeNotices}
            '';
        licenseLayoutWith = notices: ''
          install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
          install -Dm644 ${./crates/protocol/proto/LICENSE.mumble} "$out/share/licenses/nuschl/Mumble-BSD"
          install -Dm644 ${./vendor/pulseaudio/LICENSE.md} "$out/share/licenses/nuschl/pulseaudio-MIT"
          install -Dm644 ${./vendor/cpal/LICENSE} "$out/share/licenses/nuschl/cpal-Apache-2.0"
          install -Dm644 ${./vendor/ratatui-image/LICENSE} "$out/share/licenses/nuschl/ratatui-image-MIT"
          install -Dm644 ${./vendor/ratatui-image/LICENSE-icy_sixel-MIT} "$out/share/licenses/nuschl/sixel-encoder-MIT"
          install -Dm644 ${./vendor/ratatui-image/NOTICE} "$out/share/licenses/nuschl/ratatui-image-NOTICE"
          install -Dm644 ${./crates/math/fonts/GUST-FONT-LICENSE.txt} "$out/share/licenses/nuschl/NewCMMath-GUST-Font-License"
          ${installNotices notices}
        '';
        licenseLayout = licenseLayoutWith nativeNoticeBundle;
        # Licences, and on Linux the desktop binary under libexec (screen readers
        # announce the executable's name, not ".nuschl-wrapped") with wrappers.
        packageLayout =
          licenseLayout
          + lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
            runtimeFlags=(--prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath buildInputs}"
              --prefix PATH : "${
                lib.makeBinPath [
                  pkgs.bubblewrap
                  pkgs.util-linux
                ]
              }")
            mkdir -p "$out/libexec/nuschl"
            mv "$out/bin/nuschl" "$out/libexec/nuschl/nuschl"
            install -Dm755 ${videoWorker}/bin/nuschl-video-worker "$out/bin/nuschl-video-worker"
            install -Dm755 ${crates.project.workspaceMembers.nuschl-rtc-worker.build}/bin/nuschl-rtc-worker "$out/bin/nuschl-rtc-worker"
            for worker in nuschl-content-worker nuschl-crypto-worker nuschl-math-worker nuschl-media-worker nuschl-storage-worker nuschl-transfer-worker nuschl-video-worker nuschl-rtc-worker nuschl-update-verify-ed25519 nuschl-update-verify-mldsa44; do
              test -f "$out/bin/$worker"
              ln -s "../../bin/$worker" "$out/libexec/nuschl/$worker"
            done
            makeWrapper "$out/libexec/nuschl/nuschl" "$out/bin/nuschl" "''${runtimeFlags[@]}"
            wrapProgram "$out/bin/nuschl-cli" "''${runtimeFlags[@]}"
            # The terminal client finds the workers beside it in bin; the
            # wrapper gives their sandbox bubblewrap and the media worker its
            # audio libraries.
            wrapProgram "$out/bin/nuschl-tui" "''${runtimeFlags[@]}" --prefix PATH : "${
              lib.makeBinPath [
                pkgs.wl-clipboard-rs
                pkgs.xclip
                pkgs.wayland-utils
              ]
            }"
            # Desktop entry and hicolor icons; shells match the window by its app id.
            cp -R --no-preserve=mode ${
              (import ./nix/desktop-files.nix { inherit pkgs; }).linux
            }/share/. "$out/share/"
          '';
        # Whole-workspace build (crane): kept for comparison and as a fallback.
        cranePackage = craneLib.buildPackage (
          common
          // {
            inherit cargoArtifacts;
            # Workspace defaults deliberately do not enable verifier engines.
            # Reuse the two individually featured builds rather than combining
            # algorithm dependencies in a single workspace feature set.
            postInstall = ''
              install -Dm755 ${updateEdVerifier}/bin/nuschl-update-verify-ed25519 "$out/bin/nuschl-update-verify-ed25519"
              install -Dm755 ${updateMlVerifier}/bin/nuschl-update-verify-mldsa44 "$out/bin/nuschl-update-verify-mldsa44"
              ${packageLayout}
            '';
          }
        );
        # The desktop package from per-crate derivations (crate2nix): a change
        # rebuilds only the crates that depend on it.
        crates = import ./nix/crate2nix.nix { inherit pkgs toolchain; };
        workerTestCodec = crates.project.workspaceMembers.nuschl-ipc.build.override {
          features = [ "test-codec" ];
        };
        # One `<crate>-unit` check per workspace member, generated from the
        # member list, so a member starts with its unit tests running in
        # `ci`. unitExtras gives a member's leaf the test inputs it needs;
        # unitOptOut moves a member's tests to where they run.
        unitExtras = {
          nuschl-sandbox.testInputs = [
            pkgs.bubblewrap
            pkgs.util-linux
          ];
          nuschl-ipc.testInputs = [
            pkgs.bubblewrap
            pkgs.util-linux
          ];
          nuschl-content-worker.testInputs = [
            pkgs.bubblewrap
            pkgs.util-linux
          ];
          nuschl-musicbot-codec-worker.testInputs = [
            pkgs.bubblewrap
            pkgs.util-linux
          ];
          nuschl-rtc-worker.testInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.bubblewrap
            pkgs.util-linux
          ];
          nuschl-video-worker = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            features = [
              "vaapi"
              "aom"
            ];
            testInputs = [
              pkgs.bubblewrap
              pkgs.util-linux
              pkgs.ffmpeg-headless
              pkgs.pipewire
            ];
          };
          # The math worker's confinement tests spawn sandboxed workers, and
          # the CLI's e2ee tests run the crypto worker through the sandbox.
          nuschl-math-worker.testInputs = [
            pkgs.bubblewrap
            pkgs.util-linux
          ];
          nuschl-cli.testInputs = [
            pkgs.bubblewrap
            pkgs.util-linux
          ];
          nuschl-crypto-worker.testInputs = [
            pkgs.bubblewrap
            pkgs.util-linux
          ];
          # tests/self_state.rs runs a stock Murmur on loopback.
          nuschl-session.testInputs = [
            pkgs.bubblewrap
            pkgs.util-linux
            pkgs.murmur
            crates.project.workspaceMembers.nuschl-crypto-worker.build
            crates.project.workspaceMembers.nuschl-stream-sfu.build
          ]
          ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.coturn
            crates.project.workspaceMembers.nuschl-rtc-worker.build
            videoWorker
          ];
          # The signer's tests need its feature; the default build has none.
          nuschl-update-worker.features = [ "signer" ];
          nuschl-link-preview.testInputs = [ pkgs.python3 ];
          # The notification tests run fake services on a private session bus.
          nuschl.testInputs = [ pkgs.dbus ];
          # One throwaway server in the build directory, on a unix socket
          # there; nothing listens on the network. The ignored checks run too.
          nuschl-storage = {
            testInputs = [ pkgs.postgresql ];
            testCrateFlags = [ "--include-ignored" ];
            testPreRun = ''
              export NUSCHL_PG="$(pwd)/postgres"
              # Runs before each test binary; the first one creates the server.
              fresh=
              if [ ! -d "$NUSCHL_PG" ]; then
                initdb --auth=trust --no-locale --username=postgres \
                  --pgdata="$NUSCHL_PG" > initdb.log 2>&1
                fresh=1
              fi
              pg_ctl --pgdata="$NUSCHL_PG" --log="$NUSCHL_PG/server.log" \
                --options="-k $NUSCHL_PG -c listen_addresses=" --wait start
              [ -z "$fresh" ] || createdb --host="$NUSCHL_PG" --username=postgres nuschl_test
              export NUSCHL_TEST_POSTGRES_URL="postgresql:///nuschl_test?host=$NUSCHL_PG&user=postgres"
            '';
            testPostRun = ''
              pg_ctl --pgdata="$NUSCHL_PG" stop
            '';
          };
          nuschl-storage-worker.testInputs = [ pkgs.postgresql ];
        };
        unitOptOut = {
          # These run on another platform or target.
          nuschl-sandbox-windows = "Windows adapter; the tests run under Miri (`just miri`)";
        };
        unitChecks = lib.mapAttrs' (
          name: member:
          lib.nameValuePair "${name}-unit" (
            member.build.override ({ runTests = true; } // unitExtras.${name} or { })
          )
        ) (removeAttrs crates.project.workspaceMembers (lib.attrNames unitOptOut));
        # Explicit operator/CI tool; never included in client worker payloads.
        updateSigner = crates.project.workspaceMembers."nuschl-update-worker".build.override {
          features = [ "signer" ];
        };
        updateVerifier =
          feature:
          crates.project.workspaceMembers."nuschl-update-worker".build.override {
            features = [ feature ];
          };
        updateEdVerifier = updateVerifier "ed25519";
        updateMlVerifier = updateVerifier "mldsa44";
        # Explicit fixture executable, not part of the distributed payload.
        updateRuntimeFixture = updateVerifier "runtime-fixture";
        package =
          pkgs.runCommand "nuschl-${version}"
            {
              nativeBuildInputs = [ pkgs.makeWrapper ];
              meta.mainProgram = "nuschl";
            }
            ''
              mkdir -p "$out/bin"
              for crate in ${
                lib.concatStringsSep " " (
                  crates.binaries
                  ++ [
                    updateEdVerifier
                    updateMlVerifier
                  ]
                )
              }; do
                for program in "$crate"/bin/*; do
                  cp "$program" "$out/bin/"
                done
              done
              chmod -R u+w "$out/bin"
              ${packageLayout}
            '';
        shipped = withCommit "share/nuschl" package;
        # Cross builds from Linux. A separate toolchain keeps native checks cached.
        crossToolchain =
          p:
          p.pkgsBuildHost.rust-bin.stable.latest.default.override {
            targets = [
              "x86_64-pc-windows-gnu"
              "x86_64-apple-darwin"
              "aarch64-apple-darwin"
            ];
          };
        desktopCrates = "-p nuschl -p nuschl-content-worker -p nuschl-crypto-worker -p nuschl-math-worker -p nuschl-media-worker -p nuschl-storage-worker -p nuschl-transfer-worker -p nuschl-video-worker -p nuschl-rtc-worker";
        crossCommon = {
          inherit version;
          inherit src dummySrc;
          strictDeps = true;
          doCheck = false; # Tests run natively, never under an emulator.
          cargoExtraArgs = desktopCrates;
          nativeBuildInputs = [
            pkgs.protobuf
            pkgs.pkg-config
          ];
          NUSCHL_EMOJI_ASSETS = emojiAssets;
        };
        mingw = pkgs.pkgsCross.mingwW64;
        windowsVerifiers = import ./nix/cross-updaters.nix { inherit pkgs; };
        windowsPackage = buildCrossPackage ((crane.mkLib mingw).overrideToolchain crossToolchain) (
          crossCommon
          // {
            pname = "nuschl-windows";
            __CRANE_EXPORT_CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER = "${pkgs.coreutils}/bin/false";
            CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS = "-C target-feature=+crt-static -L native=${mingw.windows.pthreads}/lib";
            postInstall = ''
              rm -f "$out/bin/nuschl-video-probe.exe"
              cp ${windowsVerifiers.ed25519}/bin/nuschl-update-verify-ed25519.exe "$out/bin/"
              cp ${windowsVerifiers.mldsa44}/bin/nuschl-update-verify-mldsa44.exe "$out/bin/"
              for program in nuschl nuschl-content-worker nuschl-crypto-worker nuschl-math-worker nuschl-media-worker nuschl-storage-worker nuschl-transfer-worker nuschl-video-worker nuschl-rtc-worker nuschl-update-verify-ed25519 nuschl-update-verify-mldsa44; do
                test -f "$out/bin/$program.exe"
              done
              if ${pkgs.binutils}/bin/objdump -p "$out/bin/nuschl.exe" | grep -iE 'DLL Name:.*(libgcc|libstdc|libwinpthread)'; then
                echo "Windows binary depends on a MinGW runtime DLL" >&2
                exit 1
              fi
              install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
              cp -R ${releaseNotices.windows.bundle}/share/licenses/nuschl/. "$out/share/licenses/nuschl/"
            '';
          }
        );
        # Pinned SDK, as in ../opendeck-akp05. 14.x because cpal's CoreAudio
        # backend links the process-tap API (macOS 14.2+), which Rust cannot
        # weak-link; the deployment target follows.
        macSdk = pkgs.fetchzip {
          url = "https://github.com/joseluisq/macosx-sdks/releases/download/14.5/MacOSX14.5.sdk.tar.xz";
          hash = "sha256-YABLNOdte3WE8mnUm32zSlQ3lyDFQNnxXTW9gj51MX0=";
        };
        macMinimum = "14.2";
        # cargo-zigbuild gives zig an unversioned macOS target, for which zig
        # assumes its newest release (15.0) whatever MACOSX_DEPLOYMENT_TARGET says.
        cargoZigbuild = pkgs.cargo-zigbuild.overrideAttrs (old: {
          patches = (old.patches or [ ]) ++ [ ./nix/cargo-zigbuild-macos-version.patch ];
        });
        macBuilder = (crane.mkLib pkgs).overrideToolchain crossToolchain;
        macBuildArgs = arch: {
          nativeBuildInputs = crossCommon.nativeBuildInputs ++ [
            cargoZigbuild
            pkgs.zig
          ];
          CARGO_BUILD_TARGET = "${arch}-apple-darwin";
          cargoBuildCommand = "cargoWithProfile zigbuild --target ${arch}-apple-darwin";
          cargoCheckCommand = "true";
          SDKROOT = macSdk;
          MACOSX_DEPLOYMENT_TARGET = macMinimum;
          preBuild = ''
            export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
            export CARGO_ZIGBUILD_CACHE_DIR="$TMPDIR/zigbuild-cache"
          '';
        };
        macMinimumCheck = ''
          for program in "$out"/bin/*; do
            [ -f "$program" ] || continue
            minimum=$(${pkgs.llvmPackages.llvm}/bin/llvm-otool -l "$program" | sed -n 's/^ *minos //p' | head -1)
            if [ "$minimum" != "${macMinimum}" ]; then
              echo "$program requires macOS $minimum, expected ${macMinimum}" >&2
              exit 1
            fi
          done
        '';
        macVerifier =
          arch: feature:
          buildCrossPackage macBuilder (
            crossCommon
            // macBuildArgs arch
            // {
              pname = "nuschl-macos-${arch}-verify-${feature}";
              cargoExtraArgs = "-p nuschl-update-worker --no-default-features --features nuschl-update-worker/${feature} --bin nuschl-update-verify-${feature}";
              postInstall = ''
                test -f "$out/bin/nuschl-update-verify-${feature}"
                test "$(find "$out/bin" -maxdepth 1 -type f | wc -l)" -eq 1
              ''
              + macMinimumCheck;
            }
          );
        macSlice =
          arch:
          buildCrossPackage macBuilder (
            crossCommon
            // macBuildArgs arch
            // {
              pname = "nuschl-macos-${arch}";
              postInstall = ''
                rm -f "$out/bin/nuschl-video-probe"
                cp ${macVerifier arch "ed25519"}/bin/nuschl-update-verify-ed25519 "$out/bin/"
                cp ${macVerifier arch "mldsa44"}/bin/nuschl-update-verify-mldsa44 "$out/bin/"
                for program in nuschl nuschl-content-worker nuschl-crypto-worker nuschl-math-worker nuschl-media-worker nuschl-storage-worker nuschl-transfer-worker nuschl-video-worker nuschl-rtc-worker nuschl-update-verify-ed25519 nuschl-update-verify-mldsa44; do
                  test -f "$out/bin/$program"
                done
              ''
              + macMinimumCheck;
            }
          );
        macUniversal =
          pkgs.runCommand "nuschl-macos-universal" { nativeBuildInputs = [ pkgs.llvmPackages.llvm ]; }
            ''
              mkdir -p "$out/bin"
              for program in nuschl nuschl-content-worker nuschl-crypto-worker nuschl-math-worker nuschl-media-worker nuschl-storage-worker nuschl-transfer-worker nuschl-video-worker nuschl-rtc-worker nuschl-update-verify-ed25519 nuschl-update-verify-mldsa44; do
                llvm-lipo -create ${macSlice "x86_64"}/bin/$program ${macSlice "aarch64"}/bin/$program \
                  -output "$out/bin/$program"
                llvm-lipo "$out/bin/$program" -verify_arch x86_64 arm64
              done
              if llvm-otool -L "$out/bin/nuschl" | grep -qE '^[[:space:]]+/nix/store/'; then
                echo "macOS binary depends on a Nix store library" >&2
                exit 1
              fi
            '';
        # Nuschl.app for macOS: the universal binaries, Info.plist with the
        # microphone usage text macOS requires, and an ad-hoc signature over
        # the whole bundle (rcodesign, from Linux). Updates replace every file
        # of the bundle, signature included.
        macApp =
          pkgs.runCommand "nuschl-macos-app"
            {
              nativeBuildInputs = [ pkgs.rcodesign ];
              version = crossCommon.version;
            }
            ''
              app="$out/Nuschl.app"
              mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
              cp ${macUniversal}/bin/* "$app/Contents/MacOS/"
              mkdir -p "$app/Contents/Resources/licenses"
              cp -R ${releaseNotices.macos.bundle}/share/licenses/nuschl/. "$app/Contents/Resources/licenses/"
              chmod 755 "$app"/Contents/MacOS/*
              cat > "$app/Contents/Info.plist" <<PLIST
              <?xml version="1.0" encoding="UTF-8"?>
              <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
              <plist version="1.0">
              <dict>
                <key>CFBundleExecutable</key><string>nuschl</string>
                <key>CFBundleIdentifier</key><string>org.nuschl.Nuschl</string>
                <key>CFBundleName</key><string>Nuschl</string>
                <key>CFBundleDisplayName</key><string>Nuschl</string>
                <key>CFBundleIconFile</key><string>Nuschl</string>
                <key>CFBundlePackageType</key><string>APPL</string>
                <key>CFBundleShortVersionString</key><string>$version</string>
                <key>CFBundleVersion</key><string>$version</string>
                <key>LSMinimumSystemVersion</key><string>${macMinimum}</string>
                <key>NSHighResolutionCapable</key><true/>
                <key>NSCameraUsageDescription</key><string>Nuschl shares your camera when you start it in a video call.</string>
                <key>NSScreenCaptureUsageDescription</key><string>Nuschl shares the screen or window you choose in a video call.</string>
                <key>NSMicrophoneUsageDescription</key><string>Nuschl sends your voice to the people in your channel.</string>
                <key>NSAudioCaptureUsageDescription</key><string>Nuschl removes what your speakers play from your microphone, so others do not hear it. The sound stays on this Mac.</string>
              </dict>
              </plist>
              PLIST
              ${lib.optionalString (commit != null) ''echo ${commit} > "$app/Contents/Resources/commit"''}
              install -Dm644 ${./LICENSE} "$app/Contents/Resources/EUPL-1.2"
              install -Dm644 ${
                (import ./nix/desktop-files.nix { inherit pkgs; }).macIcon
              } "$app/Contents/Resources/Nuschl.icns"
              rcodesign sign "$app"
            '';
        # Portable Linux builds (#151, docs/distro-packaging.md): cargo-zigbuild
        # links every crate and C dependency statically. The desktop and the
        # media and video workers link glibc dynamically at the symbol floor below
        # and use the host's X11, xkbcommon, PipeWire and ALSA libraries by soname; GPUI
        # opens Vulkan and Wayland at runtime. Everything else is static musl.
        portableGlibc = "2.17";
        portableBuilder = (crane.mkLib pkgs).overrideToolchain (
          pkgs.rust-bin.stable.latest.default.override { targets = [ "x86_64-unknown-linux-musl" ]; }
        );
        portableBuild =
          target: pname: args:
          let
            triple = lib.head (lib.splitString "." target);
            vendor = portableBuilder.vendorCargoDeps { inherit src; };
          in
          buildCrossPackage portableBuilder (
            crossCommon
            // {
              inherit pname;
              cargoVendorDir = vendor;
              nativeBuildInputs =
                crossCommon.nativeBuildInputs
                ++ [
                  cargoZigbuild
                  pkgs.zig
                ]
                ++ args.nativeBuildInputs or [ ];
              CARGO_BUILD_TARGET = triple;
              cargoBuildCommand = "cargoWithProfile zigbuild --target ${target}";
              cargoCheckCommand = "true";
              # Source paths in panic messages name no store path.
              "CARGO_TARGET_${lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] triple)}_RUSTFLAGS" =
                "--remap-path-prefix=${vendor}=/cargo --remap-path-prefix=/nix/store=/store ${args.rustflags or ""}";
              preBuild = ''
                export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
                export CARGO_ZIGBUILD_CACHE_DIR="$TMPDIR/zigbuild-cache"
              '';
              postInstall = ''
                for program in "$out"/bin/*; do
                  if grep -qaE '/nix/store/[0-9a-z]{32}-' "$program"; then
                    echo "$program names a store path" >&2
                    exit 1
                  fi
                done
              ''
              + args.postInstall;
            }
            // removeAttrs args [
              "nativeBuildInputs"
              "postInstall"
              "rustflags"
            ]
          );
        portableGnu = portableBuild "x86_64-unknown-linux-gnu.${portableGlibc}" "nuschl-linux-gnu" {
          cargoExtraArgs = "--locked -p nuschl -p nuschl-media-worker -p nuschl-video-worker";
          NUSCHL_SOFTWARE_VIDEO_ONLY = "1";
          # Link inputs by soname only. Their own glibc references resolve
          # against the host's glibc at runtime.
          rustflags = "-C link-arg=-Wl,--allow-shlib-undefined -L native=${pkgs.libxcb}/lib -L native=${pkgs.libxkbcommon}/lib";
          # Build scripts query fontconfig and freetype; --as-needed drops them.
          buildInputs = [
            pkgs.alsa-lib
            pkgs.pipewire
            pkgs.libxkbcommon
            pkgs.libxcb
            pkgs.fontconfig
            pkgs.freetype
          ];
          nativeBuildInputs = [ pkgs.clang ];
          LIBCLANG_PATH = common.LIBCLANG_PATH;
          postInstall = ''
            for program in "$out"/bin/*; do
              for library in $(${pkgs.binutils}/bin/readelf -d "$program" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p'); do
                case "$library" in
                  libc.so.6 | libm.so.6 | libdl.so.2 | libpthread.so.0 | librt.so.1 | ld-linux-x86-64.so.2) ;;
                  libxcb.so.1 | libxkbcommon.so.0 | libxkbcommon-x11.so.0 | libpipewire-0.3.so.0 | libasound.so.2) ;;
                  *) echo "$program needs $library" >&2; exit 1 ;;
                esac
              done
              if ! ${pkgs.binutils}/bin/readelf -l "$program" | grep -q 'interpreter: /lib64/ld-linux-x86-64.so.2]'; then
                echo "$program has another interpreter" >&2
                exit 1
              fi
              newest=$(${pkgs.binutils}/bin/objdump -T "$program" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)
              if [ "$(printf '%s\n' "$newest" GLIBC_${portableGlibc} | sort -V | tail -1)" != GLIBC_${portableGlibc} ]; then
                echo "$program needs $newest" >&2
                exit 1
              fi
            done
          '';
        };
        portableMusl =
          pname: cargoExtraArgs:
          portableBuild "x86_64-unknown-linux-musl" pname {
            inherit cargoExtraArgs;
            postInstall = ''
              for program in "$out"/bin/*; do
                if ${pkgs.binutils}/bin/readelf -ld "$program" | grep -qE 'INTERP|NEEDED'; then
                  echo "$program is not static" >&2
                  exit 1
                fi
              done
            '';
          };
        portableHeadless = portableMusl "nuschl-linux-musl" (
          "--locked -p nuschld -p nuschl-cli -p nuschl-tui -p nuschl-musicbot -p nuschl-musicbot-codec-worker"
          + " -p nuschl-content-worker -p nuschl-crypto-worker -p nuschl-math-worker -p nuschl-storage-worker -p nuschl-transfer-worker -p nuschl-rtc-worker"
        );
        portableVerifier =
          feature:
          portableMusl "nuschl-linux-musl-verify-${feature}" "--locked -p nuschl-update-worker --no-default-features --features nuschl-update-worker/${feature} --bin nuschl-update-verify-${feature}";
        # The desktop's layout from docs/distro-packaging.md: the programs
        # side by side in lib/nuschl, with the clients linked from bin.
        linuxPortable = pkgs.runCommand "nuschl-linux-portable-${version}" { } ''
          mkdir -p "$out/lib/nuschl" "$out/bin"
          cp ${portableGnu}/bin/{nuschl,nuschl-media-worker,nuschl-video-worker} "$out/lib/nuschl/"
          for program in nuschl-cli nuschl-tui nuschl-content-worker nuschl-crypto-worker nuschl-math-worker nuschl-rtc-worker nuschl-storage-worker nuschl-transfer-worker; do
            cp ${portableHeadless}/bin/$program "$out/lib/nuschl/"
          done
          cp ${portableVerifier "ed25519"}/bin/* ${portableVerifier "mldsa44"}/bin/* "$out/lib/nuschl/"
          for program in nuschl nuschl-cli nuschl-tui; do
            ln -s ../lib/nuschl/$program "$out/bin/$program"
          done
          cp -R --no-preserve=mode ${(import ./nix/desktop-files.nix { inherit pkgs; }).linux}/share "$out/"
          ${licenseLayoutWith releaseNotices.portable.bundle}
        '';
        serverStatic = pkgs.runCommand "nuschld-static-${version}" { } ''
          install -Dm755 -t "$out/bin" ${portableHeadless}/bin/nuschld ${portableHeadless}/bin/nuschl-storage-worker
          install -Dm755 ${portableHeadless}/bin/nuschl-musicbot "$out/bin/nuschl-musicbot"
          install -Dm755 -t "$out/libexec/nuschl-musicbot" ${portableHeadless}/bin/nuschl-musicbot-codec-worker ${portableHeadless}/bin/nuschl-content-worker
          install -Dm644 ${./LICENSE} "$out/share/licenses/nuschl/EUPL-1.2"
          install -Dm644 ${./crates/protocol/proto/LICENSE.mumble} "$out/share/licenses/nuschl/Mumble-BSD"
          mkdir -p "$out/share/licenses/nuschl"
          cp -R ${releaseNotices.serverStatic.bundle}/share/licenses/nuschl/. "$out/share/licenses/nuschl/"
        '';
      in
      {
        packages.default = shipped;
        packages.${distroOutput "linux-native-stage"} = linuxDistro.stage;
        packages.${distroOutput "deb"} = linuxDistro.deb;
        packages.${distroOutput "linux-portable"} = withCommit "share/nuschl" linuxPortable;
        packages.${distroOutput "server-static"} = withCommit "share/nuschl" serverStatic;
        packages.${if linuxDistro ? aur then distroOutput "aur" else null} = linuxDistro.aur;
        packages.${distroOutput "apt-repository"} = linuxDistro.aptRepository.command;
        # The clean-system package tests.
        legacyPackages.${distroOutput "distroTests"} = distroChecks.tests;
        packages.default-crane = cranePackage;
        packages.emojiAssets = emojiAssets;
        packages.server = withCommit "share/nuschl" serverPackage;
        packages.witness = withCommit "share/nuschl" witnessPackage;
        packages.update-signer = updateSigner;
        packages.update-verifier-ed25519 = updateEdVerifier;
        packages.update-verifier-mldsa44 = updateMlVerifier;
        packages.update-runtime-fixture = updateRuntimeFixture;
        apps.update-signer = flake-utils.lib.mkApp {
          drv = updateSigner;
          exePath = "/bin/nuschl-update-sign";
        };
        packages.musicbot = withCommit "share/nuschl" musicbotPackage;
        packages.musicbot-web = musicbotWeb;
        packages.release-notices-musicbot = musicbotNotices.bundle;
        # The zip and the installer hold bin/ and the licences.
        packages.windows = withCommit "bin/share/nuschl" windowsPackage;
        packages.windows-installer = import ./packaging/windows/package.nix {
          inherit pkgs version;
          windows = self.packages.${system}.windows;
        };
        packages.windows-update-ed25519 = windowsVerifiers.ed25519;
        packages.windows-update-mldsa44 = windowsVerifiers.mldsa44;
        packages.macos-x86_64-update-ed25519 = macVerifier "x86_64" "ed25519";
        packages.macos-x86_64-update-mldsa44 = macVerifier "x86_64" "mldsa44";
        packages.macos-aarch64-update-ed25519 = macVerifier "aarch64" "ed25519";
        packages.macos-aarch64-update-mldsa44 = macVerifier "aarch64" "mldsa44";
        packages.macos-universal = macUniversal;
        packages.macos-app = macApp;
        packages.macos-aarch64 = macSlice "aarch64";
        packages.rtc-probe = rtcPackage;
        packages.${if pkgs.stdenv.hostPlatform.isLinux then "video-probe" else null} = videoProbe;
        # The web client's tunnel relay and its probe (docs/web.md), from cached crates.
        packages.web-relay =
          withCommit "share/nuschl"
            crates.project.workspaceMembers."nuschl-web-relay".build;
        packages.stream-sfu =
          withCommit "share/nuschl"
            crates.project.workspaceMembers."nuschl-stream-sfu".build;
        packages.web-client = webClient.app;
        packages.web-wasm = webClient.wasm;
        packages.web-audio-wasm = webClient.audioWasm;
        packages.web-content-wasm = webClient.contentWasm;
        packages.web-stream-wasm = webClient.streamWasm;
        packages.release-notices-linux = releaseNotices.linux.bundle;
        packages.release-notices-linux-portable = releaseNotices.portable.bundle;
        packages.release-notices-server-static = releaseNotices.serverStatic.bundle;
        packages.release-notices-windows = releaseNotices.windows.bundle;
        packages.release-notices-macos = releaseNotices.macos.bundle;
        packages.release-notices-web = releaseNotices.web.bundle;
        packages.release-notices-rtc = rtcNotices.bundle;
        # Keep share-link routes at the root; the Svelte client lives at /app/.
        packages.web-site = pkgs.runCommand "nuschl-web-site" { } ''
          cp -R ${./web/site} "$out"
          chmod -R u+w "$out"
          mkdir -p "$out/app"
          cp -R ${webClient.app}/. "$out/app/"
          mkdir -p "$out/licenses/nuschl"
          cp -R ${releaseNotices.web.bundle}/share/licenses/nuschl/. "$out/licenses/nuschl/"
        '';
        # The call page and its stream worker, served by the stream host at /call
        # (nix/stream-module.nix).
        packages.call-site =
          let
            site = lib.cleanSourceWith {
              src = ./web/call;
              filter = path: _: !(lib.hasSuffix ".test.js" path);
            };
          in
          pkgs.runCommand "nuschl-call-site" { } ''
            mkdir -p "$out/licenses/nuschl"
            cp ${site}/*.{html,css,js} "$out/"
            cp ${./web/site/nuschl.svg} "$out/nuschl.svg"
            cp ${webClient.streamWasm}/nuschl_stream_wasm{.js,_bg.wasm} "$out/"
            cp ${./LICENSE} "$out/licenses/nuschl/EUPL-1.2"
            cp -R ${releaseNotices.callPage.bundle}/share/licenses/nuschl/. "$out/licenses/nuschl/"
          '';
        apps.rtc-probe =
          (flake-utils.lib.mkApp {
            drv = rtcPackage;
            exePath = "/bin/nuschl-rtc-probe";
          })
          // {
            meta.description = "Local-only str0m audio/video transport trial";
          };
        apps.default =
          (flake-utils.lib.mkApp {
            drv = shipped;
            exePath = "/bin/nuschl";
          })
          // {
            meta.description = "Nuschl Mumble desktop client";
          };
        # `nix run .#tui -- mumble://name@host`: the terminal client.
        apps.tui =
          (flake-utils.lib.mkApp {
            drv = shipped;
            exePath = "/bin/nuschl-tui";
          })
          // {
            meta.description = "Nuschl in a terminal";
          };
        checks = {
          ${distroOutput "distro-debian"} = distroChecks.tests.debian;
          ${distroOutput "distro-ubuntu"} = distroChecks.tests.ubuntu;
          ${distroOutput "distro-arch"} = distroChecks.tests.arch;
          ${distroOutput "distro-aur"} = distroChecks.aurTest;
          build = package;
          # The shipped package shows its commit, from bin/ and libexec/nuschl/.
          package-commit = pkgs.runCommand "nuschl-package-commit" { } ''
            for program in nuschl nuschld; do
              test "$(${shipped}/bin/$program --version)" = "$program ${version} (${
                if commit == null then "dev" else commit
              })"
            done
            touch $out
          '';
          server-build = serverPackage;
          rtc-build = rtcPackage;
          clippy = craneLib.cargoClippy (
            common
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );
          # exchange.rs has its own main (harness = false) and its own check;
          # the unit-test leaf compiles it under libtest and reports zero tests.
          ipc-exchange = import ./nix/ipc-exchange-check.nix {
            inherit pkgs toolchain;
            ipc = crates.project.workspaceMembers."nuschl-ipc".build;
          };
          ipc-exchange-negative-controls = pkgs.linkFarm "nuschl-ipc-exchange-negative-controls" (
            lib.mapAttrsToList (name: path: {
              inherit name path;
            }) self.checks.${system}.ipc-exchange.negativeControls
          );
          fmt = craneLib.cargoFmt { inherit src; };
          deny = craneLib.cargoDeny (
            common
            // {
              cargoExtraArgs = "";
              cargoDenyChecks = "bans licenses sources";
            }
          );
          # Unit tests of the browser modules outside the web build: content_window
          # also reads the Rust content broker it checks against.
          web-unit = pkgs.runCommand "nuschl-web-unit" { nativeBuildInputs = [ pkgs.nodejs ]; } ''
            cd ${./.}
            node --test tests/content_window.unit.mjs tests/sdk_rpc.unit.mjs tests/sdk_rpc_startup.unit.mjs \
              tests/web_audio.unit.mjs tests/web_content.unit.mjs tests/share_links.mjs \
              tests/web_output_quality.mjs tests/web_playout_clock.mjs
            node tests/web_playout_worker.mjs ${webClient.audioWasm}
            touch $out
          '';
          # The call page's stream worker on wasm32 replays the native transcript.
          stream-wasm = pkgs.runCommand "nuschl-stream-wasm" { nativeBuildInputs = [ pkgs.nodejs ]; } ''
            node ${./tests/stream_wasm.mjs} ${webClient.streamWasm} \
              ${./crates/stream-wasm/test-vectors/transcript.txt}
            touch $out
          '';
          # The audio worker's simd128 build encodes and decodes to the scalar build's bits.
          audio-wasm-simd =
            pkgs.runCommand "nuschl-audio-wasm-simd" { nativeBuildInputs = [ pkgs.nodejs ]; }
              ''
                node ${./tests/web_audio_simd.mjs} ${webClient.audioWasmScalar} ${webClient.audioWasm} \
                  ${./tests/fixtures/music-290k.opus}
                touch $out
              '';
          # The call page's link handling, admission, worker messages and bounds.
          call-page = pkgs.runCommand "nuschl-call-page" { nativeBuildInputs = [ pkgs.nodejs ]; } ''
            cp -R ${./web/call} call
            STREAM_WASM=${webClient.streamWasm} node --test call/*.test.js
            touch $out
          '';
          # Unit tests of the repository's Python tools; cases that need private
          # release artifacts skip themselves.
          python-unit =
            pkgs.runCommand "nuschl-python-unit"
              {
                nativeBuildInputs = [
                  pkgs.python3
                  pkgs.git
                ];
              }
              ''
                cd ${./.}
                python3 tests/cross_distributions_test.py
                python3 tests/ios_installer_test.py
                python3 tests/ios_xip_test.py
                python3 tests/windows_update_test.py
                touch $out
              '';
          # Every file under tests/ runs in a check or is listed in tests/local-only.txt (#313).
          tests-wired = pkgs.runCommand "nuschl-tests-wired" { } ''
            cd ${./.}
            missing=0
            for path in tests/*; do
              [ -f "$path" ] || continue
              name=''${path#tests/} stem=''${path#tests/}
              stem=''${stem%.*}
              cut -f1 tests/local-only.txt | grep -qxF "$name" && continue
              grep -rqsF --exclude="$name" -e "$name" -e "tests/$stem" -e "import $stem" -e "from $stem " \
                nix flake.nix ci.nix justfile scripts tests .github && continue
              echo "$path runs in no check: wire it in or list it in tests/local-only.txt" >&2
              missing=1
            done
            [ "$missing" = 0 ] && touch $out
          '';
          # nix/crate2nix must equal what `just crate2nix` generates: the same
          # crate2nix and command on the source, offline from crane's vendored crates.
          crate2nix-current =
            pkgs.runCommand "crate2nix-current"
              {
                nativeBuildInputs = [ pkgs.crate2nix ];
                CARGO_NET_OFFLINE = "true";
              }
              ''
                export CARGO_HOME="$TMPDIR/cargo-home"
                install -Dm644 ${craneLib.vendorCargoDeps { inherit src; }}/config.toml "$CARGO_HOME/config.toml"
                cp -R --no-preserve=mode ${src} tree
                cd tree
                if ! crate2nix generate -f Cargo.toml -o nix/crate2nix/Cargo.nix -h nix/crate2nix/crate-hashes.json \
                  || ! diff -ru ${./nix/crate2nix} nix/crate2nix | head -n 40 >&2; then
                  echo "nix/crate2nix is out of date: run 'just crate2nix'" >&2
                  exit 1
                fi
                touch $out
              '';
          audit = craneLib.cargoAudit {
            inherit src advisory-db;
            # Share the individually reviewed exceptions with cargo-deny. Audit
            # scans every lockfile entry, including OpenMLS's unselected RustCrypto
            # test provider: only that unused graph retains crypto-bigint 0.5.
            # cargo-deny must still reject RUSTSEC-2026-0387 if it becomes reachable.
            cargoAuditExtraArgs =
              "--deny unsound --ignore RUSTSEC-2026-0387"
              +
                lib.concatMapStrings (item: " --ignore ${item.id}")
                  (builtins.fromTOML (builtins.readFile ./deny.toml)).advisories.ignore;
            # Registry-dependent yanked checks remain enabled in `cargo deny check`.
            preBuild = ''
              mkdir -p .cargo
              cp ${pkgs.writeText "nuschl-sandbox-audit.toml" "[yanked]\nenabled = false\n"} .cargo/audit.toml
            '';
          };
        }
        // unitChecks
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
          let
            homeManagerChecks = import ./nix/home-manager-test.nix {
              inherit pkgs home-manager;
              module = self.homeModules.default;
              nuschl = package;
              inherit shipped;
            };
          in
          {
            preview-worker = import ./nix/preview-worker-check.nix {
              inherit pkgs toolchain;
              ipc = crates.project.workspaceMembers."nuschl-ipc".build;
              contentWorker = crates.project.workspaceMembers."nuschl-content-worker".build;
            };
            rename-options =
              let
                legacy = nixpkgs.lib.nixosSystem {
                  inherit system;
                  modules = [
                    self.nixosModules.default
                    self.nixosModules.witness
                    self.nixosModules.web
                    {
                      system.stateVersion = "26.05";
                      services.nuschel.settings.welcome_text = "legacy alias";
                      services.nuschel-witness.name = "legacy-alias";
                      services.nuschel-web.hostName = "legacy.example.invalid";
                    }
                  ];
                };
              in
              assert legacy.config.services.nuschl.settings.welcome_text == "legacy alias";
              assert legacy.config.services.nuschl-witness.name == "legacy-alias";
              assert legacy.config.services.nuschl-web.hostName == "legacy.example.invalid";
              pkgs.runCommand "nuschl-rename-options" { } "touch $out";
            home-manager = homeManagerChecks.files;
            vm-home-manager = homeManagerChecks.vm;
            # nuschld under the module with stock clients (`nix build .#checks.x86_64-linux.vm-server`).
            vm-server = import ./nix/vm-server.nix {
              inherit pkgs serverPackage workerTestCodec;
              nuschl = package;
              module = self.nixosModules.default;
              witnessModule = self.nixosModules.witness;
            };
            # The web host's module: site, landing page, app links, and a login
            # to Murmur through the relay (`nix build .#checks.x86_64-linux.vm-web`).
            vm-web = import ./nix/vm-web.nix {
              inherit pkgs;
              relay = crates.project.workspaceMembers."nuschl-web-relay".build;
              module = self.nixosModules.web;
              streamModule = self.nixosModules.stream;
              sfuPackage = crates.project.workspaceMembers."nuschl-stream-sfu".build;
            };
            # The screen-share TURN relay and stream host under their modules,
            # with a fake SFU peer (`nix build .#checks.x86_64-linux.vm-turn-stream`).
            vm-turn-stream = import ./nix/turn-stream-vm.nix {
              inherit pkgs;
              turnModule = self.nixosModules.turn;
              streamModule = self.nixosModules.stream;
              sfuPackage = crates.project.workspaceMembers."nuschl-stream-sfu".build;
            };
            # The call page in headless Chromium and Firefox, subscribing through the
            # SFU and TURN to synthetic publishers (`nix build .#checks.x86_64-linux.vm-call-page`).
            vm-call-page = import ./nix/call-page-vm.nix {
              inherit pkgs;
              turnModule = self.nixosModules.turn;
              streamModule = self.nixosModules.stream;
              callSite = self.packages.${system}.call-site;
              sfuPackage = crates.project.workspaceMembers."nuschl-stream-sfu".build;
              nativeCallTest = crates.sessionTest;
              nativeCallWorkers = [
                crates.project.workspaceMembers.nuschl-crypto-worker.build
                crates.project.workspaceMembers.nuschl-rtc-worker.build
                videoWorker
              ];
            };
            # The music bot under its module, against Murmur and a fake library
            # (`nix build .#checks.x86_64-linux.vm-musicbot`).
            vm-musicbot = import ./nix/vm-musicbot.nix {
              inherit pkgs;
              musicbot = musicbotPackage;
              cli = crates.project.workspaceMembers.nuschl-cli.build;
              module = self.nixosModules.musicbot;
            };
            vm-smoke = import ./nix/vm-smoke.nix {
              inherit pkgs workerTestCodec;
              nuschl = package;
              rtcProbe = rtcPackage;
              module = self.nixosModules.default;
              legacyServers = {
                "1.4" = "${nixpkgs-mumble14.legacyPackages.${system}.murmur}/bin/mumble-server";
                "1.3" = "${nixpkgs-mumble13.legacyPackages.${system}.murmur}/bin/murmurd";
              };
            };
            vm-link-previews = import ./tests/link_previews_vm.nix {
              inherit pkgs;
              nuschl = package;
              sourceIdentity = self.rev or self.outPath;
              contentWorker = crates.project.workspaceMembers."nuschl-content-worker".build;
              nativeFetchTest = crates.sessionTest;
            };
          }
        );
        devShells.default = craneLib.devShell {
          inherit buildInputs;
          LIBCLANG_PATH = common.LIBCLANG_PATH;
          NUSCHL_EMOJI_ASSETS = emojiAssets;
          packages =
            nativeBuildInputs
            ++ [
              pkgs.cargo-deny
              pkgs.cargo-audit
              pkgs.just
              pkgs.gh
              pkgs.gnupg
              pkgs.rsync
              # Release notes; release/local-release.py runs it offline with cliff.toml.
              pkgs.git-cliff
              pkgs.nixfmt
              pkgs.taplo
              pkgs.jujutsu
              pkgs.python3
              pkgs.nodejs
            ]
            ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.bubblewrap
              pkgs.util-linux
              pkgs.ffmpeg-headless
              pkgs.coturn
              pkgs.dbus
            ];
          LD_LIBRARY_PATH = lib.optionalString pkgs.stdenv.hostPlatform.isLinux (
            lib.makeLibraryPath buildInputs
          );
        };
        # cargo-fuzz with nightly; libfuzzer-sys compiles libFuzzer with the
        # shell's C++ compiler. See SECURITY.md, "Fuzzing".
        devShells.fuzz = pkgs.mkShell {
          packages = [
            fuzzToolchain
            pkgs.cargo-fuzz
            pkgs.just
            pkgs.protobuf
            pkgs.pkg-config
          ];
        };
        # `just miri`: the sandbox adapter's tests under Miri, for Linux and Windows.
        devShells.miri = pkgs.mkShell {
          packages = [
            miriToolchain
            pkgs.just
          ];
        };
        formatter = pkgs.nixfmt;
      }
    );
}
