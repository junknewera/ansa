{
  description = "ansa: native Rust local agent";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/7fd4004d0511991fff3a0524856a9be09e28c5f0";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "aarch64-linux" "x86_64-linux" ];
      each = nixpkgs.lib.genAttrs systems;
      analysisFor = pkgs: pkgs.python3.withPackages (ps: [ ps.matplotlib ]);
      rustPackage = system:
        let pkgs = import nixpkgs { inherit system; }; in
        pkgs.rustPlatform.buildRustPackage {
          pname = "ansa";
          version = "0.1.0";
          src = nixpkgs.lib.cleanSourceWith {
            src = ./rust;
            filter = path: type:
              nixpkgs.lib.cleanSourceFilter path type
              && !(builtins.elem (baseNameOf path) [ "AGENTS.md" "README.md" ]);
          };
          cargoLock.lockFile = ./rust/Cargo.lock;
          strictDeps = true;
          nativeBuildInputs = [ pkgs.python3 ];
          nativeCheckInputs = [ pkgs.gitMinimal pkgs.zsh ];
          cargoBuildFlags = [ "--workspace" ];
          cargoTestFlags = [ "--workspace" ];
          doCheck = true;
          postInstall = ''
            ln -s ansa "$out/bin/ansa-rs"
          '';
          ANSA_PARITY_ROOT = ./tests/parity;
          ANSA_PROTOCOL_FIXTURES = ./tests/fixtures;
          ANSA_AUTH_FIXTURES = ./tests/fixtures/auth;
          ANSA_SESSION_FIXTURES = ./tests/fixtures/sessions;
          ANSA_ANALYSIS_PYTHON = "${analysisFor pkgs}/bin/python3";
          preBuild = ''
            python3 ${./tests/parity/check_toolchain.py} --workspace .
          '';
          meta.mainProgram = "ansa";
        };
    in {
      packages = each (system: {
        default = rustPackage system;
        ansa-rust = rustPackage system;
      });
      checks = each (system: {
        rust = rustPackage system;
      });
      devShells = each (system:
        let pkgs = import nixpkgs { inherit system; }; in {
          default = pkgs.mkShell {
            packages = [ pkgs.rustc pkgs.cargo pkgs.rustfmt pkgs.clippy (analysisFor pkgs) pkgs.gitMinimal pkgs.zsh ];
            ANSA_ANALYSIS_PYTHON = "${analysisFor pkgs}/bin/python3";
          };
        } // pkgs.lib.optionalAttrs (system == "aarch64-linux") {
          quality = pkgs.mkShell {
            packages = [ (pkgs.callPackage ./nix/quality-toolchain.nix {}) (analysisFor pkgs) pkgs.gitMinimal pkgs.zsh ];
            ANSA_ANALYSIS_PYTHON = "${analysisFor pkgs}/bin/python3";
          };
        });
    };
}
