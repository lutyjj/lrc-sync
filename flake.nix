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

      nixosModules.default = {
        imports = [ ./nix/module.nix ];
        nixpkgs.overlays = [ self.overlays.default ];
      };

      checks = forAllSystems (pkgs: {
        package = self.packages.${pkgs.system}.default;
        module = pkgs.testers.runNixOSTest {
          name = "lrc-sync";
          nodes.machine = {
            imports = [ self.nixosModules.default ];
            systemd.tmpfiles.rules = [ "d /srv/music 0775 root users -" ];
            services.lrc-sync = {
              enable = true;
              musicDir = "/srv/music";
              group = "users";
            };
          };
          testScript = ''
            machine.wait_for_unit("lrc-sync.service")
            machine.wait_until_succeeds("journalctl -u lrc-sync | grep -q 'initial library scan queued'")
            machine.succeed("test -f /var/lib/lrc-sync/cache.sqlite3")
            machine.succeed("systemctl stop lrc-sync")
            machine.succeed("systemctl show -p Result --value lrc-sync | grep -qx success")
          '';
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
