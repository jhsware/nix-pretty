# Security Architecture

| Field | Value |
| --- | --- |
| Document version | 1.1.3 |
| Last updated | 2026-05-21 |
| Status | Draft |
| Applies to | `nix-pretty` v0.1.0 (crate `nix-pretty`, repository `terminal-wrapper-for-nix`) |
| Maintainer | Project maintainers (see `Cargo.toml` `repository` field) |
| Versioning scheme | Semantic Versioning 2.0.0 (MAJOR.MINOR.PATCH). MAJOR bumps for any change that alters the threat model or removes content; MINOR for new sections, tables, attack vectors or recommendations; PATCH for editorial fixes that do not change meaning. |
| Review cadence | At least once per `nix-pretty` minor release, or whenever a new direct or transitive dependency is added. |

This document is a focused security analysis of `nix-pretty`, the small Rust
PTY wrapper described in [ARCHITECTURE.md](ARCHITECTURE.md). It complements
the architecture document by cataloguing the threat surface, walking through
known CVE classes that touch this kind of program, listing the concrete
attack vectors that apply to the current implementation, and pairing each
with the mitigation that is already in place or recommended.

The audience is reviewers, packagers and downstream consumers who want to
understand what the wrapper is and is not defending against before placing
it on the hot path between their shell and their terminal.

## 1. Scope and security goals

### 1.1 What the binary does, in security terms

`nix-pretty` runs at the privileges of the invoking user. It does the
following sensitive things and nothing else:

* Reads two pieces of caller-controlled input: the `NIX_PRETTY_SHELL`
  environment variable and the program's own `argv`.
* Allocates one pseudo-terminal pair through `openpty(3)`.
* `fork(2)`s and `execvp(3)`s a shell in the child.
* Puts the parent's stdin into raw mode through `tcsetattr(3)`.
* Copies bytes between the user's real terminal and the PTY master.
* Calls a single `ioctl(TIOCSWINSZ)` on the master on every `SIGWINCH`.
* `waitpid(2)`s the child and exits with its status.

It does not open files outside `/dev/ptmx` and the PTY pair, does not
perform any network I/O, does not parse untrusted data formats other than
the byte-level rewriter grammar, and does not run with elevated privileges
under any documented use case.

### 1.2 Security goals

In priority order:

1. **Integrity of the byte stream.** Output produced by the wrapped shell
   must reach the user's terminal byte-identical to what would have arrived
   without the wrapper, except for the explicitly documented
   `/nix/store/<hash>-<pkg>` → `nix:` collapse.
2. **Terminal-state safety.** The user's terminal must be returned to its
   pre-run termios on every exit path that the kernel can deliver: clean
   exit, child crash, parent crash, SIGINT, SIGTERM.
3. **Bounded resource use under adversarial input.** No input from the
   wrapped shell may cause the wrapper to consume unbounded memory or CPU,
   nor to block a signal that the user expects to be delivered.
4. **No additional attack surface.** Running a shell through the wrapper
   must not weaken the security properties the same shell would have when
   run directly.

### 1.3 Non-goals

The wrapper does not attempt to:

* Sandbox the shell or any program the shell starts. Anything the user
  could do at the bare shell prompt, they can do through `nix-pretty`.
* Filter or sanitise ANSI escape sequences. Pass-through is required for
  colour, line-editing and full-screen TUIs.
* Defend against an attacker who already has code execution as the user
  on the same machine.
* Defend against a malicious operating system kernel or terminal emulator.

## 2. Threat model

### 2.1 Trust boundaries

```
  +------------------+      stdin       +------------+
  | Terminal emulator| ---------------> |  parent    |
  | (trusted)        | <--------------- |  nix-pretty|
  +------------------+   stdout (rwr.)  +-----+------+
                                              | PTY master
                                              v
                                         +----+----+
                                         |  child  |
                                         |  shell  |  (untrusted output)
                                         +---------+
```

