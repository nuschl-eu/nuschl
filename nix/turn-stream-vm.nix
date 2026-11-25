# The screen-share TURN relay and stream host under their modules
# (nix/turn-module.nix, nix/stream-module.nix), driven by coturn's own
# turnutils. coturn relays only to the SFU host, and there only to its
# allowed ports; other hosts and loopback are refused. REST credentials work
# until they expire, the per-user quota and no-tcp-relay hold, TLS works.
# The stream host sends its isolation headers and no cookies. Its SFU admits
# link holders at /sfu, refuses a wrong link key, and hands out TURN
# credentials coturn accepts. No ACME here.
{
  pkgs,
  turnModule,
  streamModule,
  sfuPackage,
}:
let
  site = pkgs.runCommand "nuschl-stream-test-site" { } ''
    mkdir -p "$out"
    echo '<!doctype html><title>call page</title>' > "$out/call.html"
    echo 'console.log("asset")' > "$out/app.js"
  '';
  restSecret = "turn-test-rest-secret";
  # turnutils_peer echoes UDP on every address of its host, on port and port + 1.
  peer = port: {
    wantedBy = [ "multi-user.target" ];
    serviceConfig.ExecStart = "${pkgs.coturn}/bin/turnutils_peer -p ${toString port}";
  };
in
pkgs.testers.runNixOSTest {
  name = "nuschl-turn-stream";
  globalTimeout = 1800;
  nodes = {
    turn =
      { nodes, ... }:
      {
        imports = [ turnModule ];
        networking.nftables.enable = true;
        # Test credentials, made at boot.
        systemd.services.turn-test-credentials = {
          requiredBy = [ "coturn.service" ];
          before = [ "coturn.service" ];
          path = [ pkgs.openssl ];
          serviceConfig = {
            Type = "oneshot";
            UMask = "0077";
          };
          script = ''
            mkdir -p /var/lib/turn-test && cd /var/lib/turn-test
            printf %s ${restSecret} > rest-secret
            [ -e key.pem ] || openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
              -nodes -days 2 -subj /CN=turn.test -keyout key.pem -out cert.pem 2>/dev/null
          '';
        };
        services.nuschl-turn = {
          enable = true;
          hostName = "turn.test";
          secretFile = "/var/lib/turn-test/rest-secret";
          allowedPeerAddresses = [
            nodes.sfu.networking.primaryIPAddress
            nodes.sfu.networking.primaryIPv6Address
          ];
          allowedPeerPorts = [
            40000
            40010
          ];
          userQuota = 2;
          openFirewall = true;
          tls = {
            certificateFile = "/var/lib/turn-test/cert.pem";
            keyFile = "/var/lib/turn-test/key.pem";
          };
        };
      };
    sfu =
      { nodes, ... }:
      {
        imports = [ streamModule ];
        networking.firewall.allowedTCPPorts = [ 80 ];
        networking.firewall.allowedUDPPorts = [
          40000
          40001
        ];
        systemd.services.peer = peer 40000;
        systemd.services.peer-other-port = peer 40001;
        systemd.services.sfu-test-secret = {
          requiredBy = [ "nuschl-stream-sfu.service" ];
          before = [ "nuschl-stream-sfu.service" ];
          serviceConfig = {
            Type = "oneshot";
            UMask = "0077";
          };
          script = "mkdir -p /var/lib/sfu-test && printf %s ${restSecret} > /var/lib/sfu-test/rest-secret";
        };
        services.nuschl-stream = {
          enable = true;
          hostName = "stream.test";
          enableACME = false;
          forceSSL = false;
          inherit site;
          sfu = {
            enable = true;
            package = sfuPackage;
            port = 40010;
            addresses = [
              nodes.sfu.networking.primaryIPAddress
              nodes.sfu.networking.primaryIPv6Address
            ];
            openFirewall = true;
            turn = {
              urls = [ "turn:turn.test:3478?transport=udp" ];
              secretFile = "/var/lib/sfu-test/rest-secret";
            };
            settings.max_peers_per_room = 2;
            trafficBudget = 1000000000;
            period = "day";
          };
        };
      };
    client =
      { nodes, ... }:
      {
        environment.systemPackages = [
          pkgs.coturn
          pkgs.curl
          (pkgs.python3.withPackages (p: [ p.websockets ]))
        ];
        networking.hosts.${nodes.sfu.networking.primaryIPAddress} = [ "stream.test" ];
      };
  };
  testScript =
    { nodes, ... }:
    let
      turn4 = nodes.turn.networking.primaryIPAddress;
      turn6 = nodes.turn.networking.primaryIPv6Address;
      sfu4 = nodes.sfu.networking.primaryIPAddress;
      sfu6 = nodes.sfu.networking.primaryIPv6Address;
      client4 = nodes.client.networking.primaryIPAddress;
      client6 = nodes.client.networking.primaryIPv6Address;
    in
    ''
      import base64, hashlib, hmac, json, re

      start_all()
      turn.wait_for_unit("coturn.service")
      sfu.wait_for_unit("nginx.service")
      sfu.wait_for_unit("nuschl-stream-sfu.service")
      sfu.wait_for_unit("peer.service")
      sfu.wait_for_unit("peer-other-port.service")
      client.wait_for_unit("multi-user.target")

      secret = turn.succeed("cat /var/lib/turn-test/rest-secret").encode()
      now = int(turn.succeed("date +%s"))

      def relay(user, peer, *flags, port=40000, lifetime=600, key=secret, server="${turn4}", expect=0, password=None):
          """Sends packets to peer:port through TURN with REST credentials; returns uclient's output."""
          username = user if password else f"{now + lifetime}:{user}"
          password = password or base64.b64encode(hmac.new(key, username.encode(), hashlib.sha1).digest()).decode()
          status, out = client.execute(
              f"timeout 60 turnutils_uclient --no-even-port -c -n 3 -u '{username}' -w '{password}' "
              f"-e {peer} -r {port} {' '.join(flags)} {server} 2>&1"
          )
          print(out)
          assert status == expect, (status, out)
          return out

      def echoed(out):
          """(sent, received) from uclient's final count."""
          sent, received = re.findall(r"tot_send_msgs=(\d+), tot_recv_msgs=(\d+)\n", out)[-1]
          return int(sent), int(received)

      with subtest("only the SFU's allowed port is a peer"):
          assert echoed(relay("v1", "${sfu4}")) == (3, 3)
          assert echoed(relay("v2", "${sfu4}", port=40001)) == (3, 0)
          turn.succeed("nft list table inet nuschl-turn-peers | grep -q 'counter packets [1-9]'")
          for denied in ["${client4}", "127.0.0.1", "10.0.0.1"]:
              assert "error 403" in relay("v3-" + denied, denied, expect=255)

      with subtest("IPv6: only the SFU is a peer"):
          assert echoed(relay("v6", "${sfu6}", "-x", server="${turn6}")) == (3, 3)
          assert "error 403" in relay("v6-denied", "${client6}", "-x", server="${turn6}", expect=255)

      with subtest("expired and forged credentials are refused"):
          assert "Cannot complete Allocation" in relay("expired", "${sfu4}", lifetime=-60, expect=255)
          assert "Cannot complete Allocation" in relay("forged", "${sfu4}", key=b"0" * 64, expect=255)
          logs = turn.succeed("journalctl --namespace=nuschl-turn -o cat")
          assert ":expired>" in logs and ":forged>" in logs, logs

      with subtest("per-user quota, no TCP relays, no anonymous STUN, no CLI"):
          relay("quota", "${sfu4}")
          assert "error 486" in relay("quota", "${sfu4}", expect=255)
          relay("other-user", "${sfu4}")
          assert "error 442" in relay("tcp", "${sfu4}", "-T", expect=255)
          assert "error 401" in client.execute("timeout 10 turnutils_stunclient ${turn4} 2>&1")[1]
          turn.fail("ss -ltn | grep -q ':5766 '")

      with subtest("TURN over TLS"):
          client.succeed("cat > /tmp/turn-ca.pem <<'EOF'\n" + turn.succeed("cat /var/lib/turn-test/cert.pem") + "EOF")
          assert echoed(relay("tls", "${sfu4}", "-t", "-S", "-E", "/tmp/turn-ca.pem")) == (3, 3)

      with subtest("coturn's journal keeps admitted users and client addresses out"):
          logs = turn.succeed("journalctl --namespace=nuschl-turn -o cat")
          for private in [":v1>", ":tls>", "${client4}"]:
              assert not any(private in line for line in logs.splitlines() if "peer IP" not in line), (private, logs)

      with subtest("stream host: headers on every location, no cookies, no access log"):
          def head(path):
              out = client.succeed(f"curl -sS -o /dev/null -D - http://stream.test{path}").lower()
              print(out)
              for header in ["content-security-policy:", "cross-origin-opener-policy: same-origin",
                             "cross-origin-embedder-policy: require-corp", "cross-origin-resource-policy: same-origin",
                             "origin-agent-cluster: ?1", "publickey-credentials-get=()",
                             "referrer-policy: no-referrer", "x-content-type-options: nosniff"]:
                  assert header in out, (path, header)
              assert "set-cookie" not in out, (path, out)
              return out

          call = head("/call")
          assert call.startswith("http/1.1 200") and "display-capture=(self), camera=(self), microphone=()" in call
          assert "display-capture=(), camera=()" in head("/app.js")
          assert head("/app.js").startswith("http/1.1 200")
          for missing in ["/", "/missing"]:
              assert head(missing).startswith("http/1.1 404"), missing
          sfu.fail("grep -rqE '/call|/sfu' /var/log/nginx/")

      with subtest("SFU: admission at /sfu, TURN credentials coturn accepts"):
          client.succeed("cat > /tmp/sfu.py <<'EOF'\n" + """
      import hashlib, hmac, json
      from websockets.sync.client import connect

      def join(admit, host=None, create=False):
          with connect("ws://stream.test/sfu") as ws:
              nonce = bytes.fromhex(json.loads(ws.recv())["nonce"])
              mac = lambda key, label: hmac.new(key, label + nonce, hashlib.sha256).hexdigest()
              message = {"type": "join", "room": "03" * 16, "verifier": hashlib.sha256(bytes([2]) * 32).hexdigest(),
                         "proof": mac(admit, b"nuschl/stream/v2 sfu admit"), "codecs": ["av1", "vp9"]}
              if host:
                  message["host_proof"] = mac(host, b"nuschl/stream/v2 sfu host")
              if create:
                  message["create"] = {"admit_key": admit.hex(), "host_key": host.hex()}
              ws.send(json.dumps(message))
              return json.loads(ws.recv())

      admit, host = bytes([1]) * 32, bytes([2]) * 32
      print(json.dumps([join(admit, host, create=True), join(admit), join(bytes(32))]))
      """ + "EOF")
          created, joined, refused = json.loads(client.succeed("python3 /tmp/sfu.py"))
          assert created["type"] == "welcome" and created["role"] == "host", created
          assert joined["role"] == "participant" and refused == {"type": "error", "reason": "proof"}, (joined, refused)
          turn_credentials = created["turn"]
          assert turn_credentials["urls"] == ["turn:turn.test:3478?transport=udp"], turn_credentials
          assert echoed(relay(turn_credentials["username"], "${sfu4}", password=turn_credentials["credential"])) == (3, 3)
          sfu.succeed("ss -lun | grep -q ':40010 '")
          sfu.wait_until_succeeds("test -s /var/lib/nuschl-stream-sfu/traffic.usage", timeout=10)
          lease = lambda method, key: client.succeed(
              f"curl -sS -o /dev/null -w '%{{http_code}}' -X {method} http://stream.test/sfu/lease "
              f"--data '{{\"stream_id\": \"{'03' * 16}\", \"host_key\": \"{key}\"}}'"
          )
          assert lease("POST", "02" * 32) == "204"
          assert lease("POST", "05" * 32) == "404"
          assert lease("DELETE", "02" * 32) == "204"
          assert lease("POST", "02" * 32) == "404"
          assert sfu.succeed("stat -c %U:%G /run/nuschl-stream-sfu/sfu.sock").strip() == "nuschl-stream-sfu:nuschl-stream-sfu"
    '';
}
