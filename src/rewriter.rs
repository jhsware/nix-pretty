//! Streaming byte-level rewriter that collapses every Nix store path it sees
//! in its input to the literal string `nix:`, leaving the trailing path
//! component (if any) intact.
//!
//! # Grammar
//!
//! A Nix store path looks like
//!
//! ```text
//! /nix/store/<hash>-<pkg>[<rest>]
//! ```
//!
//! where
//!
//! * `<hash>` is at least 32 characters drawn from `[0-9a-z]` (the alphabet
//!   used by Nix store hashes; we accept the slightly broader full lowercase
//!   base-36 alphabet for resilience against future format tweaks);
//! * `<pkg>` is one or more characters from `[A-Za-z0-9._+-]` (the set Nix
//!   uses for derivation names and versions, including dashes and dots);
//! * `<rest>` is anything that follows - typically a `/`-rooted path like
//!   `/bin/ls`. We do not consume it.
//!
//! The rewriter emits `nix:` for the `/nix/store/<hash>-<pkg>` segment and
//! passes everything else through unchanged, so
//!
//! ```text
//! /nix/store/3p5l9d7v3w7nq2x9jk8m5a7s8b1234567-coreutils-9.5/bin/ls
//! ```
//!
//! becomes
//!
//! ```text
//! nix:/bin/ls
//! ```
//!
//! # Why a hand-rolled state machine
//!
//! The grammar above is regular and could be expressed with a regex, but the
//! data flow is a long-lived byte stream that may split the candidate match
//! across arbitrary chunks (PTY reads commonly land in the middle of a
//! match). A small explicit state machine buffers exactly the bytes of a
//! candidate match, commits when the match completes, and bails verbatim if
//! the match fails. There is no regex engine and no allocation per byte;
//! the buffered candidate is bounded by [`MAX_CANDIDATE_LEN`].
//!
//! # Correctness invariant
//!
//! For any input `xs` split into chunks `c1, c2, ..., cn`, calling
//! `process(ci, &mut out)` in order and then `flush(&mut out)` produces the
//! exact same `out` as a single `process(xs, &mut out)` followed by `flush`.
//! This is exercised by the unit tests for every possible split point of a
//! representative payload.

use std::io::{self, Read, Write};

/// The literal prefix that opens a candidate match.
const PREFIX: &[u8] = b"/nix/store/";

/// What we emit in place of the matched `/nix/store/<hash>-<pkg>` segment.
const REPLACEMENT: &[u8] = b"nix:";

/// Minimum number of hash characters before a `-` may close the hash. Nix
/// uses exactly 32; we accept 32 or more to tolerate any future format
/// drift and to keep the implementation simple.
const MIN_HASH_LEN: usize = 32;

/// Hard cap on the size of the in-flight candidate buffer. A real store
/// path is around 60-120 bytes; any candidate longer than this is, by
/// definition, not a real store path and we bail to keep the rewriter's
/// memory use bounded under adversarial input.
const MAX_CANDIDATE_LEN: usize = 1024;