Trusted: the user's terminal emulator, the kernel, the file system that
hosts the binary, the user's own shell startup files. The parent process is
trusted with respect to the user; the child shell is trusted with respect
to the user's intent but its **output** is treated as untrusted bytes by
the wrapper.

### 2.2 Adversaries we consider

| # | Adversary | Capability |
| --- | --- | --- |
| A1 | A program the user runs that emits hostile bytes | Can write arbitrary bytes (including ANSI sequences) to the PTY master. |
| A2 | A compromised dependency in `Cargo.toml` | Can run arbitrary code at build or run time. |
| A3 | A local unprivileged user on the same machine | No special access to the user's PTY. |
| A4 | The author of a `shell.nix` the user opens | Controls the `shellHook` and may set environment variables before `nix-pretty` starts. |
| A5 | A process that wins a race on `$PATH` lookup | Can place a binary called `bash` earlier in PATH. |

### 2.3 Adversaries explicitly out of scope

* An attacker with root on the machine.
* An attacker with the same UID as the user (they can already do anything
  the user can).
* Physical attackers, side-channel attackers, supply-chain attacks on the
  compiler.

## 3. Known CVE classes that touch this design

The following published vulnerability classes shape the design and are
worth understanding before reading the per-vector analysis in §4.

### 3.1 ANSI escape-sequence injection (log poisoning)

Recent CVEs in this class:

* **CVE-2025-58160** (RUSTSEC-2025-0055, `tracing-subscriber`) — untrusted
  user input logged verbatim allowed terminal title-bar spoofing and
  screen-clearing.
* **CVE-2025-55193** (Rails Active Record logging) — ANSI escapes in
  logged record fields manipulated the operator's terminal.
* **CVE-2025-55754** (Apache Tomcat) — ANSI escapes in URL paths reached
  the console and the Windows clipboard.
* **CVE-2023-3997** (Splunk SOAR) — ANSI escapes in HTTP requests reached
  log-viewer terminals and triggered terminal-emulator vulnerabilities.

The general pattern is documented at OWASP as CWE-150 (Improper
Neutralization of Escape, Meta, or Control Sequences).

### 3.2 TIOCSTI / TIOCLINUX terminal-input injection

A process that shares a controlling terminal with its caller can call
`ioctl(TIOCSTI)` to push bytes back into the terminal's input queue, where
they will be executed by the caller's shell after the child exits. This
attack is the reason `sudo` allocates its own PTY by default and the
reason the kernel has been progressively restricting `TIOCSTI` access.
Background reading: errno.fr "The oldest privesc", the `Duncaen/OpenDoas`
issue tracker, and the upstream patch series gating `TIOCSTI` behind
`CAP_SYS_ADMIN` on Linux.

### 3.3 PATH / SHELL environment hijacking

`execvp(3)` searches `$PATH` if the program name contains no slash.
MITRE ATT&CK technique T1574.007 covers PATH interception. The classic
form (a setuid binary trusting `$PATH`) does not apply here, but the same
mechanism still lets any process that can set the user's environment
choose which `bash` runs.

### 3.4 Async-signal-safety between `fork()` and `exec()`

POSIX requires that only async-signal-safe functions be called between
`fork()` and `exec()` in a process that previously had more than one
thread. Violations have produced silent corruption and deadlocks in
NetBSD's `ld.elf_so` and elsewhere (see `rust-lang/rust#76600`,
`rust-lang/rust#64718`). This is why `std::os::unix::process::CommandExt::
before_exec` is deprecated in favour of an `unsafe pre_exec`.

### 3.5 Dependency-chain CVEs

* **CVE-2026-33056** (`cargo` + bundled `tar`) — a malicious crate could
  change permissions on arbitrary directories during `cargo` extraction.
  Patched in Rust 1.94.1; affects build time only.
* **RUSTSEC-2021-0119** (`nix` crate, `getgrouplist` OOB write) — long
  since patched; the affected API is not used by this project, and the
  pinned `nix = "0.29"` is above all listed patched versions.
