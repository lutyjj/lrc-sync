{
  description = "Fetch LRCLib lyrics into .lrc sidecars for a music library";

  inputs.nixpkgs.url = "https://channels.nixos.org/nixos-26.05/nixexprs.tar.xz";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        lrc-sync = pkgs.callPackage ./nix/package.nix { };
        default = lrc-sync;
      });

      overlays.default = final: _: { lrc-sync = final.callPackage ./nix/package.nix { }; };

      # Defaults the package rather than adding the overlay, which read-only
      # pkgs (nixpkgs.pkgs, runNixOSTest) would reject.
      nixosModules.default =
        { lib, pkgs, ... }:
        {
          imports = [ ./nix/module.nix ];
          services.lrc-sync.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };

      checks = forAllSystems (pkgs: {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        module = pkgs.testers.runNixOSTest {
          name = "lrc-sync";
          nodes.machine = {
            imports = [ self.nixosModules.default ];
            # An unmatched sidecar exercises a write under the service's sandbox.
            systemd.tmpfiles.rules = [
              "d /srv/music 0775 root users -"
              "f /srv/music/orphan.lrc 0664 root users - [00:01.00] orphan"
            ];
            services.lrc-sync = {
              enable = true;
              musicDir = "/srv/music";
              group = "users";
              settings.LRCSYNC_ORPHAN_ACTION = "quarantine";
            };
          };
          testScript = ''
            machine.wait_for_unit("lrc-sync.service")
            machine.wait_until_succeeds("test -f /srv/music/.lrcsync-orphans/orphan.lrc")
            machine.succeed("test ! -e /srv/music/orphan.lrc")
            machine.succeed("test -f /var/lib/lrc-sync/cache.sqlite3")
            machine.succeed("systemctl is-active lrc-sync")
            machine.succeed("systemctl stop lrc-sync")
            machine.succeed("systemctl show -p Result --value lrc-sync | grep -qx success")
          '';
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
