{ lib, stdenv, fetchurl, autoPatchelfHook, zlib, zstd, openssl, curl, libxml2 }:
let
  pin = builtins.fromJSON (builtins.readFile ./quality-toolchain.json);
  components = lib.mapAttrsToList (_: value: fetchurl {
    inherit (value) url sha256;
  }) pin.components;
in stdenv.mkDerivation {
  pname = "ansa-quality-nightly";
  version = pin.date;
  dontUnpack = true;
  strictDeps = true;
  nativeBuildInputs = [ autoPatchelfHook ];
  buildInputs = [ stdenv.cc.cc.lib zlib zstd openssl curl libxml2 ];
  installPhase = ''
    mkdir -p "$out"
    for component in ${lib.escapeShellArgs (map toString components)}; do
      mkdir unpack
      tar -xJf "$component" -C unpack --strip-components=1
      sh unpack/install.sh --prefix="$out" --disable-ldconfig
      rm -r unpack
    done
  '';
  meta.platforms = [ "aarch64-linux" ];
}
