{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.lrc-sync;
  inherit (lib) mkOption types;
  toEnv = value: if lib.isBool value then lib.boolToString value else toString value;
in
{
  options.services.lrc-sync = {
    enable = lib.mkEnableOption "lrc-sync, which writes LRCLib lyrics beside a music library";

    package = lib.mkPackageOption pkgs "lrc-sync" { };

    musicDir = mkOption {
      type = types.path;
      description = "Library root. The service writes `.lrc` sidecars here, so it needs write access.";
    };

    user = mkOption {
      type = types.str;
      default = "lrc-sync";
      description = "User the service runs as. Created when left at the default.";
    };

    group = mkOption {
      type = types.str;
      default = "lrc-sync";
      description = "Group the service runs as; give it write access to `musicDir`.";
    };

    settings = mkOption {
      type = types.submodule {
        freeformType = types.attrsOf (
          types.oneOf [
            types.str
            types.int
            types.bool
          ]
        );
        options = {
          LRCSYNC_WATCH_MODE = mkOption {
            type = types.enum [
              "native"
              "poll"
            ];
            default = "native";
            description = "Use `poll` on NFS or other filesystems that deliver no change events.";
          };
          LRCSYNC_ORPHAN_ACTION = mkOption {
            type = types.enum [
              "keep"
              "reconcile"
              "quarantine"
              "delete"
            ];
            default = "keep";
            description = "What to do with sidecars that match no audio file.";
          };
          LRCSYNC_REQUEST_TIMEOUT_SECONDS = mkOption {
            type = types.ints.between 1 300;
            default = 30;
            description = "LRCLib request timeout. Shutdown waits up to this long for an active request.";
          };
        };
      };
      default = { };
      example = {
        LRCSYNC_WATCH_MODE = "poll";
        LRCSYNC_CONCURRENCY = 4;
      };
      description = ''
        `LRCSYNC_*` environment variables, as documented in the README.
        `LRCSYNC_MUSIC_DIR` and `LRCSYNC_DB_PATH` are set by the module.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !(cfg.settings ? LRCSYNC_MUSIC_DIR || cfg.settings ? LRCSYNC_DB_PATH);
        message = "services.lrc-sync: set musicDir instead of LRCSYNC_MUSIC_DIR; the database lives in the state directory.";
      }
    ];

    users.users = lib.mkIf (cfg.user == "lrc-sync") {
      lrc-sync = {
        isSystemUser = true;
        inherit (cfg) group;
      };
    };
    users.groups = lib.mkIf (cfg.group == "lrc-sync") { lrc-sync = { }; };

    systemd.services.lrc-sync = {
      description = "lrc-sync lyrics synchronisation";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      unitConfig.RequiresMountsFor = [ cfg.musicDir ];

      environment = lib.mapAttrs (_: toEnv) cfg.settings // {
        LRCSYNC_MUSIC_DIR = toString cfg.musicDir;
        LRCSYNC_DB_PATH = "/var/lib/lrc-sync/cache.sqlite3";
      };

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        User = cfg.user;
        Group = cfg.group;
        StateDirectory = "lrc-sync";
        # New sidecars stay writable by the library's group.
        UMask = "0002";
        # The watcher exits on failure; a restart rebuilds its coverage.
        Restart = "on-failure";
        RestartSec = 10;
        # An in-flight LRCLib request may run to its timeout before exit.
        TimeoutStopSec = cfg.settings.LRCSYNC_REQUEST_TIMEOUT_SECONDS + 15;

        ReadWritePaths = [ cfg.musicDir ];
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        PrivateTmp = true;
        PrivateDevices = true;
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
        ];
        CapabilityBoundingSet = "";
      };
    };
  };
}
