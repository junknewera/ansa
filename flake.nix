{
  description = "ansa: C++20 local agent and Rust migration scaffold";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/7fd4004d0511991fff3a0524856a9be09e28c5f0";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "aarch64-linux" "x86_64-linux" ];
      each = nixpkgs.lib.genAttrs systems;
      package = system:
        let pkgs = import nixpkgs { inherit system; }; in
        pkgs.stdenv.mkDerivation {
          pname = "ansa";
          version = "0.1.0";
          src = nixpkgs.lib.cleanSource ./.;
          strictDeps = true;
          nativeBuildInputs = [ pkgs.cmake pkgs.ninja pkgs.pkg-config ];
          buildInputs = [ pkgs.curl pkgs.yyjson pkgs.openssl ];
          cmakeFlags = [ "-DBUILD_TESTING=ON" ];
          doCheck = true;
          checkPhase = "ctest --output-on-failure --no-tests=error";
        };
      rustPackage = system:
        let pkgs = import nixpkgs { inherit system; }; in
        pkgs.rustPlatform.buildRustPackage {
          pname = "ansa-rust";
          version = "0.1.0";
          src = nixpkgs.lib.cleanSource ./rust;
          cargoLock.lockFile = ./rust/Cargo.lock;
          strictDeps = true;
          nativeBuildInputs = [ pkgs.python3 ];
          cargoBuildFlags = [ "--workspace" ];
          cargoTestFlags = [ "--workspace" ];
          doCheck = true;
          preBuild = ''
            python3 ${./tests/parity/check_toolchain.py} --workspace .
          '';
          meta.mainProgram = "ansa-rs";
        };
    in {
      packages = each (system: {
        default = package system;
        ansa-rust = rustPackage system;
      });
      checks = each (system: {
        protocol = package system;
        rust = rustPackage system;
      });
      devShells = each (system:
        let pkgs = import nixpkgs { inherit system; }; in {
          default = pkgs.mkShell {
            inputsFrom = [ (package system) ];
            packages = [ pkgs.rustc pkgs.cargo pkgs.rustfmt pkgs.clippy pkgs.python3 ];
          };
        });
    };
}
