{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    # x86_64 Linux only: intel-media-driver and vpl-gpu-rt exist nowhere else, and libav-sys has
    # a committed FFmpeg config for that target alone.
    flake-utils.lib.eachSystem [ "x86_64-linux" ] (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.nightly."2026-07-30".default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.pkg-config
            pkgs.cargo-nextest
            pkgs.cargo-fuzz
            pkgs.mold
            pkgs.clang
            pkgs.libva-utils
            pkgs.nasm
            pkgs.meson
            pkgs.ninja
          ];

          buildInputs = [ pkgs.libva pkgs.libvpl ];

          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "cc";
          RUSTFLAGS = "-C link-arg=-fuse-ld=mold";
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          # The test binaries load libva and libvpl at run time and carry no rpath to the store.
          LD_LIBRARY_PATH = "${pkgs.libva.out}/lib:${pkgs.libvpl}/lib";
          ONEVPL_SEARCH_PATH = "${pkgs.vpl-gpu-rt}/lib";
          LIBVA_DRIVERS_PATH = "${pkgs.intel-media-driver}/lib/dri:${pkgs.mesa}/lib/dri";
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
