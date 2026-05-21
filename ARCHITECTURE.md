# Architecture

This document describes how `nix-pretty` is structured, the principles that
shaped the design, the trade-offs that were considered and the way the code
is laid out so that future contributors can navigate it quickly.

## What the program does, in one paragraph

`nix-pretty` allocates a pseudo-terminal (PTY), forks, lets the child exec a
shell (default: `bash`) on the slave side of the PTY, and runs a small event
loop on the parent side. The parent copies bytes from the user's real
terminal (`stdin`) into the PTY master, and copies bytes from the PTY master
into the user's real terminal (`stdout`) after streaming them through a
byte-level rewriter that replaces every literal `/nix/store` with
`[nix-store]`. When the child exits, the parent restores the original
terminal settings and propagates the child's exit code.

## Guiding principles

The stated priorities for the project, in order, are safety, ease of use,
performance, ease of distribution and maintainability. Every non-trivial
design choice below ties back to one or more of these.

### Safety first

Safety here means three concrete things. First, the wrapper must never
corrupt the byte stream flowing through it: ANSI escape sequences, UTF-8 text,
binary data piped through `cat`, partial reads that split the search pattern
across two buffers - none of those may produce output the original shell
would not. Second, the wrapper must restore the user's terminal to a sane
state on every exit path, including panics and unexpected child deaths;
otherwise an aborted run leaves the user's terminal in raw mode and they
need to type `reset` blind. Third, the wrapper must not introduce a new
attack surface: there is no parsing of untrusted input, no shelling out, no
network I/O.

These constraints push us towards a tiny, well-typed core with most of the
logic in pure-Rust functions that can be tested deterministically.

### Ease of use

A user should be able to run `nix-pretty` and feel that nothing has changed
except that the noisy hashes are gone. Interactive shells, colour, readline,
job control, full-screen TUIs like `htop` or `vim` all need to keep working.
That requirement, more than anything else, forces the PTY-based design.

### Performance

The wrapper sits on the hot path between every command the user runs and the
text they see. It needs to add no perceptible latency and no measurable CPU
overhead during normal interactive use. In practice this means a single-pass
byte scanner with no allocation per match and no regex engine.

### Ease of distribution

The whole project compiles to a single statically-linkable Rust binary. We
deliberately avoid native C build steps, plugin systems or per-user config
files. The release profile is tuned (`lto = "thin"`, `strip = "symbols"`,
`codegen-units = 1`, `panic = "abort"`) for a small binary.

### Maintainability

The code is split into a pure library (`rewriter`) and a thin platform shim
(`pty`). The library is fully covered by unit tests. The shim is small enough
to read in one sitting and uses well-known POSIX primitives through the
`nix` crate so that the same code works on macOS and Linux without per-OS
branches.

## Module layout

```
src/
  lib.rs        re-exports the public API
  rewriter.rs   pure streaming /nix/store -> [nix-store] transform
  pty.rs        Unix PTY plumbing, fork/exec, raw-mode RAII, event loop
  main.rs       argument parsing, env reading, calls into pty::run
```

The library has no platform-specific code; the binary is the only place
that talks to a PTY.

## The rewriter

The rewriter is a small state machine that exposes three operations:
`process(input, output)`, `flush(output)` and `new()`. It buffers a
**bounded** tail of unflushed bytes (at most `len(pattern) - 1 = 9` bytes)
so that a literal `/nix/store` split across two reads still gets matched.

The matching strategy is the obvious one: linear scan, compare against
the fixed pattern, emit the replacement on a hit, otherwise emit one byte
and advance. The reason this is fast enough is that the pattern is short,
ASCII, and starts with `/`, which is rare in normal terminal output, so the
loop spends nearly all its time emitting bytes one-for-one and almost never
doing a full pattern compare.

We deliberately do **not** use a regex engine. The behaviour is simpler to
reason about, the binary is smaller, the dependency surface is smaller, and
the rewriter is provably correct for the trivial case of a single fixed
pattern.

## The PTY layer

On Unix the parent process:

1. Reads the current terminal's `winsize` and `termios` from `stdin`.
2. Calls `openpty()` (via the `nix` crate) with that `winsize`, producing a
   master / slave file-descriptor pair.
3. Puts `stdin` into raw mode through a RAII guard
   (`RawModeGuard`). The guard captures the original `termios` and restores
   it from its `Drop` impl, so even a panic returns the terminal to its
   pre-run state.
4. `fork()`s. The child closes the master fd, becomes a session leader via
   `setsid()`, acquires the slave as its controlling terminal with the
   `TIOCSCTTY` ioctl, dups the slave to fds 0/1/2, and `execvp`s the shell.
   The parent closes the slave and keeps the master.
5. Spawns a background thread that copies bytes from real-stdin into the
   PTY master. This forwarding is one-way and verbatim - no rewriting is
   ever done on input. When the parent exits, this thread is left to die
   with the process; it is intentionally not joined because it would be
   blocked on a `read()` from the controlling tty.
6. Spawns a SIGWINCH listener thread (via `signal-hook`) that propagates
   window-size changes from the real terminal to the PTY by issuing
   `TIOCSWINSZ` on the master. Without this, programs running on the
   slave would not see resize events.
7. On the main thread, reads from the PTY master into a fixed-size buffer,
   feeds each chunk through a `PathRewriter`, and writes the result to real
   stdout, flushing after each write so latency stays low.
8. When the master read returns 0 (or `EIO` on Linux when the slave has
   closed), the loop flushes the rewriter's tail buffer and falls through
   to `waitpid()`. The child's exit code, or `128 + signal_number` for a
   signalled exit, becomes the wrapper's own exit code.

