# nix-pretty

A small macOS / Linux helper, written in Rust, that wraps `bash` inside a
nix-shell and collapses every Nix store path that scrolls past in the
terminal: the noisy hash is dropped, the human-readable package name and
the trailing path component are kept. The hash-heavy paths that Nix prints
distract from the actual log message; this wrapper makes them legible
without touching the underlying tooling.

The binary is called `nix-pretty`. It spawns the configured shell in a
pseudo-terminal, forwards stdin verbatim and rewrites the shell's output on the
fly. Everything else - colours, line editing, interactive prompts, full-screen
TUIs - keeps working because we run the shell on a real PTY.

## What gets rewritten

Every Nix store path the shell emits is collapsed to `nix:` plus the
package name plus whatever trailing path component followed:

```
before: /nix/store/3p5l9d7v3w7nq2x9jk8m5a7s8b1234567-coreutils-9.5/bin/ls
after:  nix:coreutils-9.5/bin/ls
```

The hash is dropped (it is essentially noise to a human reader), but the
package name is kept so a `PATH` or `-L` list that contains many store
paths remains scannable rather than collapsing to a wall of identical
`nix:` tokens.

The rewriter recognises a store path as the literal prefix `/nix/store/`,
followed by at least 32 lowercase alphanumeric characters (the Nix hash),
followed by `-`, followed by one or more package-name characters
(`[A-Za-z0-9._+-]`, the alphabet every derivation in nixpkgs uses). Anything
that does not match this grammar - including bare `/nix/store` mentions that
are not real store paths - passes through untouched.

The rewriter operates on raw bytes and is safe for streams that contain ANSI
colour escapes, UTF-8 text, or partial reads where a store path lands across
two chunks.

## What you see vs. what the shell actually receives

The rewriter only runs on bytes flowing from the PTY master back to your
real terminal. Everything else — stdin, internal shell pipelines, captured
command output, argv, environment variables, on-disk paths — is untouched.

This has two consequences that are worth spelling out, because they look
surprising at first glance.

### Pasted or typed paths look rewritten on screen

If you paste `/nix/store/<hash>-coreutils-9.5/bin/ls` at the bash prompt,
what you see on screen is `nix:coreutils-9.5/bin/ls`. The full path is
still what bash receives, and pressing Enter will run the real `/bin/ls`.
The reason for the visual mismatch is how a PTY works:

```
keypress  ->  real stdin  ->  PTY master  ->  PTY slave  ->  bash
                                                              |
                                                              v
                                                          echo back
                                                              |
real stdout  <-  rewriter  <-  PTY master  <-  PTY slave  <---+
```

`nix-pretty` never rewrites bytes going *into* bash. It only rewrites bytes
coming *out* of the PTY master. Bash (via readline) and the kernel's line
discipline echo your typed/pasted characters back through that same master
fd so you can see what you're typing — and from the rewriter's point of
view those echoed bytes are output, indistinguishable from anything else
the shell printed. So they get collapsed the same way a build log would.

The actual command bash executes is fine. Only the visual echo is collapsed.

### stderr is rewritten too

A child started under `nix-pretty` has fd 1 and fd 2 both `dup2`'d to the
same slave PTY (see `src/pty.rs`, `child_after_fork`). After that, stdout
and stderr are multiplexed onto a single byte stream on the master side,
with no marker telling them apart. The rewriter therefore sees a merged
stream and collapses store paths in both.

The grammar is strict (`/nix/store/<32+-char-hash>-<pkg>`), so error
messages that don't contain a literal store path are unaffected. Only the
path text itself collapses; everything around it — file names, line
numbers, the actual error description — passes through verbatim.

If you need the original, uncollapsed stderr for a single command, run that
command outside `nix-pretty` (e.g. by exiting the wrapped shell and
re-running it from your normal shell, or by piping through `cat` from a
non-wrapped terminal).

### Scripts passing paths around are unaffected

The rewriter is purely a display-side transform. Anything that stays inside
the shell process never goes near it:

* `path=$(nix-build .)` captures the real store path into `$path`.
* `nix-build . | grep something` pipes real bytes between processes; the
  rewriter never sees the pipe.
* `cp "$path/bin/foo" ./bar` passes the real path as argv to `cp`.
* Files on disk, exported env vars, scripted `read`/`exec` invocations —
  all use the original paths.

The only place a path is collapsed is the byte stream printed to your
terminal. If a script then tries to *read its own terminal output back* to
recover a path it just printed, that won't work — but no real script does
that; scripts use variables.

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

### Local builds with `./build.sh`

For day-to-day use - building a fresh binary and dropping it on your `PATH` -
the repo ships a small helper at `./build.sh`. The first time, mark it
executable:

```
chmod +x build.sh
```

After that, a normal cycle is one command:

```
./build.sh
```

That:

1. Re-execs itself inside `nix-shell` if `shell.nix` is present and you are
   not already inside one, so the pinned Rust toolchain is used.
2. Bumps the **patch** version in `Cargo.toml` (`--minor` / `--major` /
   `--no-bump` are available too).
3. Runs `cargo build --release`.
4. Commits **only** `Cargo.toml` and `Cargo.lock` with the message
   `chore(release): local build vX.Y.Z`. Anything else in your worktree is
   left untouched.
5. Installs the resulting `target/release/nix-pretty` to `/usr/local/bin`
   on both macOS and Linux. That path is in the default `PATH` on both
   OSes and makes no assumption about Homebrew, MacPorts or any other
   package manager being present. The script uses `sudo` for the final
   copy step only when the destination is not writable.

The script **never** creates a git tag and **never** pushes. Tagging is
reserved for official releases that go through a separate, human-driven
workflow.

Common variations:

