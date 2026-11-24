# The web host under its NixOS module (docs/web.md): nginx serving the site, the
# share-link landing page and the app-link files, and the tunnel relay behind it.
# A client without a browser logs in to a stock Murmur through the relay (TLS on
# the client, as the web client will do in WebAssembly), the packaged web app in
# headless Chromium joins voice by double-click, connects its saved servers on
# load, counts the people inside a collapsed channel, mutes when the browser
# stops its microphone and answers Ctrl+K and Ctrl+F, and what the relay must
# refuse is refused. No ACME in the VM: plain HTTP.
{
  pkgs,
  relay,
  module,
  streamModule,
}:
let
  mediaPort = 40010;
  # The browser test's microphone: every captured frame carries sound.
  tone = pkgs.runCommand "synthetic-997hz.wav" { nativeBuildInputs = [ pkgs.ffmpeg-headless ]; } ''
    ffmpeg -loglevel error -f lavfi -i sine=frequency=997:sample_rate=48000 -t 10 -ac 1 -f wav "$out"
  '';
  certificate =
    pkgs.runCommand "web-video-test-certificate" { nativeBuildInputs = [ pkgs.openssl ]; }
      ''
        mkdir "$out"
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
          -subj /CN=stream.test -addext subjectAltName=DNS:stream.test \
          -keyout "$out/key.pem" -out "$out/cert.pem" 2>/dev/null
      '';
