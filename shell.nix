{ sources ? import ./nix/sources.nix
, pkgs ? import sources.nixpkgs { }
}:

# Rust development environment for `nix-pretty`, a small terminal wrapper that
# rewrites `/nix/store` paths in PTY output so logs stay readable inside
# nix-shell.
#
# Everything here is pinned through `nix/sources.json` (managed by niv).
pkgs.mkShell {
  name = "terminal-wrapper-for-nix-dev";

  # Toolchain. We deliberately stick to the Rust packages that ship with the
  # pinned nixpkgs to keep the dev environment fully reproducible without an
  # extra overlay or rustup.
  packages = with pkgs; [
    rustc
    cargo
    rustfmt
    clippy
    rust-analyzer
    # Useful for debugging the PTY layer locally.
    pkg-config
  ];

  # Helpful defaults for an interactive shell session.
  RUST_BACKTRACE = "1";

  shellHook = ''
    echo "terminal-wrapper-for-nix dev shell"
    echo "  rustc:  $(rustc --version 2>/dev/null || echo not found)"
    echo "  cargo:  $(cargo --version 2>/dev/null || echo not found)"
    echo
    echo "Common commands:"
    echo "  cargo build           - debug build"
    echo "  cargo build --release - optimized build"
    echo "  cargo test            - run the test suite"
    echo "  cargo clippy          - lint"
    echo "  cargo fmt             - format"
  '';
}
