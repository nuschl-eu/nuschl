# The call page (web/call) on the stream host under its module, with coturn
# under the TURN module, in headless Chromium and Firefox (tests/call_page.py):
# synthetic publishers on the stream host's origin, a Chromium and a relay-only
# Firefox subscriber, a page user publishing Chromium's fake camera and screen,
# a codec switch, the refusals. The page's screenshots are the test's output
# (`result/screenshots`). The stream host serves HTTPS with a certificate made
# for the test; no ACME.
{
  pkgs,
  turnModule,
  streamModule,
  callSite,
  nativeCallTest,
  nativeCallWorkers,
  sfuPackage,
}:
let
  restSecret = "call-test-rest-secret";
  mediaPort = 40010;
  certificate = pkgs.runCommand "stream-test-certificate" { nativeBuildInputs = [ pkgs.openssl ]; } ''
    mkdir "$out"
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
      -subj /CN=stream.test -addext subjectAltName=DNS:stream.test \
      -addext basicConstraints=critical,CA:FALSE \
      -keyout "$out/key.pem" -out "$out/cert.pem" 2>/dev/null
  '';
  site = pkgs.runCommand "nuschl-call-test-site" { } ''
    cp -R ${callSite} "$out"
    chmod u+w "$out"
    cp ${../tests/call_publisher.html} "$out/call_publisher.html"
    cp ${../tests/call_publisher.js} "$out/call_publisher.js"
  '';
  secret = file: {
    serviceConfig = {
      Type = "oneshot";
      UMask = "0077";
    };
    script = "mkdir -p ${dirOf file} && printf %s ${restSecret} > ${file}";
  };
in
pkgs.testers.runNixOSTest {
  name = "nuschl-call-page";
  globalTimeout = 1800;
  nodes = {
    turn =
      { nodes, ... }:
      {
        imports = [ turnModule ];
        networking.nftables.enable = true;
        systemd.services.turn-test-secret = secret "/var/lib/turn-test/rest-secret" // {
          requiredBy = [ "coturn.service" ];
          before = [ "coturn.service" ];
        };
        services.nuschl-turn = {
          enable = true;
          hostName = "turn.test";
          secretFile = "/var/lib/turn-test/rest-secret";
          allowedPeerAddresses = [ nodes.sfu.networking.primaryIPAddress ];
          allowedPeerPorts = [ mediaPort ];
          openFirewall = true;
        };
      };
    sfu =
      { nodes, ... }:
      {
        imports = [ streamModule ];
        networking.firewall.allowedTCPPorts = [ 443 ];
        # Counts media that arrives through the TURN relay.
        networking.nftables = {
          enable = true;
          tables.relayed = {
            family = "inet";
            content = ''
              chain input {
                type filter hook input priority -10;
                ip saddr ${nodes.turn.networking.primaryIPAddress} udp dport ${toString mediaPort} counter
              }
            '';
          };
        };
        systemd.services.sfu-test-secret = secret "/var/lib/sfu-test/rest-secret" // {
          requiredBy = [ "nuschl-stream-sfu.service" ];
          before = [ "nuschl-stream-sfu.service" ];
        };
        services.nuschl-stream = {
          enable = true;
          hostName = "stream.test";
          enableACME = false;
          inherit site;
          sfu = {
            enable = true;
            package = sfuPackage;
            port = mediaPort;
            addresses = [ nodes.sfu.networking.primaryIPAddress ];
            openFirewall = true;
            turn = {
              urls = [ "turn:turn.test:3478?transport=udp" ];
              secretFile = "/var/lib/sfu-test/rest-secret";
            };
            # No desktop renews the creator's lease here.
            settings.lease_secs = 3600;
          };
        };
        services.nginx.virtualHosts."stream.test" = {
          sslCertificate = "${certificate}/cert.pem";
          sslCertificateKey = "${certificate}/key.pem";
        };
      };
    client =
      { nodes, ... }:
      {
        virtualisation = {
          memorySize = 8192;
          cores = 4;
        };
        environment.systemPackages = [
          pkgs.chromedriver
          pkgs.geckodriver
          (pkgs.python3.withPackages (p: [ p.selenium ]))
          pkgs.bubblewrap
          pkgs.util-linux
        ]
        ++ nativeCallWorkers;
        environment.variables = {
          CHROMIUM = "${pkgs.chromium}/bin/chromium";
          FIREFOX = "${pkgs.firefox}/bin/firefox";
          SE_OFFLINE = "true";
          NUSCHL_NATIVE_CALL_TEST = "${nativeCallTest}/bin/nuschl-session-test";
          NUSCHL_NATIVE_CALL_CA = "${certificate}/cert.pem";
          NUSCHL_NATIVE_CALL_PORT = toString mediaPort;
        };
        fonts.packages = [ pkgs.dejavu_fonts ];
        networking.hosts = {
          ${nodes.sfu.networking.primaryIPAddress} = [ "stream.test" ];
          ${nodes.turn.networking.primaryIPAddress} = [ "turn.test" ];
        };
      };
  };
  testScript = ''
    start_all()
    turn.wait_for_unit("coturn.service")
    sfu.wait_for_unit("nginx.service")
    sfu.wait_for_unit("nuschl-stream-sfu.service")
    client.wait_for_unit("multi-user.target")

    with subtest("the stream host serves the call page and its wasm with their headers"):
        headers = client.succeed("curl -sSk -o /dev/null -D - https://stream.test/call").lower()
        assert headers.startswith("http/2 200") or headers.startswith("http/1.1 200"), headers
        assert "require-trusted-types-for 'script'" in headers and "cross-origin-embedder-policy: require-corp" in headers
        wasm = client.succeed("curl -sSk -o /dev/null -D - https://stream.test/nuschl_stream_wasm_bg.wasm").lower()
        assert "content-type: application/wasm" in wasm, wasm
        client.succeed("curl -sSkf -o /tmp/notices https://stream.test/licenses/nuschl/THIRD-PARTY.txt && grep -q aes-gcm /tmp/notices")
        client.succeed("curl -sSkf -o /tmp/eupl https://stream.test/licenses/nuschl/EUPL-1.2 && grep -q 'European Union Public Licence' /tmp/eupl")

    with subtest("Chromium and relay-only Firefox subscribe to synthetic publishers"):
        try:
            client.succeed("python3 ${../tests/call_page.py} /tmp/screenshots >&2")
        finally:
            client.succeed("sync")
            client.copy_from_machine("/tmp/screenshots", "")
        # Media reached the SFU from coturn: the relay-only Firefox has no other path.
        sfu.succeed("nft list table inet relayed | grep -q 'counter packets [1-9]'")
  '';
}
