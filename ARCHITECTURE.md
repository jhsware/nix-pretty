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
byte-level rewriter that collapses every `/nix/store/<hash>-<pkg>` segment
to `nix:` followed by the matched pkg name (e.g. `nix:coreutils-9.5`),
leaving any trailing path component (`/bin/ls` and similar) untouched. When
the child exits, the parent restores the original terminal settings and
propagates the child's exit code.

## Guiding principles

The stated priorities for the project, in order, are safety, ease of use,
performance, ease of distribution and maintainability. Every non-trivial
design choice below ties back to one or more of these.

### Safety first

Safety here means three concrete things. First, the wrapper must never
corrupt the byte stream flowing through it: ANSI escape sequences, UTF-8 text,
binary data piped through `cat`, partial reads that split a candidate match
across two buffers - none of those may produce output the original shell
would not. Second, the wrapper must restore the user's terminal to a sane
state on every exit path, including panics and unexpected child deaths;
otherwise an aborted run leaves the user's terminal in raw mode and they
need to type `reset` blind. This is achieved by a pair of restoration
mechanisms — a `RawModeGuard` RAII guard for unwinding panics and clean
shutdown, and a process-wide panic hook for the `panic = "abort"` path
used by the release profile (see "The PTY layer" below). Third, the
wrapper must not introduce a new attack surface: there is no parsing of
untrusted input beyond the small state machine described below, no
shelling out, no network I/O.

These constraints push us towards a tiny, well-typed core with most of the
logic in pure-Rust functions that can be tested deterministically.

### Ease of use

A user should be able to run `nix-pretty` and feel that nothing has changed
except that the noisy store paths are gone. Interactive shells, colour,
readline, job control, full-screen TUIs like `htop` or `vim` all need to
keep working. That requirement, more than anything else, forces the
PTY-based design.

### Performance

The wrapper sits on the hot path between every command the user runs and the
text they see. It needs to add no perceptible latency and no measurable CPU
overhead during normal interactive use. In practice this means a single-pass
byte scanner with no per-byte allocation and no regex engine.

### Ease of distribution

The whole project compiles to a single statically-linkable Rust binary. We
deliberately avoid native C build steps, plugin systems or per-user config
files. The release profile is tuned (`lto = "thin"`, `strip = "symbols"`,
`codegen-units = 1`, `panic = "abort"`) for a small binary. Because
`panic = "abort"` skips `Drop`, the parent installs a panic hook on entry
to `pty::run` so the terminal still gets restored to its pre-raw mode
before the process aborts; see "The PTY layer" for the details.

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
  rewriter.rs   pure state-machine rewriter (store-path -> `nix:<pkg>`)
  pty.rs        Unix PTY plumbing, fork/exec, raw-mode RAII, event loop
  main.rs       argument parsing, env reading, calls into pty::run
```

The library has no platform-specific code; the binary is the only place
that talks to a PTY.

## The rewriter

The rewriter recognises a small regular grammar:

```
storepath := "/nix/store/" hash "-" pkg
hash      := [0-9a-z]{32,}            ; greedy, at least 32 chars
pkg       := [A-Za-z0-9._+-]+         ; at least one pkg char
```

When the rewriter sees a complete `storepath`, it emits `nix:` followed by
the matched `pkg` bytes (e.g. `nix:coreutils-9.5`) and discards the prefix
and hash. Anything else passes through verbatim. A trailing path component
(e.g. `/bin/ls`) is not part of the match, so it survives in the output
unchanged.

The implementation is a hand-rolled state machine with five states:

```
Idle  --PREFIX[0]-->  Prefix(matched)
                             |
                  matched == PREFIX.len()
                             v
                      Hash(count) <-----+ is_hash_char(b)
                             |          |
                  b == '-' && count >= 32
                             v          |
                        WantPkg  -------+
                             |
                       is_pkg_char(b)
                             v
                       Pkg(count) <-----+ is_pkg_char(b)
                             |          |
                   !is_pkg_char(b), count > 0
                             v
                      commit "nix:" + pkg
                             |
                             v
                    (re-process b in Idle)
```

While the rewriter is inside a candidate match, every consumed byte is
appended to an internal `buffer`. A successful match calls `commit`, which
emits `REPLACEMENT` (the literal `nix:`) followed by the matched pkg bytes
from the tail of `buffer`, then clears the buffer. Any failed transition
(wrong byte for the current state, hash too short, missing dash, empty
pkg) calls `bail`, which emits the buffer verbatim, resets to `Idle`, and
re-processes the offending byte so that a stray `/` after a half-matched
prefix can still start a new candidate match.

A hard cap (`MAX_CANDIDATE_LEN = 1024` bytes) bounds the in-flight buffer.
Once it is hit the rewriter bails. This keeps memory use bounded under
adversarial input (e.g. an unbounded run of hash characters after a prefix
that never sees its dash) and is comfortably above any real store path.

The rewriter is pure: no I/O, no syscalls, no allocation per byte beyond
the bounded buffer, and **no `unsafe`** — the module declares
`#![deny(unsafe_code)]` so the property is enforced by the compiler
rather than by convention. All chunk-split correctness lives in
`process`, and `flush` handles the end-of-input edge case where a
complete `Pkg` candidate needs to be committed even though no
terminator byte was seen.

