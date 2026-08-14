{ lib
, rustPlatform
, src
, cacert
}:

# Builds `couchdb-file-sync` from source. `src` is the flake source (the whole
# repository); the single crate's Cargo files are consumed.
rustPlatform.buildRustPackage {
  pname = "couchdb-file-sync";
  version = "0.5.0";

  inherit src;

  # The sandboxed build has no system CA store, but the `reqwest`/`couch_rs`
  # clients load CA certificates eagerly at client-builder time (even for
  # plain `http://` URLs). Provide a CA bundle so the test suite's client
  # construction does not fail with "No CA certificates were loaded from the
  # system".
  buildInputs = [ cacert ];
  env = {
    SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
    NIX_SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
  };

  # The committed Cargo.lock is used with --locked, so the build resolves no
  # dependencies from the network; everything comes from the Nix store.
  cargoLock = {
    lockFile = ./../Cargo.lock;
  };

  # rusqlite bundles its own sqlite3 and reqwest is built with rustls (no
  # OpenSSL), so no pkg-config or system libraries are required to build or run
  # the binary. The test suite (dry-run, conflict detection, remote moves) is
  # self-contained, so the default `cargo test` check phase runs.

  meta = with lib; {
    description = "Filesystem-to-CouchDB sync engine with bidirectional sync and conflict detection";
    homepage = "https://github.com/haydonryan/couchdb-file-sync";
    license = licenses.mit;
    mainProgram = "couchdb-file-sync";
    platforms = [ "x86_64-linux" "aarch64-linux" ];
  };
}