* No published CVE or RustSec advisory currently applies to `nix 0.29`,
  `libc 0.2.x` or `signal-hook 0.3.x` as used here.

## 4. Vulnerabilities and attack vectors

Each entry below states the vector, the likelihood and impact under the
threat model in §2, the current mitigation, and any residual risk.

| § | Attack vector | Likelihood | Impact | Mitigation implemented |
| --- | --- | --- | --- | --- |
| 4.1 | `NIX_PRETTY_SHELL` arbitrary-binary execution | Medium | Low | Yes (documented; no setuid) |
| 4.2 | PATH-based `execvp` lookup of default `bash` | Low | Low | No |
| 4.3 | ANSI / OSC escape-sequence pass-through | High | Medium | N/A (by design) |
| 4.4 | TIOCSTI input injection from the child† | Low | High | Yes (`setsid` + `TIOCSCTTY`) |
| 4.5 | Async-signal-safety between `fork()` and `execvp()` | Low | Medium | Yes (fork-before-threads) |
| 4.6 | `panic = "abort"` defeats `RawModeGuard` restore | Low | Low | Yes (panic hook) |
| 4.7 | Unbounded rewriter buffer (DoS) | Low | Low | Yes (`MAX_CANDIDATE_LEN`) |
| 4.8 | Memory unsafety in `unsafe` blocks | Low | High | Yes (audited; `nix` wrappers) |
| 4.9 | Information disclosure: irreversible `nix:` form | High | Low | N/A (by design) |
| 4.10 | SIGWINCH thread liveness | Low | Low | Yes (`signal-hook`) |
| 4.11 | Stdin forwarder thread on shutdown | Low | Low | Yes (documented) |
| 4.12 | Build-time supply-chain (CVE-2026-33056 et al.) | Low | High | No |
| 4.13 | Re-exec loop in `shellHook` | Low | Low | Yes (env-var guard in example) |
| 4.14 | Argv pass-through to the wrapped shell | Low | Low | Yes (`CString::new` rejects NUL) |

† **Privilege-escalation note (§4.4).** This is the only vector in the
table that, if its mitigation were absent, would cross a trust boundary:
a successful `TIOCSTI` injection would execute attacker-chosen commands
in the user's **outer** shell after `nix-pretty` exits, effectively
escalating from "an output the user is viewing" to "a command the user
ran". The mitigation (separate session via `setsid()` and a fresh
controlling terminal via `TIOCSCTTY`) is implemented and verified by
inspection of `child_after_fork` in `src/pty.rs`. None of the other
vectors in the table cross a privilege boundary: `nix-pretty` runs
entirely at the invoking user's privilege level and is not intended
to be installed setuid.

### 4.1 `NIX_PRETTY_SHELL` arbitrary-binary execution

* **Vector.** `main.rs` reads `NIX_PRETTY_SHELL` and passes the value
  straight to `execvp`. An adversary who controls the user's environment
  (a malicious `shell.nix`, a profile script, a CI runner config) can
  redirect `nix-pretty` to run any program of their choice.
* **Adversary.** A4 (malicious `shell.nix`), A2 (compromised dep that
  sets the variable in a `build.rs`).
* **Likelihood.** Medium for users who run untrusted `shell.nix` files.
* **Impact.** Low. The substituted binary runs at the user's existing
  privilege level — exactly what would happen if the same `shell.nix`
  simply ran the malicious binary directly. There is no privilege boundary
  to cross.
* **Mitigation.** Documented behaviour; the variable is part of the
  intended UX. The binary deliberately does no setuid/setgid work, so
  there is no privilege escalation surface. The README example pins the
  shell with an absolute path (`/run/current-system/sw/bin/bash`).
* **Residual risk.** Users who copy-paste `shell.nix` snippets from the
  internet should review the `shellHook`. This is the same advice as
  using `nix-shell` directly.

### 4.2 PATH-based `execvp` lookup

