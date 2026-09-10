{ lib, rustPlatform, cmake }:

let
  cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  version = cargoToml.package.version;

  src = lib.cleanSource ./.;

  cargoLock.lockFile = ./Cargo.lock;

  # aws-lc-sys (the TLS client's crypto backend) builds AWS-LC's C sources
  # with cmake. Bindings are pregenerated for all four platforms this package
  # targets, so no bindgen/libclang is needed here.
  nativeBuildInputs = [ cmake ];

  # The test suite builds a rustls-backed kube::Client (even for the "fake"
  # test cluster), which needs native root CA certs — unavailable in Nix's
  # network-less build sandbox. Tests run properly in CI via `cargo test`;
  # this only skips re-running them inside the hermetic package build.
  doCheck = false;

  meta = {
    description = "A Kubernetes TUI, reimagined in Rust";
    homepage = "https://github.com/nklmilojevic/sofka";
    mainProgram = "sofka";
    platforms = lib.platforms.unix;
    license = with lib.licenses; [ mit asl20 ];
  };
}
