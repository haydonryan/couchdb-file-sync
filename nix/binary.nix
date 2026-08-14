{ lib
, system
, fetchurl
, stdenv
, autoPatchelfHook
, glibc
}:

# Installs the prebuilt `couchdb-file-sync` release artifact (fast path, no
# compilation). Artifacts are published by the release CI
# (.github/workflows/release.yml) as `couchdb-file-sync-<target>.tar.gz` for the
# Linux targets, each archive containing just the `couchdb-file-sync` binary.

let
  # Centralized per-platform mapping: Nix system -> release artifact.
  # `target` is the Rust release target, `hash` is the sha256 (SRI) of the
  # published tarball for v0.5.0.
  artifacts = {
    x86_64-linux = {
      target = "x86_64-unknown-linux-gnu";
      url = "https://github.com/haydonryan/couchdb-file-sync/releases/download/v0.5.0/couchdb-file-sync-x86_64-unknown-linux-gnu.tar.gz";
      hash = "sha256-wYvZOsYy2X7qTIhUa+MU5Qy/H581rwbE0G/IWnbelbY=";
    };
    aarch64-linux = {
      target = "aarch64-unknown-linux-gnu";
      url = "https://github.com/haydonryan/couchdb-file-sync/releases/download/v0.5.0/couchdb-file-sync-aarch64-unknown-linux-gnu.tar.gz";
      hash = "sha256-9a3eOZrmjbqbaCRlPVqQgSvBgizr2sn/zVAvgHMg8a4=";
    };
  };

  art = artifacts.${system} or (throw ''
    couchdb-file-sync: app-bin is unsupported on system "${system}".
    Supported systems: ${lib.concatStringsSep ", " (lib.attrNames artifacts)}.
    (app-src builds from source on any supported Linux system.)
  '');
in
stdenv.mkDerivation {
  pname = "couchdb-file-sync";
  version = "0.5.0";

  src = fetchurl {
    url = art.url;
    sha256 = art.hash;
  };

  # The release tarball contains a single flat file (no top-level directory).
  sourceRoot = ".";

  # The glibc build dynamically links glibc/libgcc (libc, libm, libgcc_s), so
  # rewrite its interpreter and library paths for the Nix store.
  nativeBuildInputs = [ autoPatchelfHook ];
  buildInputs = [ glibc stdenv.cc.cc.lib ];

  installPhase = ''
    runHook preInstall
    install -Dm755 couchdb-file-sync $out/bin/couchdb-file-sync
    runHook postInstall
  '';

  meta = with lib; {
    description = "Filesystem-to-CouchDB sync engine with bidirectional sync and conflict detection (prebuilt release)";
    homepage = "https://github.com/haydonryan/couchdb-file-sync";
    license = licenses.mit;
    mainProgram = "couchdb-file-sync";
    platforms = [ "x86_64-linux" "aarch64-linux" ];
  };
}