* **Vector.** When `NIX_PRETTY_SHELL` is unset, the default `"bash"` is
  resolved through `$PATH` by `execvp`. Any directory that appears before
  the real `bash`'s directory in `$PATH` and contains a writable file
  called `bash` will win.
* **Adversary.** A5.
* **Likelihood.** Low in practice — users with hostile `$PATH` entries
  have larger problems.
* **Impact.** Low (no privilege boundary, see §4.1).
* **Mitigation.** None at the code level. Documentation could be improved
  by recommending an absolute path in the `shellHook` example.
* **Recommendation.** Update the default-shell documentation to suggest
  an absolute path for `NIX_PRETTY_SHELL` in security-sensitive setups.

### 4.3 ANSI / OSC escape-sequence pass-through

* **Vector.** The rewriter is explicitly byte-transparent for everything
  except the `/nix/store/<hash>-<pkg>` grammar. ANSI CSI sequences, OSC
  sequences (terminal title, hyperlink, clipboard on supporting
  emulators), DCS and APC sequences all pass through unchanged. A
  malicious program in the child shell can therefore inject any escape
  the user's terminal emulator interprets — the same class of
  vulnerabilities behind CVE-2025-58160, CVE-2025-55193 and
  CVE-2025-55754 (§3.1).
* **Adversary.** A1 (any program the user runs).
* **Likelihood.** High whenever the user views attacker-controlled
  output (CI logs, build output of untrusted derivations, `cat` of a
  hostile file).
* **Impact.** Bounded by the terminal emulator. Modern emulators are
  hardened against the worst cases (no longer executing OSC 51/52
  unconditionally, requiring user opt-in for clipboard writes), but
  terminal-title spoofing and screen-clearing are usually still
  available.
* **Mitigation.** Pass-through is required by goal §1.2.1 (integrity)
  and §1.3 (we explicitly do not filter). The wrapper does not increase
  this risk: a user running the same program without the wrapper sees
  the same escapes.
* **Recommendation.** Implemented in v1.1.1: README.md has a "Security
  considerations" section that states explicitly that the wrapper is not a
  sanitiser. Downstream consumers reading the README cannot mistake
  `nix-pretty` for a defence against CWE-150.

### 4.4 TIOCSTI input injection from the child

* **Vector.** A child process that shares the **parent's** controlling
  terminal could call `ioctl(TIOCSTI, ...)` and push bytes into the
  user's shell input buffer, to be executed after `nix-pretty` exits.
* **Adversary.** A1.
* **Likelihood.** Would be high without the PTY design; effectively
  blocked by it.
* **Impact.** Would be arbitrary code execution in the user's outer
  shell.
* **Mitigation (strong).** The child is moved into a **new session**
  via `setsid()` and acquires the **slave PTY** as its controlling
  terminal via `ioctl(TIOCSCTTY)` before `execvp`. The child therefore
  has no handle to the parent's tty, and any `TIOCSTI` it issues lands
  in its own slave queue, not the user's terminal. This is the same
  technique `sudo`'s pty mode and `script(1)` use to defeat the attack.
* **Residual risk.** None as long as the child cannot open the parent's
  controlling tty by name (`/dev/tty`). Because the child has its own
  session and its own ctty (the slave), `/dev/tty` in the child resolves
  to the slave, not the user's terminal. Verified by inspection of
  `child_after_fork` in `src/pty.rs`.

### 4.5 Async-signal-safety between `fork()` and `execvp()`

* **Vector.** In a multi-threaded program, only async-signal-safe
  functions may be called in the child between `fork()` and `exec()`.
  Violations can deadlock the allocator or corrupt internal libc state.
  `child_after_fork` calls `setsid`, `ioctl`, `dup2`, `_exit` and
  `libc::write` (all async-signal-safe), but also allocates with
  `CString::new`, `Vec::with_capacity` and `Iterator::collect` (not
  async-signal-safe).
* **Adversary.** Not adversarial — this is a correctness concern that
  could become exploitable if the allocator is in a non-reentrant state
  at fork time.