/// Streaming rewriter. Construct once per stream, feed chunks through
/// [`PathRewriter::process`], and call [`PathRewriter::flush`] at end of
/// input.
#[derive(Debug, Clone)]
pub struct PathRewriter {
    state: State,
    /// Bytes of the in-flight candidate match. Cleared on commit (after
    /// emitting `REPLACEMENT`) or on bail (after emitting them verbatim).
    /// Bounded by [`MAX_CANDIDATE_LEN`] by construction.
    buffer: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum State {
    /// Not currently inside a candidate match.
    Idle,
    /// Matched 1..PREFIX.len() bytes of `/nix/store/`. The next byte must
    /// be `PREFIX[matched]` for the prefix to keep matching.
    Prefix { matched: usize },
    /// Matched the full prefix, now greedily consuming hash characters.
    /// `count` is the number of hash characters consumed so far.
    Hash { count: usize },
    /// Just consumed the `-` separator after a long-enough hash; the next
    /// byte must be a valid package character.
    WantPkg,
    /// Consuming package characters. `count` is `>= 1`.
    Pkg { count: usize },
}

impl Default for PathRewriter {
    fn default() -> Self {
        Self::new()
    }
}

impl PathRewriter {
    /// Build a fresh rewriter in the idle state.
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            buffer: Vec::with_capacity(96),
        }
    }

    /// The literal prefix this rewriter looks for.
    #[inline]
    pub fn pattern() -> &'static [u8] {
        PREFIX
    }

    /// The replacement this rewriter emits for a successfully matched store
    /// path segment.
    #[inline]
    pub fn replacement() -> &'static [u8] {
        REPLACEMENT
    }

    /// Feed a chunk of input through the rewriter. Bytes that are part of a
    /// completed match are emitted as `REPLACEMENT`; bytes that are part of
    /// a failed (bailed) match are emitted verbatim; bytes that are not part
    /// of any candidate match are emitted verbatim immediately.
    pub fn process(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            self.step(b, out);
        }
    }

    /// Emit any remaining buffered candidate at end of input. A candidate
    /// that has consumed at least one package character is treated as a
    /// completed match; everything else is emitted verbatim.
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        match self.state {
            State::Pkg { count } if count > 0 => {
                self.commit(out);
            }
            _ => {
                if !self.buffer.is_empty() {
                    out.extend_from_slice(&self.buffer);
                    self.buffer.clear();
                }
                self.state = State::Idle;
            }
        }
    }

    /// Convenience: rewrite a single byte slice in one shot.
    pub fn rewrite_all(input: &[u8]) -> Vec<u8> {
        let mut r = Self::new();
        let mut out = Vec::with_capacity(input.len());
        r.process(input, &mut out);
        r.flush(&mut out);
        out
    }

    // --- internals --------------------------------------------------------

    fn step(&mut self, b: u8, out: &mut Vec<u8>) {
        // Hard cap on the in-flight candidate. If we are growing past it
        // without committing, this can't be a real store path; bail.
        if self.buffer.len() >= MAX_CANDIDATE_LEN {
            self.bail(b, out);
            return;
        }

        match self.state {
            State::Idle => {
                if b == PREFIX[0] {
                    self.state = State::Prefix { matched: 1 };
                    self.buffer.push(b);
                } else {
                    out.push(b);
                }
            }
            State::Prefix { matched } => {
                if matched < PREFIX.len() && b == PREFIX[matched] {
                    self.buffer.push(b);
                    if matched + 1 == PREFIX.len() {
                        self.state = State::Hash { count: 0 };
                    } else {
                        self.state = State::Prefix {
                            matched: matched + 1,
                        };
                    }
                } else {
                    self.bail(b, out);
                }
            }
            State::Hash { count } => {
                if is_hash_char(b) {
                    self.buffer.push(b);
                    self.state = State::Hash { count: count + 1 };
                } else if b == b'-' && count >= MIN_HASH_LEN {
                    self.buffer.push(b);
                    self.state = State::WantPkg;
                } else {
                    self.bail(b, out);
                }
            }
            State::WantPkg => {
                if is_pkg_char(b) {
                    self.buffer.push(b);
                    self.state = State::Pkg { count: 1 };
                } else {
                    // We had prefix + hash + '-' but no package character.
                    // That's not a real store path; bail.
                    self.bail(b, out);
                }
            }
            State::Pkg { count } => {
                if is_pkg_char(b) {
                    self.buffer.push(b);
                    self.state = State::Pkg { count: count + 1 };
                } else {
                    // Package terminator. The candidate is a valid store
                    // path; commit, then re-process the terminator byte from
                    // a clean Idle state (so e.g. a `/` after the pkg can
                    // start a new match... it won't here, but in general
                    // bytes after pkg may resume scanning).
                    self.commit(out);
                    self.step(b, out);
                }
            }
        }
    }

    /// Match failed: emit the in-flight candidate verbatim, reset, and
    /// re-process the byte that caused the bail in case it itself starts a
    /// new candidate (e.g. a stray `/` after a half-matched `/nix/store/`).
    fn bail(&mut self, b: u8, out: &mut Vec<u8>) {
        if !self.buffer.is_empty() {
            out.extend_from_slice(&self.buffer);
            self.buffer.clear();
        }
        self.state = State::Idle;
        // Re-process b in Idle. Bounded recursion: Idle either emits b
        // directly or transitions to Prefix; in neither path does it call
        // bail again, so there is no possibility of unbounded recursion.
        self.step(b, out);
    }

    /// Match succeeded: emit `REPLACEMENT`, reset.
    fn commit(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(REPLACEMENT);
        self.buffer.clear();
        self.state = State::Idle;
    }
}

