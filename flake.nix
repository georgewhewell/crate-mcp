{
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    naersk.url = "github:nix-community/naersk";
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    flake-utils,
    naersk,
    nixpkgs,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          config = {
            allowUnfree = true;
          };
          overlays = [
            rust-overlay.overlays.default
          ];
        };

        toolchain = pkgs.rust-bin.selectLatestNightlyWith (toolchain:
          toolchain.default.override {
            extensions = ["rust-src" "rust-analyzer"];
          });

        naersk' = pkgs.callPackage naersk {
          cargo = toolchain;
          rustc = toolchain;
        };
      in {
        packages = {
          inherit toolchain;
        };

        defaultPackage = naersk'.buildPackage {
          src = ./.;
        };

        devShell = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            toolchain
            pkg-config
          ];

          buildInputs = with pkgs; [openssl curl];

          shellHook = ''
            set -euo pipefail
            echo "rustc path: $(which rustc)"
          '';
        };
      }
    );
}