## The PTY layer

On Unix the parent process:

0. Installs a process-wide panic hook
   (`install_panic_termios_restore`). The hook reads a small
   `OnceLock<Mutex<Option<(RawFd, Termios)>>>` populated in step 3 below
   and issues a best-effort `tcsetattr(TCSANOW, &original)` on the outer
   tty before chaining to the default hook. This is the mitigation for
   `panic = "abort"` skipping `Drop`: without it, a panic in the parent
   would leave the user in raw mode (see SECURITY.md §4.6).
1. Reads the current terminal's `winsize` and `termios` from `stdin`.
2. Calls `openpty()` (via the `nix` crate) with that `winsize`, producing a
   master / slave file-descriptor pair.
3. Puts `stdin` into raw mode through a RAII guard
   (`RawModeGuard`). The guard captures the original `termios` and
   restores it from its `Drop` impl **and** publishes a copy of
   `(fd, original)` to the panic-hook state from step 0, so the
   terminal is restored on every documented exit path: clean exit,
   child crash, parent crash with unwinding, parent crash with abort.
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
   closed), the loop flushes the rewriter's buffered candidate and falls
   through to `waitpid()`. The child's exit code, or `128 + signal_number`
   for a signalled exit, becomes the wrapper's own exit code.

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

### Single PTY: input echo and stderr ride on the output stream

A consequence of the single-PTY design that is worth stating explicitly,
because it surprises new readers: the only stream the rewriter ever sees
is "bytes coming out of the PTY master." That stream multiplexes three
logically different things that the child process produced independently:

1. The shell's stdout (`fd 1`).
2. The shell's stderr (`fd 2`). In `child_after_fork` we `dup2` the slave
   onto both `fd 1` and `fd 2`, so once the shell has run a single
   `write(2, ...)` the bytes are indistinguishable from `write(1, ...)`
   on the master side.
