{
  lib,
  rustPlatform,
  cmake,
  cacert,
}:
let
  cargo = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = cargo.package.name;
  inherit (cargo.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../tests
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # aws-lc-sys, reqwest's rustls crypto provider, builds with CMake.
  nativeBuildInputs = [ cmake ];
  dontUseCmakeConfigure = true;

  # The HTTP client loads the system CA store when built, tests included.
  nativeCheckInputs = [ cacert ];

  meta = {
    description = "Fetch LRCLib lyrics into .lrc sidecars for a music library";
    homepage = "https://github.com/lutyjj/lrc-sync";
    license = lib.licenses.asl20;
    mainProgram = "lrc-sync";
    platforms = lib.platforms.linux;
  };
}