* **Likelihood.** Currently zero by construction: `fork()` is called
  before any helper thread is spawned (`spawn_winch_thread` and
  `spawn_stdin_forwarder` both run in the **parent**, after `fork`).
  The child sees a single-threaded process at fork time, and the POSIX
  restriction does not apply.
* **Impact.** None today.
* **Mitigation.** Maintained by the call-ordering invariant
  ("fork before threads") in `pty::run`. The invariant is asserted in
  the source comment `// SAFETY: we have not spawned any threads yet`.
* **Residual risk.** A future refactor that moves thread spawning before
  the fork (for example, to start a logging thread early) would
  silently break the invariant. A regression test that asserts no
  `std::thread::spawn` reaches the pre-fork code path, or a clippy lint,
  would harden this.

### 4.6 `panic = "abort"` defeats the `RawModeGuard` restore on panic

* **Vector.** `RawModeGuard` is a RAII guard that restores termios on
  drop. `ARCHITECTURE.md` claims the guard restores the terminal "even
  on panic". However, `[profile.release]` in `Cargo.toml` sets
  `panic = "abort"`, which means panics terminate the process **without
  unwinding**, so `Drop` does **not** run. In a release build, a panic
  in the parent leaves the user's terminal in raw mode and they must
  type `reset` blind to recover.
* **Adversary.** A1 indirectly: a sufficiently strange byte stream that
  triggers a panic in the parent (none currently known) would expose the
  user to this.
* **Likelihood.** Low — the parent's hot path has no panics on any
  exercised input, and there are no `unwrap()`s in the pump.
* **Impact.** Usability degradation, not a security boundary breach.
  The terminal will misbehave until `reset` is typed.
* **Mitigation (strong).** Implemented in v1.1.2: `pty::run` installs a
  process-wide panic hook (`install_panic_termios_restore`) that issues
  `tcsetattr(TCSANOW, &original)` on the outer tty before chaining to
  the previous (default) hook. The original termios is captured by
  `RawModeGuard::install` in a module-level
  `OnceLock<Mutex<Option<(RawFd, Termios)>>>` and cleared by the same
  guard's `Drop`, so a panic that happens after normal shutdown does
  not try to restore on a closed fd. The hook restores **before** the
  default hook prints the panic message, so the message lands on a
  cooked-mode terminal that the user can read without typing `reset`
  first. The behaviour is unit-tested by
  `raw_mode_guard_populates_and_clears_termios_restore` (state
  lifecycle) and `restore_termios_from_state_actually_restores_termios`
  (the restore path itself).
* **Residual risk.** A panic that happens **before**
  `install_panic_termios_restore` returns (i.e. while the panic hook
  itself is being set) would not be caught by the hook. The window is
  a single `Once::call_once` block at the very top of `run`, before
  any tty is touched, so the terminal cannot yet be in raw mode.

### 4.7 Unbounded rewriter buffer (DoS)

* **Vector.** A malicious program could write `/nix/store/` followed by
  arbitrary lowercase characters forever, attempting to grow the
  candidate buffer.
* **Adversary.** A1.
* **Likelihood.** Easy to construct.
* **Impact.** Would be unbounded memory if the buffer were uncapped.
* **Mitigation (strong).** `MAX_CANDIDATE_LEN = 1024` bytes caps the
  in-flight buffer. Hitting the cap calls `bail`, which emits the
  buffer verbatim and resets to `Idle`. This is unit-tested by
  `candidate_buffer_is_bounded`, which feeds `2 * MAX_CANDIDATE_LEN`
  hash characters and asserts the buffer never exceeds the cap.
* **Residual risk.** None.

### 4.8 Memory unsafety in the small pool of `unsafe` blocks

* **Vector.** `pty.rs` contains a handful of `unsafe` blocks wrapping
  raw libc calls: `ioctl`, `setsid`, `dup2`, `_exit`, `read`, `write`,
  `isatty`, plus `BorrowedFd::borrow_raw` and `fork()`. Each is a
  potential memory-safety hazard if the invariants are violated.
