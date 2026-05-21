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

    # Re-exec under nix-pretty so /nix/store paths in command output
    # collapse to `nix:`. POSIX-clean: no `printf %q`, no `mktemp -t`,
    # no `[[ ... ]]`, no here-strings, no bash array tricks.
    if [ -z "$NIX_PRETTY_ACTIVE" ] && [ -t 1 ] && command -v nix-pretty >/dev/null 2>&1; then
      NIX_PRETTY_ACTIVE=1
      NIX_PRETTY_SHELL=${pkgs.bashInteractive}/bin/bash
      # Pass nix-shell's PS1 through the environment so the rcfile that
      # re-applies it never has to quote/escape the value.
      NIX_PRETTY_OUTER_PS1=$PS1
      export NIX_PRETTY_ACTIVE NIX_PRETTY_SHELL NIX_PRETTY_OUTER_PS1

      # `mktemp` is not in POSIX, but the form `mktemp DIR/PREFIX.XXXXXX`
      # is accepted by both BSD (macOS) and GNU (Linux) implementations.
      _nixpretty_rc=$(mktemp "''${TMPDIR:-/tmp}/nixpretty-rc.XXXXXX")

      # Single-quoted heredoc delimiter `'RCFILE'` means no expansion at
      # write time - $NIX_PRETTY_OUTER_PS1 is written literally and the
      # wrapped bash expands it at startup. The body is itself POSIX:
      # `[ -f ... ]`, `.` (the source builtin), no bash-isms.
      cat > "$_nixpretty_rc" <<'RCFILE'
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
PS1=$NIX_PRETTY_OUTER_PS1
RCFILE

      # IMPORTANT: long options must come BEFORE short options on a
      # bash command line, otherwise bash bails with `--: invalid
      # option`. Documented in `man bash`, INVOCATION: "These options
      # must appear on the command line before the single-character
      # options to be recognized." So `--rcfile FILE -i`, not the
      # other way around.
      exec nix-pretty --rcfile "$_nixpretty_rc" -i
    fi
  '';
}