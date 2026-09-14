{
  pkgs ? import <nixpkgs> { overlays = [ (import <rust-overlay>) ]; }
}:
let
  toolchain = pkgs.rust-bin.nightly.latest.default.override {
    targets = [ "x86_64-unknown-linux-gnu" "i686-unknown-linux-gnu" ];
    extensions = [ "rust-src" "rust-analyzer" "clippy" ];
  };
in
pkgs.mkShell {
  nativeBuildInputs = [
    toolchain
    pkgs.pkg-config
    pkgs.nodejs
    pkgs.bun
    pkgs.opus
    pkgs.cmake
    pkgs.openssl
    pkgs.gcc_multi
    pkgs.llvmPackages_21.libclang
  ];

  CARGO_TARGET_I686_UNKNOWN_LINUX_GNU_LINKER = "${pkgs.pkgsi686Linux.stdenv.cc}/bin/cc";
  LIBCLANG_PATH = "${pkgs.llvmPackages_21.libclang.lib}/lib";
}
