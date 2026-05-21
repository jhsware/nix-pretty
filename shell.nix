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

    # Re-exec the dev shell under nix-pretty so /nix/store paths in
    # command output collapse to `nix:`. The wrapper itself is the
    # thing this project builds, so the `command -v` guard means this
    # is a no-op on a fresh checkout that has not yet run `./build.sh`
    # to install the binary.
    if [ -z "$NIX_PRETTY_ACTIVE" ] && [ -t 1 ] && command -v nix-pretty >/dev/null 2>&1; then
      NIX_PRETTY_ACTIVE=1
      NIX_PRETTY_SHELL=${pkgs.bashInteractive}/bin/bash
      NIX_PRETTY_OUTER_PS1=$PS1
      export NIX_PRETTY_ACTIVE NIX_PRETTY_SHELL NIX_PRETTY_OUTER_PS1

      # Hand the wrapped bash an rcfile via process substitution. The
      # body is single-quoted so $NIX_PRETTY_OUTER_PS1 is written
      # literally and expanded by the WRAPPED bash from its env at
      # startup. No mktemp, no on-disk file.
      #
      # Long options must come BEFORE short options on a bash command
      # line - bash bails with `--: invalid option` otherwise. See
      # `man bash`, INVOCATION: "These options must appear on the
      # command line before the single-character options to be
      # recognized." So `--rcfile FILE -i`, not the other way around.
      exec nix-pretty --rcfile <(printf '%s\n' \
        '[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"' \
        'PS1=$NIX_PRETTY_OUTER_PS1') -i
    fi
  '';
}