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
    #
    # We pin NIX_PRETTY_SHELL to an absolute path from nixpkgs so the
    # wrapper does not resolve `bash` through $PATH - that closes the
    # PATH-hijack vector documented in SECURITY.md §4.2.
    #
    # The wrapped bash would normally read ~/.bashrc and reset PS1 to
    # your everyday prompt, dropping nix-shell's `[nix-shell:...]$`
    # prefix. We side-step that by handing it a small --rcfile that
    # sources ~/.bashrc first and then re-applies the PS1 nix-shell
    # already set for us.
    if [ -z "$NIX_PRETTY_ACTIVE" ] && [ -t 1 ] && command -v nix-pretty >/dev/null 2>&1; then
      export NIX_PRETTY_ACTIVE=1
      export NIX_PRETTY_SHELL=${pkgs.bashInteractive}/bin/bash

      _nixpretty_rc="$(mktemp -t nixpretty-rc.XXXXXX)"
      {
        echo '[ -f ~/.bashrc ] && . ~/.bashrc'
        printf 'PS1=%q\n' "$PS1"
      } > "$_nixpretty_rc"

      exec nix-pretty -i --rcfile "$_nixpretty_rc"
    fi
  '';
}