* **Adversary.** Not adversarial — correctness concern.
* **Likelihood.** Low. Each block is short, documented with a SAFETY
  comment, and uses the standard POSIX-from-Rust idiom.
* **Impact.** Could be memory corruption if an invariant were broken.
* **Mitigation.** Code-review discipline. The `nix` crate wraps the
  same syscalls in safe types where it can; the unsafe blocks here are
  only present where `nix 0.29` does not expose a safe wrapper. The
  rewriter (`src/rewriter.rs`) is structurally `unsafe`-free, enforced
  at compile time by an inner `#![deny(unsafe_code)]` (added in
  v1.1.3); any future PR that adds `unsafe` to the rewriter fails the
  build.
* **Recommendation.** Annual `cargo audit` plus a quick `unsafe` grep
  during release reviews. The `rewriter.rs` invariant is now
  compiler-enforced, not just convention.

### 4.9 Information disclosure: the collapsed `nix:` form is irreversible

* **Vector.** The rewriter discards the hash and package name. A user
  who copies `nix:/bin/ls` from their terminal cannot recover the
  original store path from the wrapper.
* **Adversary.** Not adversarial — usability concern, listed here
  because it appears in `Limitations`.
* **Likelihood.** Certain when used.
* **Impact.** Usability only.
* **Mitigation.** Documented. The READMEs `Limitations` section points
  users at `nix path-info` and `realpath` for the original path.

### 4.10 SIGWINCH thread liveness

* **Vector.** The SIGWINCH listener thread runs the lifetime of the
  process. It is not joined at shutdown; instead the kernel reaps it on
  `_exit`. If the listener could deadlock holding a signal mask, it
  could block delivery of other signals.
* **Adversary.** Not adversarial.
* **Likelihood.** Negligible. `signal-hook`'s `Signals::forever`
  iterator is the canonical async-signal-safe pattern and does not hold
  any signal mask outside its self-pipe `read`.
* **Mitigation.** Use of `signal-hook` rather than a hand-rolled
  signal handler.

### 4.11 Stdin forwarder thread keeps reading on shutdown

* **Vector.** The stdin-forwarder thread is left blocked on
  `read(STDIN_FILENO)` when the main thread exits. On a clean
  `_exit`, the kernel tears it down. On a partial shutdown (signal,
  panic + abort), the thread dies with the process.
* **Adversary.** Not adversarial.
* **Likelihood.** Always present.
* **Impact.** None observed.
* **Mitigation.** Documented in code; relies on `std::process::exit`
  semantics, which is the documented behaviour.

### 4.12 Build-time supply-chain (CVE-2026-33056 and similar)

* **Vector.** A malicious crate published to `crates.io` could escape
  Cargo's extraction sandbox during `cargo build`, as in CVE-2026-33056.
* **Adversary.** A2.
* **Likelihood.** Low for direct dependencies (`nix`, `libc`,
  `signal-hook` are all widely-audited). Higher for transitive deps
  in general, though the dep graph here is small.
* **Impact.** Code execution at build time as the building user.
* **Mitigation.** Build on a Rust toolchain ≥ 1.94.1 (which contains
  the CVE-2026-33056 patch). The project already pins `rust-version =
  "1.74"` as a minimum; bumping the **floor** to a version that
  contains the cargo fix would harden builds for downstream users.
* **Recommendation.** Add `cargo audit` to CI; pin transitive deps via
  a committed `Cargo.lock` (this is a binary crate so `Cargo.lock`
  should already be tracked).

### 4.13 Re-exec loop in `shellHook`

* **Vector.** The recommended `shellHook` uses `NIX_PRETTY_ACTIVE` as
  a guard against infinite re-execution. A typo or copy-paste error in
  the user's `shell.nix` could create a fork bomb.
* **Adversary.** Not adversarial (user error).
* **Likelihood.** Low.
* **Impact.** Local DoS until the user terminates the runaway shell
  tree.