/// Hash characters: digits and lowercase letters. Nix uses a 32-char subset
/// of base-32 (no `e`, `o`, `t`, `u`) but we accept the full lowercase
/// alphabet for robustness; over-matching at this layer would mean failing
/// the dash check later, which still bails cleanly.
#[inline]
fn is_hash_char(b: u8) -> bool {
    b.is_ascii_digit() || b.is_ascii_lowercase()
}

/// Package-name characters. Matches the practical alphabet used by every
/// derivation in nixpkgs: letters, digits, dot, dash, underscore, plus.
#[inline]
fn is_pkg_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+')
}

/// Pump bytes from `reader` through a [`PathRewriter`] into `writer` until
/// EOF, flushing on every write so latency stays low for interactive use.
/// Returns the number of input bytes processed.
///
/// This is the same plumbing the PTY loop uses, factored out so it can be
/// driven by a `Cursor` in tests without spawning a real PTY.
pub fn pipe_through<R: Read, W: Write>(mut reader: R, mut writer: W) -> io::Result<u64> {
    let mut rewriter = PathRewriter::new();
    let mut input = [0u8; 8192];
    let mut output: Vec<u8> = Vec::with_capacity(8192);
    let mut total: u64 = 0;
    loop {
        match reader.read(&mut input) {
            Ok(0) => break,
            Ok(n) => {
                output.clear();
                rewriter.process(&input[..n], &mut output);
                writer.write_all(&output)?;
                writer.flush()?;
                total += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    output.clear();
    rewriter.flush(&mut output);
    if !output.is_empty() {
        writer.write_all(&output)?;
        writer.flush()?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ----- helpers -------------------------------------------------------

    /// A representative 32-character hash made of lowercase letters and
    /// digits, used in most of the positive examples below.
    const HASH32: &[u8] = b"3p5l9d7v3w7nq2x9jk8m5a7s8b1234567";

    /// A representative store path. Note this is exactly what the user's
    /// example uses; the rewriter must collapse it to `nix:/bin/ls`.
    fn example_path() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"/nix/store/");
        v.extend_from_slice(HASH32);
        v.extend_from_slice(b"-coreutils-9.5/bin/ls");
        v
    }

    /// Reference implementation used as the oracle: a non-streaming
    /// equivalent of the rewriter. For each position in `input` it tries to
    /// match a full store path; on success it emits `nix:` and skips past
    /// the matched bytes; otherwise it emits one byte and advances.
    fn oracle(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut i = 0;
        while i < input.len() {
            if let Some(consumed) = try_match_at(&input[i..]) {
                out.extend_from_slice(PathRewriter::replacement());
                i += consumed;
            } else {
                out.push(input[i]);
                i += 1;
            }
        }
        out
    }

    /// Returns the number of bytes consumed if `s` starts with a full store
    /// path according to the documented grammar; otherwise `None`.
    fn try_match_at(s: &[u8]) -> Option<usize> {
        if !s.starts_with(PREFIX) {
            return None;
        }
        let mut i = PREFIX.len();
        // Hash: greedy, must be at least MIN_HASH_LEN.
        let hash_start = i;
        while i < s.len() && is_hash_char(s[i]) {
            i += 1;
        }
        if i - hash_start < MIN_HASH_LEN {
            return None;
        }
        // Dash.
        if i >= s.len() || s[i] != b'-' {
            return None;
        }
        i += 1;
        // Package: at least one pkg char.
        let pkg_start = i;
        while i < s.len() && is_pkg_char(s[i]) {
            i += 1;
        }
        if i == pkg_start {
            return None;
        }
        Some(i)
    }

    /// Feed `input` through the rewriter using the given split points and
    /// return the concatenated output.
    fn feed_with_splits(input: &[u8], splits: &[usize]) -> Vec<u8> {
        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        let mut prev = 0;
        for &s in splits {
            assert!(s >= prev && s <= input.len(), "bad split point");
            r.process(&input[prev..s], &mut out);
            prev = s;
        }
        r.process(&input[prev..], &mut out);
        r.flush(&mut out);
        out
    }

    // ----- the headline example -----------------------------------------

    #[test]
    fn user_example_collapses_to_nix_colon_bin_ls() {
        // This is the exact example from the requirements doc; if anything
        // ever drifts in the rewriter, this test is the canary.
        let input = example_path();
        assert_eq!(PathRewriter::rewrite_all(&input), b"nix:/bin/ls");
    }

    // ----- basic behaviour ----------------------------------------------

    #[test]
    fn pattern_and_replacement_are_what_we_advertise() {
        assert_eq!(PathRewriter::pattern(), b"/nix/store/");
        assert_eq!(PathRewriter::replacement(), b"nix:");
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let out = PathRewriter::rewrite_all(b"");
        assert!(out.is_empty());
    }

    #[test]
    fn input_without_pattern_passes_through_unchanged() {
        let cases: &[&[u8]] = &[
            b"hello world",
            b"\x1b[31mcolor\x1b[0m",
            b"/usr/local/bin/foo",
            b"/nix",
            b"/nix/stor",
            b"/nix/store",            // missing the trailing slash
            "raksmorgas".as_bytes(),
        ];
        for c in cases {
            assert_eq!(PathRewriter::rewrite_all(c), *c, "input: {:?}", c);
        }
    }

    #[test]
    fn single_match_followed_by_path() {
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-coreutils-9.5/bin/ls");
        assert_eq!(
            PathRewriter::rewrite_all(&input),
            b"nix:/bin/ls"
        );
    }

    #[test]
    fn single_match_with_no_trailing_path() {
        // Store path at end of input with no `/<rest>` after the pkg name.
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-coreutils-9.5");
        assert_eq!(PathRewriter::rewrite_all(&input), b"nix:");
    }

    #[test]
    fn match_at_start_middle_and_end_of_line() {
        let mut input = Vec::new();
        input.extend_from_slice(b"see /nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-coreutils-9.5/bin/ls for details");
        assert_eq!(
            PathRewriter::rewrite_all(&input),
            b"see nix:/bin/ls for details"
        );
    }

    #[test]
    fn multiple_matches_in_one_chunk() {
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-pkg-1/bin and /nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-pkg-2/bin");
        assert_eq!(
            PathRewriter::rewrite_all(&input),
            b"nix:/bin and nix:/bin"
        );
    }

    #[test]
    fn pkg_with_dashes_dots_pluses_underscores() {
        // Real-world-ish pkg names mix dots, pluses, dashes, underscores.
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-gtk+3-3.24.42_dev/lib");
        assert_eq!(PathRewriter::rewrite_all(&input), b"nix:/lib");
    }

    #[test]
    fn hash_longer_than_32_still_matches() {
        // The grammar accepts >=32 hash chars to tolerate format drift and
        // to be lenient with user-supplied examples.
        let long_hash = b"a3p5l9d7v3w7nq2x9jk8m5a7s8b1234567xyz"; // 37 chars
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(long_hash);
        input.extend_from_slice(b"-pkg/bin");
        assert_eq!(PathRewriter::rewrite_all(&input), b"nix:/bin");
    }

    #[test]
    fn near_misses_are_left_alone() {
        let mut short_hash = Vec::new();
        short_hash.extend_from_slice(b"/nix/store/abc-pkg/bin"); // 3-char hash
        let mut no_dash = Vec::new();
        no_dash.extend_from_slice(b"/nix/store/");
        no_dash.extend_from_slice(HASH32);
        no_dash.extend_from_slice(b"_pkg/bin"); // underscore instead of dash
        let mut no_pkg = Vec::new();
        no_pkg.extend_from_slice(b"/nix/store/");
        no_pkg.extend_from_slice(HASH32);
        no_pkg.extend_from_slice(b"-/foo"); // dash then immediate slash, no pkg char
        let cases: &[(&[u8], &[u8])] = &[
            (b"/nix/storage/foo",       b"/nix/storage/foo"),    // 'a' is not '/'
            (b"/Nix/Store/abc",         b"/Nix/Store/abc"),      // case-sensitive
            (&short_hash,               &short_hash),            // hash too short
            (&no_dash,                  &no_dash),               // wrong separator
            (&no_pkg,                   &no_pkg),                // empty pkg
            (b"nix/store/whatever",     b"nix/store/whatever"),  // missing leading /
        ];
        for (input, want) in cases {
            assert_eq!(
                PathRewriter::rewrite_all(input),
                want.to_vec(),
                "input: {:?}",
                input
            );
        }
    }

    // ----- chunk boundary tests -----------------------------------------

    #[test]
    fn every_split_point_produces_the_same_output() {
        // Mix of a real match, leading junk, trailing junk, and a near-miss
        // to exercise both commit and bail paths under chunking.
        let mut input = Vec::new();
        input.extend_from_slice(b"prefix /nix/storage/x ");
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-pkg-1.0/bin/ls suffix");
        let want = oracle(&input);

        for split in 0..=input.len() {
            let got = feed_with_splits(&input, &[split]);
            assert_eq!(got, want, "split at byte {}", split);
        }
    }

    #[test]
    fn one_byte_at_a_time_matches_one_shot() {
        let input = example_path();
        let want = PathRewriter::rewrite_all(&input);

        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        for &b in &input {
            r.process(&[b], &mut out);
        }
        r.flush(&mut out);
        assert_eq!(out, want);
    }

    #[test]
    fn flush_emits_buffered_candidate_when_stream_ends_mid_match() {
        // Stream ends in the middle of the hash. Buffered bytes must be
        // emitted verbatim, not silently dropped.
        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        r.process(b"hello /nix/store/abcdef", &mut out);
        assert_eq!(out, b"hello "); // partial candidate is held back
        r.flush(&mut out);
        assert_eq!(out, b"hello /nix/store/abcdef");
    }

    #[test]
    fn flush_commits_when_stream_ends_with_complete_pkg() {
        // Stream ends right after the pkg name, with no trailing path.
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-pkg-1.0");

        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // Nothing emitted yet: still in Pkg state waiting for terminator.
        assert!(out.is_empty());
        r.flush(&mut out);
        assert_eq!(out, b"nix:");
    }

    #[test]
    fn rewriter_can_be_reused_after_flush() {
        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        r.process(b"/nix/sto", &mut out);
        r.flush(&mut out);
        assert_eq!(out, b"/nix/sto");

        out.clear();
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-pkg/bin");
        r.process(&input, &mut out);
        r.flush(&mut out);
        assert_eq!(out, b"nix:/bin");
    }

    // ----- safety / bounds ----------------------------------------------

    #[test]
    fn candidate_buffer_is_bounded() {
        // Pathological input: an unbounded stream of hash chars after the
        // prefix. The buffer must not grow without limit; once we exceed
        // MAX_CANDIDATE_LEN the rewriter bails.
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend(std::iter::repeat(b'a').take(MAX_CANDIDATE_LEN * 2));

        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // Internal buffer must stay bounded by MAX_CANDIDATE_LEN.
        assert!(
            r.buffer.len() <= MAX_CANDIDATE_LEN,
            "buffer grew to {}",
            r.buffer.len()
        );
        r.flush(&mut out);
        // And the bail path must have emitted all input verbatim.
        assert_eq!(out, input);
    }

    // ----- mixed content -------------------------------------------------

    #[test]
    fn preserves_ansi_color_sequences_around_match() {
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b[32m");
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-pkg-1/bin");
        input.extend_from_slice(b"\x1b[0m\n");
        assert_eq!(
            PathRewriter::rewrite_all(&input),
            b"\x1b[32mnix:/bin\x1b[0m\n"
        );
    }

    #[test]
    fn ansi_escape_inside_pkg_terminates_match_safely() {
        // If an ANSI escape lands in the middle of what looks like a pkg
        // name, the partial pkg counts (it's >= 1 char) and we commit.
        // The escape sequence and the rest pass through verbatim.
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-pkg\x1b[0mtail");
        assert_eq!(
            PathRewriter::rewrite_all(&input),
            b"nix:\x1b[0mtail"
        );
    }

    #[test]
    fn preserves_utf8_multibyte_around_match() {
        let mut input = Vec::new();
        input.extend_from_slice("smörgås ".as_bytes());
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-pkg/bin");
        input.extend_from_slice(" krydda".as_bytes());

        let mut want = Vec::new();
        want.extend_from_slice("smörgås ".as_bytes());
        want.extend_from_slice(b"nix:/bin");
        want.extend_from_slice(" krydda".as_bytes());

        assert_eq!(PathRewriter::rewrite_all(&input), want);
    }

    #[test]
    fn binary_payload_passes_through_unchanged_when_no_match() {
        let mut payload = Vec::new();
        for b in 0u8..=255 {
            payload.push(b);
        }
        // Note: this payload happens not to contain `/nix/store/` as a
        // substring (255 unique bytes can't), so it is unchanged.
        assert_eq!(PathRewriter::rewrite_all(&payload), payload);
    }

    // ----- random / fuzz-style ------------------------------------------

    #[test]
    fn random_payload_round_trips_through_oracle() {
        let mut data: Vec<u8> = Vec::with_capacity(10_000);
        let mut state: u32 = 0x1234_5678;
        for i in 0..10_000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            // Bias towards printable ASCII so failures are readable, but
            // keep some non-ASCII bytes too.
            let byte = if state & 0xff < 200 {
                (32u8 + (state & 0x5f) as u8).min(126)
            } else {
                state as u8
            };
            data.push(byte);
            if i % 137 == 0 {
                // Embed a real store path.
                data.extend_from_slice(b"/nix/store/");
                data.extend_from_slice(HASH32);
                data.extend_from_slice(b"-coreutils-9.5/bin/ls ");
            }
            if i % 211 == 0 {
                data.extend_from_slice(b"/nix/stor"); // proper prefix only
            }
            if i % 311 == 0 {
                data.extend_from_slice(b"/nix/storage/x"); // near miss
            }
            if i % 419 == 0 {
                // Almost a store path but with a too-short hash.
                data.extend_from_slice(b"/nix/store/abc-pkg/bin");
            }
        }
        let want = oracle(&data);
        assert_eq!(PathRewriter::rewrite_all(&data), want);

        // Same payload, but fed in pseudo-random chunks.
        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        let mut i = 0;
        while i < data.len() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let take = ((state % 23) as usize + 1).min(data.len() - i);
            r.process(&data[i..i + take], &mut out);
            i += take;
        }
        r.flush(&mut out);
        assert_eq!(out, want);
    }

    // ----- pipe_through -------------------------------------------------

    #[test]
    fn pipe_through_basic() {
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-a/x and /nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-b/y");
        let mut out: Vec<u8> = Vec::new();
        let n = pipe_through(Cursor::new(&input), &mut out).unwrap();
        assert_eq!(n as usize, input.len());
        assert_eq!(out, b"nix:/x and nix:/y");
    }

    #[test]
    fn pipe_through_handles_empty_input() {
        let mut out: Vec<u8> = Vec::new();
        let n = pipe_through(Cursor::new(b""), &mut out).unwrap();
        assert_eq!(n, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn pipe_through_flushes_trailing_buffered_candidate() {
        let mut out: Vec<u8> = Vec::new();
        pipe_through(Cursor::new(b"trailing /nix/sto"), &mut out).unwrap();
        assert_eq!(out, b"trailing /nix/sto");
    }

    #[test]
    fn pipe_through_commits_when_stream_ends_with_complete_pkg() {
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-pkg");
        let mut out: Vec<u8> = Vec::new();
        pipe_through(Cursor::new(&input), &mut out).unwrap();
        assert_eq!(out, b"nix:");
    }

    /// A `Read` adapter that returns its input in fixed-size chunks.
    struct Chunked<'a> {
        data: &'a [u8],
        chunk: usize,
    }
    impl<'a> Read for Chunked<'a> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.data.is_empty() {
                return Ok(0);
            }
            let n = self.data.len().min(self.chunk).min(buf.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    #[test]
    fn pipe_through_handles_small_chunks() {
        let input = example_path();
        let want = PathRewriter::rewrite_all(&input);
        for chunk in 1..=input.len() {
            let mut out: Vec<u8> = Vec::new();
            pipe_through(
                Chunked {
                    data: &input,
                    chunk,
                },
                &mut out,
            )
            .unwrap();
            assert_eq!(
                out,
                want,
                "chunk size {} produced wrong output",
                chunk
            );
        }
    }

    /// A `Read` that returns `Interrupted` once then the real data.
    struct Interrupting<'a> {
        data: &'a [u8],
        emitted: bool,
        done: bool,
    }
    impl<'a> Read for Interrupting<'a> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.emitted {
                self.emitted = true;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "boom"));
            }
            if self.done {
                return Ok(0);
            }
            let n = self.data.len().min(buf.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            if self.data.is_empty() {
                self.done = true;
            }
            Ok(n)
        }
    }

    #[test]
    fn pipe_through_retries_on_interrupted() {
        let mut input = Vec::new();
        input.extend_from_slice(b"hi /nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-x/bin");
        let mut out: Vec<u8> = Vec::new();
        pipe_through(
            Interrupting {
                data: &input,
                emitted: false,
                done: false,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"hi nix:/bin");
    }

    /// A `Write` that always errors. Confirms I/O errors are propagated.
    struct AlwaysFails;
    impl Write for AlwaysFails {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "nope"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn pipe_through_propagates_write_errors() {
        let mut input = Vec::new();
        input.extend_from_slice(b"/nix/store/");
        input.extend_from_slice(HASH32);
        input.extend_from_slice(b"-x/bin");
        let err = pipe_through(Cursor::new(&input), AlwaysFails).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
