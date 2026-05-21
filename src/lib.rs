//! Library entry point for `nix-pretty`.
//!
//! The crate is intentionally split in two pieces:
//!
//! * [`rewriter`] is pure, deterministic, `no_std`-friendly logic that
//!   transforms a byte stream by replacing `/nix/store` with `[nix-store]`.
//!   All correctness lives here and is covered by unit tests.
//! * [`pty`] is the small platform shim that wires a PTY, the rewriter and
//!   stdin/stdout together. It is gated to Unix builds since macOS / Linux
//!   are the only targets where `nix-shell` runs.

pub mod rewriter;

#[cfg(unix)]
pub mod pty;

pub use rewriter::PathRewriter;