in
pkgs.testers.runNixOSTest {
  name = "nuschl-web";
  globalTimeout = 1800;
  nodes = {
    web =
      { nodes, ... }:
      {
        imports = [
          module
          streamModule
        ];
        services.nuschl-web = {
          enable = true;
          hostName = "web";
          enableACME = false;
          forceSSL = false;
          relay = {
            package = relay;
            # The test network is private; public servers need no allowance.
            allowNetworks = [ "192.168.1.0/24" ];
            settings.client_opens_per_minute = 20;
          };
          installUrl = "https://example.org/get-nuschl";
          callHost = "stream.test";
          appLinks = {
            android = [
              {
                package = "org.nuschl.android";
                certificateFingerprints = [ "AA:BB" ];
              }
            ];
            apple = [ "ABCDE12345.org.nuschl.Nuschl" ];
          };
        };
        services.nuschl-stream = {
          enable = true;
          hostName = "stream.test";
          enableACME = false;
          sfu = {
            enable = true;
            port = mediaPort;
            addresses = [ nodes.web.networking.primaryIPAddress ];
            openFirewall = true;
          };
        };
        services.nginx.virtualHosts."stream.test" = {
          sslCertificate = "${certificate}/cert.pem";
          sslCertificateKey = "${certificate}/key.pem";
        };
        networking.firewall.allowedTCPPorts = [
          80
          443
        ];
      };
    murmur =
      { ... }:
      {
        services.murmur = {
          enable = true;
          openFirewall = true;
          welcometext = "Welcome through the relay";
          # Every client reaches Murmur from the relay's one address; the browser
          # steps open more connections than the default autoban allows.
          autobanAttempts = 0;
        };
        environment.systemPackages = [ pkgs.sqlite ];
        # Another TCP service on a port the relay would forward to: not Mumble.
        networking.firewall.allowedTCPPorts = [ 64740 ];
        systemd.services.not-mumble = {
          wantedBy = [ "multi-user.target" ];
          script = ''
            exec ${pkgs.socat}/bin/socat TCP-LISTEN:64740,fork,reuseaddr SYSTEM:'echo SSH-2.0-OpenSSH_9.9; sleep 5'
          '';
        };
      };
    client =
      { nodes, ... }:
      {
        environment.systemPackages = [
          relay
          pkgs.curl
          pkgs.jq
          pkgs.nodejs
        ];
        environment.variables = {
          NUSCHL_PLAYWRIGHT = "${pkgs.playwright-driver}/index.mjs";
          NUSCHL_CHROMIUM = "${pkgs.chromium}/bin/chromium";
        };
        fonts.packages = [ pkgs.dejavu_fonts ];
        virtualisation.memorySize = 2048;
        networking.hosts.${nodes.web.networking.primaryIPAddress} = [ "stream.test" ];
      };
  };
  testScript = ''
    import itertools
    import time

    start_all()
    web.wait_for_unit("nuschl-web-relay.service")
    web.wait_for_file("/run/nuschl-web-relay/relay.sock")
    web.wait_for_unit("nginx.service")
    web.wait_for_unit("nuschl-stream-sfu.service")
    web.wait_for_open_port(80)
    murmur.wait_for_unit("murmur.service")
    # Channels for the web app's tests, added to the database Murmur created.
    murmur.wait_for_open_port(64738)
    murmur.succeed(
        "systemctl stop murmur",
        "sqlite3 /var/lib/murmur/murmur.sqlite \"INSERT INTO channels (server_id, channel_id, parent_id, name, inheritacl)"
        " VALUES (1, 1, 0, 'Lobby', 1), (1, 2, 0, 'Music', 1), (1, 3, 0, 'AFK', 1), (1, 4, 1, 'Games', 1)\"",
        "systemctl start murmur",
    )
    murmur.wait_for_open_port(64738)
    murmur.wait_for_open_port(64740)
    client.wait_for_unit("multi-user.target")

    # The site, the landing page for any token, and the app-link files.
    client.succeed("curl -sf http://web/ | grep -q 'example.org/get-nuschl'")
    page = client.succeed("curl -sf http://web/join/AAAAAAAAAAAAAAAAAAAAAA")
    assert "Join on Nuschl" in page and "/join.js" in page, page
    client.succeed("curl -sf http://web/join/ | grep -q 'Join on Nuschl'")
    client.succeed("curl -sf http://web/join.js | grep -q 'nuschl://join/'")
    headers = client.succeed("curl -sfI http://web/join/AAAAAAAAAAAAAAAAAAAAAA")
    assert "Content-Security-Policy" in headers and "no-referrer" in headers, headers
    assert "X-DNS-Prefetch-Control: off" in headers, headers
    assert "Cross-Origin-Embedder-Policy: require-corp" in headers, headers
    assert "camera=()" in headers and "display-capture=()" in headers, headers
    app_headers = client.succeed("curl -sfI http://web/app/")
    assert "camera=(self)" in app_headers and "display-capture=(self)" in app_headers, app_headers
    assert "wss://stream.test/sfu https://stream.test/sfu/lease" in app_headers, app_headers
    client.succeed("curl -sf http://web/.well-known/assetlinks.json | jq -e '.[0].target.package_name == \"org.nuschl.android\"'")
    client.succeed("curl -sfI http://web/.well-known/assetlinks.json | grep -qi 'content-type: application/json'")
    client.succeed("curl -sf http://web/.well-known/apple-app-site-association | jq -e '.applinks.details[0].appIDs[0] == \"ABCDE12345.org.nuschl.Nuschl\"'")
    # Tokens in /join/ paths are not logged.
    web.fail("grep -r AAAAAAAAAAAAAAAAAAAAAA /var/log/nginx/")

    # A login to Murmur through the relay, TLS on the client: the relay pinged
    # the server, so datagrams go over UDP too. The relay waits 1.5 s for that
    # ping; on a loaded host Murmur's event loop can miss it right after
    # starting, so the probe repeats within a 60 s hang guard (#376), printing
    # each attempt's time.
    deadline = time.monotonic() + 60
    for attempt in itertools.count(1):
        started = time.monotonic()
        out = client.succeed("nuschl-web-relay-probe --relay ws://web/relay --origin http://web --server murmur:64738")
        print(f"relay probe attempt {attempt}: {time.monotonic() - started:.2f} s: {out.strip()}")
        assert "connected session=" in out, out
        if "udp=true udp_answered=true" in out:
            break
        assert time.monotonic() < deadline, out
        time.sleep(5)
    # The packaged app: a mouse or pen double-click joins voice, touch opens chat.
    client.succeed("node ${../tests/web_channel_dblclick.browser.mjs} http://web/app/ murmur 64738")

    # The packaged app connects saved servers on load and rejoins Lobby; the
    # service on 64740 refuses.
    client.succeed("node ${../tests/web_autoconnect.browser.mjs} http://web/app/ murmur 64738 64740")

    # The relay never logs payloads; its journal names no user.
    web.fail("journalctl -u nuschl-web-relay | grep -q relay-probe")

    # What the relay refuses: other origins, other ports, local and private
    # addresses outside the allowance, and services that are not Mumble.
    client.fail("nuschl-web-relay-probe --relay ws://web/relay --origin http://evil.example --server murmur:64738")
    out = client.fail("nuschl-web-relay-probe --relay ws://web/relay --origin http://web --server murmur:22")
    assert "PortRefused" in out, out
    out = client.fail("nuschl-web-relay-probe --relay ws://web/relay --origin http://web --server 127.0.0.1:64738")
    assert "AddressRefused" in out, out
    out = client.fail("nuschl-web-relay-probe --relay ws://web/relay --origin http://web --server 10.0.0.1:64738")
    assert "AddressRefused" in out, out
    out = client.fail("nuschl-web-relay-probe --relay ws://web/relay --origin http://web --server murmur:64740")
    assert "TLS" in out or "NotMumble" in out, out
    # The kernel refuses loopback to the relay as well (IPAddressDeny).
    web.succeed("systemctl show nuschl-web-relay -p IPAddressDeny | grep -q 127.0.0.0/8")

    # The app's collapsed Lobby counts bob in Games below it (#451).
    client.succeed("mkdir -p /tmp/shots")
    print(client.succeed("node ${../tests/web_tree.browser.mjs} ${pkgs.chromium}/bin/chromium ${pkgs.playwright-driver} /tmp/shots"))
    client.copy_from_machine("/tmp/shots", "screenshots")

    # When the browser ends or mutes capture, the app mutes and opens nothing (#447).
    print(client.succeed("node ${../tests/web_mic_stopped.browser.mjs} ${pkgs.chromium}/bin/chromium ${pkgs.playwright-driver} ${tone}"))

    # The packaged app: Ctrl+K switches channels, Ctrl+F finds a sent message.
    client.succeed("node ${../tests/web_shortcuts.browser.mjs} http://web/app/ murmur 64738")

    # Both synthetic video sources cross WebRTC/SFrame and the call card uses Murmur.
    client.succeed("node ${../tests/web_video_call.browser.mjs} http://web/app/ murmur 64738")
  '';
}