```
./build.sh --minor                  # 0.1.4 -> 0.2.0 instead of 0.1.5
./build.sh --no-bump --no-commit    # just rebuild & reinstall current source
./build.sh --prefix "$HOME/.local/bin"
./build.sh --no-install             # build only, do not touch /usr/local/bin
./build.sh --no-nix-shell           # use whatever cargo is already in PATH
./build.sh --help                   # full flag list
```

`PREFIX=/some/dir ./build.sh` is equivalent to `./build.sh --prefix /some/dir`.

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
    # ... whatever else your project already does in shellHook ...

    # Re-exec the current shell under nix-pretty, but only once and only
    # when we are attached to a terminal. We pin NIX_PRETTY_SHELL to an
    # absolute path from nixpkgs so the wrapper does not resolve `bash`
    # through $PATH - that closes the PATH-hijack vector documented in
    # SECURITY.md §4.2.
    if [ -z "$NIX_PRETTY_ACTIVE" ] && [ -t 1 ] && command -v nix-pretty >/dev/null 2>&1; then
      NIX_PRETTY_ACTIVE=1
      NIX_PRETTY_SHELL=${pkgs.bashInteractive}/bin/bash
      NIX_PRETTY_OUTER_PS1=$PS1
      export NIX_PRETTY_ACTIVE NIX_PRETTY_SHELL NIX_PRETTY_OUTER_PS1

      # Hand the wrapped bash an rcfile via process substitution. The
      # body is single-quoted so $NIX_PRETTY_OUTER_PS1 is written
      # literally and expanded by the WRAPPED bash from its env at
      # startup. No mktemp, no on-disk file.
      exec nix-pretty --rcfile <(printf '%s\n' \
        '[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"' \
        'PS1=$NIX_PRETTY_OUTER_PS1') -i
    fi
  '';
}
```

The guard variable `NIX_PRETTY_ACTIVE` prevents an infinite re-exec loop. The
`[ -t 1 ]` check skips the wrapper when the shell is run non-interactively
(for example by editor tooling that scrapes the environment). The
`command -v nix-pretty` guard makes the hook a no-op when `nix-pretty` is
not (yet) installed - useful both on fresh checkouts and on machines where
you have not built the binary.

Pinning `NIX_PRETTY_SHELL` to `${pkgs.bashInteractive}/bin/bash` (which Nix
evaluates to an immutable `/nix/store/.../bin/bash` path) means
`nix-pretty` never consults `$PATH` to find the shell. If you would
rather use a system shell, give the absolute path explicitly — for
example `NIX_PRETTY_SHELL=/run/current-system/sw/bin/bash` on NixOS or
`NIX_PRETTY_SHELL=/bin/bash` on a Linux system. Avoid leaving the
default (bare `bash`) for any setup where `$PATH` could contain a
writable directory ahead of the real `bash`'s location.

The `--rcfile` dance preserves nix-shell's `[nix-shell:~/...]$` prompt
across the exec into the wrapped bash. Without it, the wrapped bash
would read your normal `~/.bashrc` and reset `PS1` to your everyday
prompt, which is correct but loses the visual cue that you are inside
a nix-shell. Process substitution `<(...)` plumbs the rcfile body
through an anonymous pipe (`/dev/fd/N`) instead of a named temp file,
so nothing is written to disk - the helper subprocess that produced
the bytes exits as soon as the wrapped bash reads them, and the OS
reclaims the pipe.

The argument order `--rcfile <(...) -i` (long option first, then short
option) is deliberate. `bash` will only recognise long options that
appear *before* any short option on the command line - swapping to
`-i --rcfile <(...)` makes bash bail with `--: invalid option`. This
is documented in `man bash` under INVOCATION: "These options must
appear on the command line before the single-character options to
be recognized."

Process substitution itself is a bash extension (not POSIX). Since
`shellHook` always runs in bash and the wrapped shell is also bash,
this is fine in practice; the rcfile body inside the `<(...)` is
POSIX-clean (`[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"`, plain
`PS1=$...`) so it will keep working even if a future bash tightens
its POSIX mode.

If your project's `shell.nix` already has a `shellHook` for other purposes
(GPG setup, env vars, status messages), append the nix-pretty block to the
existing hook rather than replacing it - `exec` transfers control and any
lines after it are dead code. This project's own `shell.nix` is a working
example of that pattern.

## Limitations

* The collapsed `nix:<pkg>` form is one-way: the hash is dropped from the
  display. The package name is kept (so `nix:coreutils-9.5/bin/ls` tells
  you which package a binary came from), but if you need the *exact*
  hash-bearing store path back, get it from the underlying tool — for
  example with `nix path-info` or `realpath`. The rewriter only affects
  what the shell prints; on-disk paths are untouched.
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
cargo audit           # supply-chain CVE check (also runs in CI daily)
```

The rewriter lives in `src/rewriter.rs` and has exhaustive unit tests covering
the headline example, every possible chunk-boundary split, near-misses (short
hash, wrong separator, empty package name, case-mismatched prefix), pkg names
with the full punctuation alphabet, ANSI colour escapes around and inside
matches, UTF-8 multi-byte text, a large pseudo-random payload checked against
a non-streaming reference oracle, and the bounded-candidate-buffer safety
property. The PTY glue lives in `src/pty.rs` behind `#[cfg(unix)]`.

Supply-chain hygiene is enforced by `cargo audit` running in CI
(`.github/workflows/audit.yml`) on every push and PR, plus a daily
scheduled run that catches new RustSec advisories even on a quiet
repository.
See [ARCHITECTURE.md](ARCHITECTURE.md) for the design rationale.

## Licence

Dual-licensed under MIT or Apache-2.0 at your option.
