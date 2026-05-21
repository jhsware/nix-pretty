# terminal-wrapper-for-nix

A small macOS / Linux helper, written in Rust, that wraps `bash` inside a
nix-shell and collapses every Nix store path that scrolls past in the terminal
down to the short, readable form `nix:`. The hash-heavy paths that Nix prints
are noisy and distract from the actual log message. This wrapper makes them
disappear without touching the underlying tooling.

The binary is called `nix-pretty`. It spawns the configured shell in a
pseudo-terminal, forwards stdin verbatim and rewrites the shell's output on the
fly. Everything else - colours, line editing, interactive prompts, full-screen
TUIs - keeps working because we run the shell on a real PTY.

## What gets rewritten

Every Nix store path the shell emits is collapsed to `nix:` plus whatever
trailing path component followed the package name:

```
before: /nix/store/3p5l9d7v3w7nq2x9jk8m5a7s8b1234567-coreutils-9.5/bin/ls
after:  nix:/bin/ls
```

The rewriter recognises a store path as the literal prefix `/nix/store/`,
followed by at least 32 lowercase alphanumeric characters (the Nix hash),
followed by `-`, followed by one or more package-name characters
(`[A-Za-z0-9._+-]`, the alphabet every derivation in nixpkgs uses). Anything
that does not match this grammar - including bare `/nix/store` mentions that
are not real store paths - passes through untouched.

The rewriter operates on raw bytes and is safe for streams that contain ANSI
colour escapes, UTF-8 text, or partial reads where a store path lands across
two chunks.

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

* The collapsed `nix:` form is one-way - the hash and package name are gone
  from the terminal. If you need to copy a full store path you can still get
  it from the underlying tool, for example with `nix path-info` or
  `realpath`. The rewriter only affects what the shell prints; on-disk paths
  are untouched.
* The pattern is hard-coded. There is no `--pattern` flag, and no escape
  hatch in the terminal stream to ask for the original path back. If you
  want the original output, run the command without the wrapper.
* macOS and Linux only. There is no Windows code path; PTY semantics differ
  enough that supporting it well is a separate project.
* The wrapper allocates a single PTY pair per session. If your workflow forks
  many short-lived shells (for example through `xargs bash -c ...`), wrap the
  outermost shell rather than each child.

## Security considerations

`nix-pretty` is a **byte-stream rewriter, not a sanitiser**. It is deliberately
transparent to everything except the `/nix/store/<hash>-<pkg>` grammar:

* ANSI / CSI colour and cursor escape sequences pass through unchanged.
* OSC sequences (terminal title, hyperlinks, clipboard on emulators that
  support OSC 52) pass through unchanged.
* DCS and APC sequences pass through unchanged.
* UTF-8 and arbitrary binary bytes pass through unchanged.

The wrapper therefore does **not** protect against terminal-escape injection
attacks (CWE-150, CVE-2025-58160, CVE-2025-55193, CVE-2025-55754 and similar)
in output produced by the wrapped shell or programs it runs. A user running
the same program *without* `nix-pretty` would see the exact same escape bytes
on their terminal, so the wrapper does not add new risk — but it also does
not remove the existing risk.

If you need to view untrusted output (CI logs, build output of unknown
derivations, hostile files) safely, run that program outside `nix-pretty`
in a dedicated terminal emulator with escape filtering, or pipe it through
a tool that strips control sequences first.

The remaining security properties (PTY isolation of the child, bounded
buffers, `setsid` + `TIOCSCTTY` to defeat `TIOCSTI` push-back) are
documented in detail in [SECURITY.md](SECURITY.md).

## Development

```
nix-shell
cargo test            # full test suite, including streaming rewriter tests
cargo clippy -- -D warnings
cargo fmt --check
```

The rewriter lives in `src/rewriter.rs` and has exhaustive unit tests covering
the headline example, every possible chunk-boundary split, near-misses (short
hash, wrong separator, empty package name, case-mismatched prefix), pkg names
with the full punctuation alphabet, ANSI colour escapes around and inside
matches, UTF-8 multi-byte text, a large pseudo-random payload checked against
a non-streaming reference oracle, and the bounded-candidate-buffer safety
property. The PTY glue lives in `src/pty.rs` behind `#[cfg(unix)]`.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design rationale.

## Licence

Dual-licensed under MIT or Apache-2.0 at your option.
