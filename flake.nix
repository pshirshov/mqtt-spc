{
  description = "SPC alarm panel to MQTT bridge for Home Assistant";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f {
        pkgs = nixpkgs.legacyPackages.${system};
      });
    in
    {
      devShells = forAllSystems ({ pkgs }: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
          ];
        };
      });

      packages = forAllSystems ({ pkgs }: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "spc-mqtt";
          version = "0.1.0";
          src = ./.;
          useFetchCargoVendor = true;
          cargoHash = "sha256-Vo2HKwDtXBkNWjbZT7ZG+qUt8OCWOLeHsYyEpYCXtEg=";
        };
      });
    };
}
