# The call host (#143): the static call page at /call (web/call) with the
# plan's isolation headers, no cookies and no access log, and the companion
# SFU (apps/stream-sfu) behind /sfu with its media on one UDP port.
# `services.nuschl-stream`.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.nuschl-stream;
  inherit (lib) mkOption types;
  toml = pkgs.formats.toml { };
  socketDirectory = "/run/nuschl-stream-sfu";
  socket = "${socketDirectory}/sfu.sock";
  sfuConfig = toml.generate "nuschl-stream-sfu.toml" (
    {
      listen = "unix:${socket}";
      media_listen = "[::]:${toString cfg.sfu.port}";
      media_addresses = cfg.sfu.addresses;
    }
    // lib.optionalAttrs (cfg.sfu.trafficBudget != null) {
      traffic_budget = cfg.sfu.trafficBudget;
      inherit (cfg.sfu) period;
      state_directory = "/var/lib/nuschl-stream-sfu";
    }
    // lib.optionalAttrs (cfg.sfu.turn.secretFile != null) {
      turn = {
        inherit (cfg.sfu.turn) urls;
        secret_file = "/run/credentials/nuschl-stream-sfu.service/turn-secret";
        lifetime_secs = cfg.sfu.turn.lifetime;
      };
    }
    // cfg.sfu.settings
  );
  limit =
    default: description:
    mkOption {
      type = types.ints.positive;
      inherit default description;
    };
  csp = lib.concatStringsSep "; " [
    "default-src 'none'"
    "script-src 'self' 'wasm-unsafe-eval'"
    "worker-src 'self'"
    "connect-src 'self'"
    "img-src 'self' blob:"
    "style-src 'self'"
    "base-uri 'none'"
    "form-action 'none'"
    "frame-ancestors 'none'"
    "require-trusted-types-for 'script'"
  ];
  # No identity code runs here, so passkeys are denied too. The call page alone
  # may ask for the camera and screen; nginx drops inherited add_header lines in a
  # block with its own, so /call repeats the full set.
  headers = capture: ''
    add_header Content-Security-Policy "${csp}" always;
    add_header Cross-Origin-Opener-Policy same-origin always;
    add_header Cross-Origin-Embedder-Policy require-corp always;
    add_header Cross-Origin-Resource-Policy same-origin always;
    add_header Origin-Agent-Cluster "?1" always;
    add_header Permissions-Policy "display-capture=${capture}, camera=${capture}, microphone=(), geolocation=(), publickey-credentials-get=(), publickey-credentials-create=()" always;
    add_header Referrer-Policy no-referrer always;
    add_header X-Content-Type-Options nosniff always;
    ${lib.optionalString cfg.forceSSL ''add_header Strict-Transport-Security "max-age=31536000" always;''}
  '';
