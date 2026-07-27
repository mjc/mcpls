{
  description = "MCPLS development and benchmark environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = {
    self,
    nixpkgs,
    ...
  }: let
    systems = [
      "aarch64-darwin"
      "aarch64-linux"
      "x86_64-darwin"
      "x86_64-linux"
    ];
    forAllSystems = function:
      builtins.listToAttrs (
        map (system: {
          name = system;
          value = function system;
        })
        systems
      );
  in {
    packages = forAllSystems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      gungraun-runner = pkgs.rustPlatform.buildRustPackage rec {
        pname = "gungraun-runner";
        version = "0.19.4";
        src = pkgs.fetchCrate {
          inherit pname version;
          hash = "sha256-DrIbeUVI+fhrp87rzIxYRvAlPSJ3ksa6cHHNFg4I+zE=";
        };
        cargoHash = "sha256-68SL8pEYw9nV9g3ZmUjWDL9DXOBDfSMy1y3ZuKuHW2I=";
        doCheck = false;
      };
    in {
      inherit gungraun-runner;
      default = gungraun-runner;
    });

    devShells = forAllSystems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      default = pkgs.mkShell {
        packages = with pkgs;
          [
            actionlint
            cargo
            cargo-nextest
            clippy
            self.packages.${system}.gungraun-runner
            python3
            rust-analyzer
            rustc
            rustfmt
          ]
          ++ lib.optionals stdenv.isLinux [
            valgrind
          ];
      };
    });
  };
}
