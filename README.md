# terminal-wrapper-for-nix

A small macOS / Linux helper, written in Rust, that wraps `bash` inside a
nix-shell so every `/nix/store` path that scrolls past in the terminal is
rewritten to `[nix-store]`. The hash-heavy paths that Nix prints are noisy and
distract from the actual log message. This wrapper makes them disappear without
touching the underlying tooling.

The binary is called `nix-pretty`. It spawns the configured shell in a
pseudo-terminal, forwards stdin verbatim and rewrites the shell's output on the
fly. Everything else - colours, line editing, interactive prompts, full-screen
TUIs - keeps working because we run the shell on a real PTY.

## What gets rewritten

Every literal occurrence of `/nix/store` in the shell's output stream is
replaced with `[nix-store]`. The store hash and package name are preserved so
the rewritten path is still useful for copying or pasting back into commands
that operate on store paths.

```
before: /nix/store/3p5l9d7v3w7nq2x9jk8m5a7s8b1234567-coreutils-9.5/bin/ls
after:  [nix-store]/3p5l9d7v3w7nq2x9jk8m5a7s8b1234567-coreutils-9.5/bin/ls
```

The rewriter operates on raw bytes and is safe for streams that contain ANSI
colour escapes, UTF-8 text, or partial reads where the literal `/nix/store`
ends up split across two chunks.

## Requirements

A Unix-like system with a working PTY (macOS or Linux). Nix is not required at
runtime - the wrapper is just a string rewriter - but the primary use case is
running it inside a `nix-shell` started from a project's `shell.nix`.

## Building from source

The project ships with a `shell.nix` that pins the Rust toolchain through niv,
so the recommended workflow is:

```
nix-shell        # drops you in a shell with rustc, cargo, clippy, rustfmt
cargo build --release
```

The binary lands in `target/release/nix-pretty`. Copy it anywhere on `PATH`,
for example `~/.local/bin/`.

If you would rather use your system Rust toolchain, `cargo build --release`
works equally well outside `nix-shell`. The only build dependencies are the
crates listed in `Cargo.toml`; there is no C build step.

## Using it

### As an explicit command

Inside any shell, run:

```
nix-pretty
```

That starts an interactive `bash -i` inside a PTY and pipes its output through
the rewriter. You can pass arguments through to the shell:

```
nix-pretty -c 'nix-build .'         # one-shot command, output is rewritten
nix-pretty -l                       # login shell
```

The default shell is `bash` (resolved through `PATH`). To use a different
shell, set `NIX_PRETTY_SHELL`:

```
NIX_PRETTY_SHELL=/run/current-system/sw/bin/bash nix-pretty
```

The wrapper exits with the same status code as the wrapped shell.

### As a `shell.nix` hook

To make every `nix-shell` for a project run inside the wrapper automatically,
add this to the project's `shell.nix`:

```nix
{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  # ... your packages, env vars, etc ...

  shellHook = ''
    # Re-exec the current shell under nix-pretty, but only once and only when
    # we are attached to a terminal.
    if [ -z "$NIX_PRETTY_ACTIVE" ] && [ -t 1 ] && command -v nix-pretty >/dev/null; then
      export NIX_PRETTY_ACTIVE=1
      exec nix-pretty
    fi
  '';
}
```

The guard variable `NIX_PRETTY_ACTIVE` prevents an infinite re-exec loop. The
`[ -t 1 ]` check skips the wrapper when the shell is run non-interactively
(for example by editor tooling that scrapes the environment).

## Limitations

* The wrapper rewrites the literal ten-character prefix `/nix/store`. It does
  not try to parse hashes, validate package names, or follow symlinks. That is
  by design: it keeps the implementation small, fast and impossible to fool
  with weird input.
* macOS and Linux only. There is no Windows code path; PTY semantics differ
  enough that supporting it well is a separate project.
* The wrapper allocates a single PTY pair per session. If your workflow forks
  many short-lived shells (for example through `xargs bash -c ...`), wrap the
  outermost shell rather than each child.

## Development

```
nix-shell
cargo test            # full test suite, including streaming rewriter tests
cargo clippy -- -D warnings
cargo fmt --check
```

The rewriter lives in `src/rewriter.rs` and has exhaustive unit tests covering
empty input, normal matches, multiple matches in one chunk, splits across
arbitrary chunk boundaries, ANSI colour sequences, large random payloads and
adversarial near-matches. The PTY glue lives in `src/pty.rs` behind
`#[cfg(unix)]`.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design rationale.

## Licence

Dual-licensed under MIT or Apache-2.0 at your option.
