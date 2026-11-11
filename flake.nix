{
  description = "ansa: independent C++20 local agent";
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
    in {
      packages = each (system: { default = package system; });
      checks = each (system: { protocol = package system; });
      devShells = each (system:
        let pkgs = import nixpkgs { inherit system; }; in {
          default = pkgs.mkShell {
            inputsFrom = [ (package system) ];
          };
        });
    };
}