* **Mitigation.** The README example sets `export NIX_PRETTY_ACTIVE=1`
  before `exec nix-pretty`, and uses `[ -t 1 ]` to skip non-interactive
  invocations.
* **Recommendation.** Consider having the binary itself refuse to
  re-exec when it detects it is already running (e.g., a self-check on
  process ancestry) as a belt-and-braces measure. Not currently
  implemented; the existing guard is sufficient for documented usage.

### 4.14 Argv pass-through to the wrapped shell

* **Vector.** `main.rs` collects `env::args().skip(1)` and passes the
  argv straight to `execvp` via `CString::new`. A NUL byte in any
  argument causes `CString::new` to fail and the child to die with exit
  127 — that is the only validation performed.
* **Adversary.** A4.
* **Likelihood.** Same as `shell.nix` trust (§4.1).
* **Impact.** Low. The argv is passed to a shell the user chose; the
  shell parses it under its own rules.
* **Mitigation.** No shell-out, no command construction in the wrapper.
  All argv handling goes through `CString::new`, which rejects embedded
  NULs at the type-system boundary.

## 5. Defence-in-depth properties already in place

The design buys several defensive properties for free:

* **Zero network code.** No socket APIs are linked; the dependency graph
  has no networking crates.
* **Zero filesystem writes.** The binary opens `/dev/ptmx` (via `openpty`)
  and reads `argv` and the environment. It does not write any file. There
  is no on-disk state to poison.
* **No setuid/setgid usage assumed.** The binary is meant to be installed
  as a regular user executable. None of the documented use cases require
  elevated privileges, which removes the entire class of privilege-
  escalation attacks against environment-variable handling.
* **Bounded memory.** Every buffer in the program is fixed-size or capped:
  `4096` for the PTY-read and stdin buffers, `1024` for the in-flight
  rewriter candidate, `8192` for the output staging vector.
* **Pure, hand-rolled rewriter.** The rewriter has zero `unsafe`, no
  regex engine, and no allocation per byte beyond the bounded buffer.
  Correctness is exercised by an oracle-based fuzz harness in
  `rewriter::tests::random_payload_round_trips_through_oracle` and by
  every-chunk-boundary tests.
* **RAII + panic hook for terminal state.** `RawModeGuard` is the only
  owner of the raw-mode property and restores termios on every drop
  path the runtime can deliver under `panic = "unwind"`. The release
  profile uses `panic = "abort"`, so a process-wide panic hook
  (`install_panic_termios_restore`) reads the same `(fd, termios)` pair
  and restores it before the default hook prints the panic message and
  the process aborts (see §4.6).
* **PTY isolation of the child.** `setsid` + `TIOCSCTTY` give the child
  a fresh session and a fresh controlling terminal (the slave). This
  defeats the TIOCSTI class of attack (§4.4) by construction.
* **Small, audited dependency set.** Three direct dependencies, all
  widely used, all currently free of known CVEs in the pinned versions.

## 6. Recommended hardening

Ordered by cost / benefit.