3. The echo of the user's typed/pasted input. The PTY line discipline
   (and bash's readline, when it is in charge of echoing) sends typed
   characters back out through the slave so the user can see what they
   are typing. From the master fd's point of view those echoed bytes are
   simply more output.

The wrapper never touches stdin on its way *in* — `spawn_stdin_forwarder`
copies bytes verbatim to the master. But it cannot tell, on the way *out*,
whether a given byte originated as program stdout, program stderr, or the
echo of a keystroke. The rewriter therefore collapses store paths
uniformly, including in the echo of a paste and in stderr error messages.

The behavioural implications are documented in README.md ("What you see
vs. what the shell actually receives"). The short version: what `bash`
*receives* from a paste is always the real, untouched bytes — only what
the user *sees* echoed back is collapsed. Splitting the three streams
back apart would mean either a second PTY (unusual and fights with
`setsid` / `TIOCSCTTY`), a separate pipe for stderr (loses `isatty`-gated
behaviours like coloured error output), or in-band tagging (requires
shell cooperation). None of those is worth the cost for the current
single-display-target use case.

### State machine versus regex

A regex like `/nix/store/[0-9a-z]{32,}-[A-Za-z0-9._+-]+` expresses the same
grammar in one line. We rejected it on two grounds. First, pulling in a
regex engine roughly doubles the binary size and the dependency surface for
behaviour we already get from ~120 lines of straight-line Rust. Second, a
regex on a streaming byte source still needs the same chunk-boundary
buffering and bail logic; the engine does not save us much code, only
syntax. The hand-rolled state machine is also easier to test exhaustively
and to reason about under adversarial input.

### Lenient hash, strict separator

Nix's actual base-32 alphabet is a 26-character subset of `[0-9a-z]` (no
`e`, `o`, `t`, `u`). We accept the full `[0-9a-z]` alphabet for hash
characters instead. The cost of over-acceptance is zero: any byte sequence
that satisfies our lenient hash rule but is not a real store path will fail
the subsequent `-` or `pkg` check and bail cleanly, emitting the original
bytes. The benefit is robustness against any future Nix format tweak.

The hash length is `>= 32` rather than `== 32` for the same reason: future
proofing, at no observable cost.

### Sync threads versus async runtime

We deliberately do not pull in `tokio` or `async-std`. The wrapper has at
most three concurrent activities - stdin to master, master to stdout, and
SIGWINCH handling - and they map naturally onto two helper threads plus the
main loop. An async runtime would multiply binary size and dependency count
for no observable benefit.

### Candidate buffer size

The in-flight buffer holds the bytes of an as-yet-undecided match. It can
grow to roughly `len(PREFIX) + len(hash) + 1 + len(pkg)`. For real-world
store paths that is at most ~120 bytes; we bound it at 1024 bytes to defend
against adversarial input that tries to make the rewriter swallow arbitrary
memory. Hitting the bound bails the candidate, so behaviour on pathological
input remains correct (the bytes are emitted verbatim) while staying
bounded.

### What we do not handle

* **Windows.** Win32 console PTY (ConPTY) semantics are different enough to
  warrant a separate code path. The project is Unix-only by design.
* **Per-line buffering of programs that bypass the PTY.** Programs that
  write directly to `/dev/tty` go around our master fd. There is no
  cross-platform way to intercept that without LD_PRELOAD-style tricks,
  which would violate the safety priority.
* **Round-tripping the collapsed form back to a full path.** The hash is
  gone once the rewriter has emitted `nix:<pkg>`. The pkg name is kept (so
  the user can tell which package a line came from), but the exact
  hash-bearing store path is not recoverable from the display. If a tool
  needs the original path it should be invoked outside the wrapper.
* **Configurable patterns.** The grammar is hard-coded. Making it
  configurable would add a config file or a CLI flag, both of which would
  add surface area for bugs in exchange for a feature nobody has asked for.

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

Supply-chain hygiene for the small dependency set is enforced by
`cargo audit` running in CI (`.github/workflows/audit.yml`) on every
push to `master`, every PR, and on a daily 06:00 UTC schedule. The
scheduled run catches new RustSec advisories even on a quiet
repository, so a vulnerability disclosed against a pinned dep
surfaces in CI within a day without anyone having to think about it.

## Testing strategy

The rewriter has exhaustive unit tests in `src/rewriter.rs`. The matrix
covers:

* the canonical headline example (the exact `before -> after` from the
  requirements doc, pinned in a dedicated test so it can never silently
  regress);
* empty input, input that does not contain a store path, input that
  contains one, multiple, or many store paths in one chunk;
* pkg names that mix the full punctuation alphabet (`gtk+3-3.24.42_dev`);
* hashes longer than 32 characters (the "lenient hash" rule);
* near-matches that must pass through unchanged: short hash, wrong
  separator (`_` instead of `-`), missing pkg, missing leading slash,
  case-mismatched prefix, and the `/nix/storage` false friend;
* every possible chunk-boundary split point of a representative payload
  that contains both a real match and a near-miss; one-byte-at-a-time
  feeding of the headline example;
* `flush()` behaviour at end of input both in mid-match (emit buffered
  candidate verbatim) and at the end of a complete pkg (commit `nix:<pkg>`);
* the bounded-candidate-buffer property: feeding `2 * MAX_CANDIDATE_LEN`
  hash characters never grows the in-flight buffer past the cap, and the
  bail path emits the input verbatim;
* mixed content: ANSI colour escapes around and inside matches, UTF-8
  multibyte text around matches, an all-256-bytes binary payload that
  contains no store path and so must round-trip identity;
* a 10000-byte deterministic pseudo-random payload run both one-shot and
  in random-sized chunks against a non-streaming reference oracle that
  implements the same grammar.

The same coverage is mirrored for the `pipe_through` convenience helper:
basic match, empty input, mid-match flush, complete-pkg flush, small
fixed-chunk reads of every chunk size from 1 to N, `Interrupted`-retry,
and write-error propagation.

The PTY layer is not unit-tested end-to-end. It is deliberately kept small
enough to audit by eye, and its bytes-through-rewriter contract is exercised
by `pipe_through` against arbitrary byte streams. The PTY-specific
behaviours that we can test without forking a real shell — `RawModeGuard`
restoring termios on drop, the panic-hook restore state lifecycle, the
panic-hook restore path itself, and the winsize ioctl round-trip — have
direct unit tests against a freshly-allocated PTY pair.

## Failure modes and how we respond to them

| Failure | Response |
| --- | --- |
| Cannot allocate a PTY | Print a clear error to stderr; exit 1. |
| Shell binary not found | `execvp` fails in the child, which exits 127; the parent observes that exit code and propagates it. |
| Panic in the parent | A custom panic hook (`install_panic_termios_restore`) restores the outer terminal's termios from a saved `(fd, original)` pair before the default hook prints the panic message. The `RawModeGuard`'s `Drop` impl also restores on the unwinding path; both paths are covered so the terminal is sane on every documented exit. |
| SIGWINCH arrives during shutdown | The signal thread eventually returns when the main thread exits and closes the master fd. |
| Master read returns `EIO` | Treated as EOF; flush the rewriter buffer and exit normally. |
| Slave closed but child still alive (long-running background process) | `waitpid()` blocks until the child actually exits, which is the correct shell-wrapper semantics. |
| Adversarial input grows the candidate buffer | `MAX_CANDIDATE_LEN` bails the candidate; bytes are emitted verbatim and memory stays bounded. |

## Future work

* A `--pattern` / `--replacement` flag if anyone ever asks for it. Likely
  shape: `--pattern store-only` (the original `/nix/store -> [nix-store]`
  swap) versus `--pattern full` (the current default).
* Integration test that uses a real PTY (via `posix_openpt`) to spawn the
  binary in CI. Currently the build is verified manually because allocating
  a PTY in CI requires runner-specific setup.
* Optional buffered diagnostic mode that logs every bail to stderr behind
  an env var, for debugging "why didn't my path get rewritten".