The whole event loop is straight-line, single-threaded apart from the two
small helper threads, and contains no `unsafe` outside the `nix`/`libc`
calls that wrap POSIX syscalls.

## Trade-offs that were considered

### PTY versus pipes

A pipe-based wrapper would have been ~50 lines of code. It would have meant
running the shell with stdout connected to a pipe whose reader rewrites
bytes and forwards them. The problem is that bash and almost every program
the user will run check `isatty(1)` and change their behaviour accordingly:
no colours, no prompt redrawing, no readline, no full-screen TUIs. The
result is a wrapper that nobody wants to use. We accept the extra complexity
of a real PTY because it is the difference between "useful" and "not".

### Regex versus fixed-string scan

A regex like `/nix/store/[0-9a-z]{32}-[^/]+` would allow stripping the hash
and the package-version segment as well. We considered it and rejected it
for two reasons. First, the user asked for the simpler `/nix/store` ->
`[nix-store]` rewrite, which preserves enough of the path that the result
is still usable (you can paste it back into commands that operate on store
paths, after restoring the prefix). Second, a regex engine pulls in a much
larger dependency and a much larger binary, in exchange for behaviour that
is harder to predict on adversarial input. A fixed-string scan with a tiny
state machine is easier to test exhaustively.

### Sync poll loop versus async runtime

We deliberately do not pull in `tokio` or `async-std`. The wrapper has at
most three concurrent activities - stdin to master, master to stdout, and
SIGWINCH handling - and they map naturally onto two helper threads plus the
main loop. An async runtime would multiply binary size and dependency count
for no observable benefit.

### Streaming buffer size

We hold back at most `len(pattern) - 1 = 9` bytes between calls to
`process()`. That is the minimum buffer that can guarantee correctness
across arbitrary chunk boundaries, and it bounds the wrapper's latency: in
the worst case the user sees output 9 bytes later than they otherwise
would. In practice the bound is hit only when a `/nix/store` literal lands
at the exact end of a chunk.

### What we do not handle

* **Windows.** Win32 console PTY (ConPTY) semantics are different enough to
  warrant a separate code path. The project is Unix-only by design.
* **Per-line buffering of programs that bypass the PTY.** Programs that
  write directly to `/dev/tty` go around our master fd. There is no
  cross-platform way to intercept that without LD_PRELOAD-style tricks,
  which would violate the safety priority.
* **Configurable patterns.** The pattern is hard-coded. Making it
  configurable would add a config file or a CLI flag, both of which would
  add surface area for bugs in exchange for a feature nobody asked for.

## Dependencies

The `Cargo.toml` lists three runtime dependencies. Each one earns its place:

* **`nix`** wraps the POSIX calls (`openpty`, `tcgetattr`/`tcsetattr`,
  `fork`, `execvp`, `waitpid`, `ioctl`) in safe Rust types. Replacing it
  would mean re-implementing those wrappers ourselves; `nix` is widely used
  and audited, so we lean on it.
* **`libc`** is pulled in for a handful of constants (`TIOCSCTTY`,
  `TIOCSWINSZ`, `winsize`) that `nix` does not re-export ergonomically.
* **`signal-hook`** gives us an async-signal-safe SIGWINCH listener thread
  with no race-prone hand-rolled signal handler.

There are no `[dev-dependencies]`. Tests use only `std`.

## Testing strategy

The rewriter has exhaustive unit tests in `src/rewriter.rs`. The matrix
covers:

* empty input, single-byte input, input shorter than the pattern;
* input that does not contain the pattern, input that contains the
  pattern once, twice or many times;
* every possible chunk-boundary split inside the pattern, generated by a
  loop that splits a known good input at each byte position;
* input mixed with ANSI escape sequences and UTF-8 multi-byte text;
* adversarial near-matches like `/nix/stor`, `/nix/storage`,
  `/nix/store/nix/store`;
* a large random payload that should be reproduced byte-for-byte;
* the `flush()` invariant that after a full pass `process()` + `flush()`
  the cumulative output equals `pattern_replace(input)`.

The PTY layer is not unit-tested directly. It is deliberately kept small
enough to audit by eye, and its bytes-through-rewriter contract is exercised
by a `pipe_through` helper in the library that uses the same rewriter
plumbing the PTY loop does, only with a `Read` and a `Write` instead of a
PTY master fd. That helper has its own integration test.

## Failure modes and how we respond to them

| Failure | Response |
| --- | --- |
| Cannot allocate a PTY | Print a clear error to stderr; exit 1. |
| Shell binary not found | `execvp` fails in the child, which exits 127; the parent observes that exit code and propagates it. |
| Panic in the parent | The `RawModeGuard`'s `Drop` impl restores the terminal; the panic message is printed via the default hook. |
| SIGWINCH arrives during shutdown | The signal thread eventually returns when the main thread exits and closes the master fd. |
| Master read returns `EIO` | Treated as EOF; flush the rewriter tail and exit normally. |
| Slave closed but child still alive (long-running background process) | `waitpid()` blocks until the child actually exits, which is the correct shell-wrapper semantics. |

## Future work

* A `--pattern` / `--replacement` flag if anyone ever asks for it.
* Optional rewriting of `/nix/store/<hash>-` to also drop the hash, behind
  a flag. Implementation would extend the state machine to consume 32 lowercase
  base-32 characters plus a trailing dash after a confirmed prefix match.
* Integration test that uses a real PTY (via `posix_openpt`) to spawn the
  binary in CI. Currently the build is verified manually because allocating
  a PTY in CI requires runner-specific setup.
