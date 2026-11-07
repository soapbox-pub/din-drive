{
  description = "Din Drive: an encrypted drive on Nostr and Blossom, mounted with FUSE";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "din-drive";
          version = "0.1.0";
          src = nixpkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: _type: baseNameOf path != "target";
          };
          cargoLock.lockFile = ./Cargo.lock;
          # The FUSE test needs /dev/fuse, which the sandbox lacks.
          checkFlags = [ "--skip" "fuse_" ];
          # The HTTPS client wants CA certificates even for local tests.
          nativeCheckInputs = [ pkgs.cacert ];
          preCheck = "export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        };
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt pkgs.gcc ];
        };
      });
    };
}
