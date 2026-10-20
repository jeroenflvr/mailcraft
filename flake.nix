{
  description = "MailCraft: package, dev shell, and NixOS module";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    let
      eachSystem = flake-utils.lib.eachDefaultSystem;
    in
    (eachSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustToolchain =
          pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rustfmt" "clippy" ];
          };
      in
      {
        packages.default = pkgs.callPackage ./mailcraft.nix {};
        packages.mailcraft = pkgs.callPackage ./mailcraft.nix {};

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.mailcraft ];

          packages = with pkgs; [
            rustToolchain
            cargo-nextest
            trunk
            wasm-bindgen-cli
            binaryen
            taplo
            just
          ];

          shellHook = ''
            export RUST_BACKTRACE=1
            export LIBCLANG_PATH="${pkgs.clang.lib}/lib"
            echo "MailCraft dev shell ready"
          '';
        };
      }))
    //
    {
      nixosModules.default = { pkgs, ... }: {
        nixpkgs.overlays = [ (final: prev: { mailcraft = prev.callPackage ./mailcraft.nix {}; }) ];
        environment.systemPackages = [ pkgs.mailcraft ];
      };

      overlays.default = final: prev: {
        mailcraft = prev.callPackage ./mailcraft.nix {};
      };
    };
}
