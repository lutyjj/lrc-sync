{
  lib,
  rustPlatform,
  aws-lc,
  cacert,
  pkg-config,
  sqlite,
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

  # rustls' aws-lc-rs and the cache's SQLite, linked to nixpkgs' copies:
  # nixpkgs' cargo hook asks aws-lc-sys for the system's AWS-LC, and with
  # pkg-config libsqlite3-sys finds the system's SQLite.
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [
    aws-lc
    sqlite
  ];

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