1. **Document the ANSI pass-through policy in `README.md`.** Add a short
   "Security considerations" section that states the wrapper does not
   sanitise terminal escapes. Cost: minutes. Benefit: prevents
   downstream consumers from treating the wrapper as a sanitiser.
   *Status: implemented in v1.1.1 (see README.md "Security
   considerations").*
2. **Install a panic hook that restores termios.** Eliminates the
   `panic = "abort"` discrepancy in §4.6. Cost: ~20 lines of code.
   Benefit: usability and matches the architecture document's claim.
   *Status: implemented in v1.1.2 (`pty.rs` `install_panic_termios_restore`,
   tested by `raw_mode_guard_populates_and_clears_termios_restore` and
   `restore_termios_from_state_actually_restores_termios`).*
3. **Add `cargo audit` to CI.** Cost: one CI job. Benefit: catches
   future RUSTSEC advisories in dependencies before release.
4. **Add a structural invariant against `unsafe` in `rewriter.rs`.**
   For example, an inner module attribute `#![deny(unsafe_code)]`.
   Cost: one line. Benefit: makes the rewriter's safety property
   compiler-enforced.
   *Status: implemented in v1.1.3 (`#![deny(unsafe_code)]` at the top
   of `src/rewriter.rs`).*
5. **Bump the MSRV floor to a cargo version containing the
   CVE-2026-33056 fix.** Cost: one Cargo.toml line. Benefit: protects
   downstream rebuilds.
6. **Recommend an absolute path for `NIX_PRETTY_SHELL` in the
   `shellHook` example.** Reduces PATH-hijack surface (§4.2). Cost:
   one line in the README.
7. **(Optional) Integration test that spawns the binary under a real
   PTY** to assert termios restoration on every documented exit path.
   This is already listed under "Future work" in the architecture
   document.

## 7. Audit checklist for new contributors

When changing the code, a contributor should be able to answer "yes" to
the following before merging:

* Does any new code path call `fork`? If yes, does it spawn no threads
  before the fork?
* Does any new `unsafe` block document its safety invariants in a
  `SAFETY:` comment?
* Does any new code allocate on the stdin → master or master → stdout
  hot path?
* Does any new feature read environment variables? If yes, are they
  documented and free of any implicit trust assumption?
* Does any new dependency appear in `Cargo.toml`? If yes, does it have
  a non-trivial audit history and no open RUSTSEC advisories at the
  pinned version?
* If the change touches the rewriter, does it remain `unsafe`-free and
  bounded by `MAX_CANDIDATE_LEN`?
* Are the unit-test coverage classes in `rewriter.rs` (headline example,
  near-miss matrix, every chunk-boundary, ANSI/UTF-8 mix, random oracle)
  preserved?

## 8. Disclosure

Security-relevant issues should be reported privately to the maintainers
via the contact listed in `Cargo.toml`'s `repository` field. Issues with
a public proof of concept that affects downstream users should be
embargoed for a reasonable window (suggested: 30 days) before publishing
the PoC.

## References

* OWASP CWE-150, Improper Neutralization of Escape, Meta, or Control
  Sequences.
* MITRE ATT&CK T1574.007, Path Interception by PATH Environment Variable.
* RustSec Advisory Database, `https://rustsec.org/`.
* RUSTSEC-2025-0055 / CVE-2025-58160, `tracing-subscriber` ANSI escape
  injection.
* CVE-2026-33056, Cargo extraction permission-change vulnerability.
* RUSTSEC-2021-0119, `nix::unistd::getgrouplist` out-of-bounds write.
* CVE-2025-55193, Rails Active Record logging ANSI injection.
* CVE-2025-55754, Apache Tomcat ANSI escape injection.
* CVE-2023-3997, Splunk SOAR log poisoning.
* errno.fr, "The oldest privesc: injecting careless administrators'
  terminals using TTY pushback".
* `rust-lang/rust#64718`, `rust-lang/rust#76600` — async-signal-safety
  concerns around `fork()` in multi-threaded Rust programs.

## Revision history

| Version | Date | Change |
| --- | --- | --- |
| 1.0.0 | 2026-05-21 | Initial Security Architecture Document. |
| 1.1.0 | 2026-05-21 | Added §4 attack-vector summary table with privilege-escalation note. Added versioning metadata at the top of the document. |
| 1.1.1 | 2026-05-21 | Marked §6.1 as implemented: ANSI pass-through policy documented in README.md "Security considerations". Updated §4.3 recommendation accordingly. |
| 1.1.2 | 2026-05-21 | Marked §6.2 as implemented: panic hook installed in `pty::run` (`install_panic_termios_restore`) that restores termios on the `panic = "abort"` path. Updated §4.6 mitigation, §4 table row, and §5 RAII bullet. |
| 1.1.3 | 2026-05-21 | Marked §6.4 as implemented: `#![deny(unsafe_code)]` added at the top of `src/rewriter.rs` so the rewriter's `unsafe`-free invariant is compiler-enforced. Updated §4.8 mitigation accordingly. |