in
{
  options.services.nuschl-stream = {
    enable = lib.mkEnableOption "the Nuschl call host";
    hostName = lib.mkOption {
      type = lib.types.str;
      example = "stream.nuschl.eu";
      description = "The virtual host, separate from the app and identity origins.";
    };
    enableACME = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Get a certificate for hostName (security.acme must be configured).";
    };
    forceSSL = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Redirect HTTP to HTTPS.";
    };
    site = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.call-site;
      defaultText = lib.literalExpression "inputs.nuschl.packages.\${pkgs.stdenv.hostPlatform.system}.call-site";
      description = "call.html (served at /call) and its assets; null serves only 404s.";
    };
    sfu = {
      enable = lib.mkEnableOption "the companion SFU: signalling at /sfu, media on one UDP port";
      package = mkOption {
        type = types.package;
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.stream-sfu;
        defaultText = lib.literalExpression "inputs.nuschl.packages.\${pkgs.stdenv.hostPlatform.system}.stream-sfu";
      };
      port = mkOption {
        type = types.port;
        default = 3479;
        description = "The UDP port of all media.";
      };
      addresses = mkOption {
        type = types.listOf types.str;
        example = [
          "203.0.113.10"
          "2001:db8::10"
        ];
        description = "Public addresses peers reach the media port at, one host candidate each.";
      };
      openFirewall = lib.mkEnableOption "the media port";
      trafficBudget = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        example = 20000000000000;
        description = ''
          Hard SFU UDP egress budget in bytes per UTC period, or null for no
          budget. The rate follows the remaining budget and time, with burst
          credit from unused elapsed share. Media stops when the budget is
          spent. Reservations persist in the state directory.
        '';
      };
      period = mkOption {
        type = types.enum [
          "day"
          "month"
        ];
        default = "month";
        description = "The traffic budget's period, in UTC.";
      };
      turn = {
        urls = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [ "turn:turn.nuschl.eu:3478?transport=udp" ];
          description = "TURN URLs handed to admitted peers.";
        };
        secretFile = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "/run/agenix/nuschl-turn-rest-secret";
          description = "Runtime path of the REST secret shared with services.nuschl-turn; null hands out no TURN credentials.";
        };
        lifetime = limit 600 "Seconds a TURN credential stays valid.";
      };
      settings = mkOption {
        default = { };
        description = "Further nuschl-stream-sfu.toml settings (apps/stream-sfu/src/config.rs).";
        type = types.submodule {
          freeformType = toml.type;
          options = {
            max_rooms = limit 1024 "Rooms at once; a room lasts until it expires.";
            max_peers_per_room = limit 16 "Peers per room.";
            max_sources_per_peer = limit 3 "Tracks one peer may publish.";
            room_lifetime_secs = limit 14400 "Seconds from a room's creation to its end.";
            lease_secs = limit 30 "Seconds a room lasts without a lease renewal from its creator's device.";
            max_message_bytes = limit 262144 "The largest signalling message.";
            messages_per_second = limit 32 "Signalling messages per peer and second (twice this as a burst).";
            creations_per_hour = limit 20 "Room creations per client address (IPv6 per /64) and hour.";
            receiver_bitrate = limit 25000000 "Forwarded media per receiver, bits per second.";
            room_bitrate = limit 150000000 "Forwarded media per room, bits per second.";
            upload_bitrate = limit 1000000000 "The host's upload, bits per second; forwarding uses at most 70 % of it.";
            max_connections = limit 4096 "Signalling connections at once.";
            connections_per_address = limit 32 "Signalling connections at once per client address (IPv6 per /64).";
          };
        };
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          cfg.sfu.turn.secretFile == null || !(lib.hasPrefix builtins.storeDir cfg.sfu.turn.secretFile);
        message = "services.nuschl-stream.sfu.turn.secretFile must be a runtime path outside the Nix store.";
      }
      {
        assertion = cfg.sfu.turn.secretFile == null || cfg.sfu.turn.urls != [ ];
        message = "services.nuschl-stream.sfu.turn.secretFile needs turn.urls.";
      }
    ];

    services.nginx = {
      enable = true;
      virtualHosts.${cfg.hostName} = {
        inherit (cfg) enableACME forceSSL;
        root = cfg.site;
        extraConfig = ''
          access_log off;
          ${headers "()"}
        '';
        locations =
          (
            if cfg.site == null then
              { "/".return = "404"; }
            else
              {
                "/".tryFiles = "$uri =404";
                "= /call" = {
                  tryFiles = "/call.html =404";
                  extraConfig = headers "(self)";
                };
              }
          )
          // lib.optionalAttrs cfg.sfu.enable {
            "= /sfu" = {
              proxyPass = "http://unix:${socket}:";
              proxyWebsockets = true;
              extraConfig = ''
                proxy_set_header X-Real-IP $remote_addr;
                proxy_buffering off;
              '';
            };
            "= /sfu/lease" = {
              proxyPass = "http://unix:${socket}:";
              extraConfig = ''
                ${headers "()"}
                # The host key authorizes a lease. The browser receives only its HTTP status.
                add_header Access-Control-Allow-Origin "*" always;
                proxy_set_header X-Real-IP $remote_addr;
                client_max_body_size 1k;
              '';
            };
          };
      };
    };

    # A system user (not a dynamic one: nginx must share its group to reach the
    # socket). It owns nothing but the socket's directory.
    users.users = lib.mkIf cfg.sfu.enable {
      nuschl-stream-sfu = {
        isSystemUser = true;
        group = "nuschl-stream-sfu";
      };
      ${config.services.nginx.user}.extraGroups = [ "nuschl-stream-sfu" ];
    };
    users.groups.nuschl-stream-sfu = lib.mkIf cfg.sfu.enable { };

    systemd.services.nuschl-stream-sfu = lib.mkIf cfg.sfu.enable {
      description = "Nuschl companion SFU";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      before = [ "nginx.service" ];
      restartTriggers = [ sfuConfig ];
      serviceConfig = {
        ExecStart = lib.escapeShellArgs [
          "${cfg.sfu.package}/bin/nuschl-stream-sfu"
          "--config"
          sfuConfig
        ];
        LoadCredential = lib.optional (
          cfg.sfu.turn.secretFile != null
        ) "turn-secret:${cfg.sfu.turn.secretFile}";
        User = "nuschl-stream-sfu";
        Group = "nuschl-stream-sfu";
        RuntimeDirectory = "nuschl-stream-sfu";
        StateDirectory = "nuschl-stream-sfu";
        StateDirectoryMode = "0700";
        RuntimeDirectoryMode = "0750";
        UMask = "0007";
        Restart = "on-failure";
        RestartSec = "2s";
        LimitNOFILE = 16384;
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        PrivateIPC = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        CapabilityBoundingSet = "";
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
        ];
        MemoryMax = "2G";
      };
    };

    networking.firewall.allowedUDPPorts = lib.mkIf (cfg.sfu.enable && cfg.sfu.openFirewall) [
      cfg.sfu.port
    ];
  };
}
