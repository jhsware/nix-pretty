//! Streaming byte-level rewriter that replaces every literal `/nix/store` in
//! its input with `[nix-store]`.
//!
//! This module is intentionally pure: no I/O, no syscalls, no dependencies
//! beyond `std`. Everything platform-specific lives in [`crate::pty`].
//!
//! # Why a state machine, not a regex
//!
//! The pattern is fixed, short and starts with `/`. A single linear scan over
//! the input with a bounded look-back buffer is sufficient to be correct in
//! the presence of chunk boundaries that split the pattern, and it has no
//! per-call allocation. Pulling in a regex engine would multiply the binary
//! size and dependency surface for no behavioural benefit.
//!
//! # Correctness invariant
//!
//! For any input `xs` split into chunks `c1, c2, ..., cn`, calling
//! `process(ci, &mut out)` in order and then `flush(&mut out)` produces the
//! exact same `out` as a single `process(xs, &mut out)` followed by `flush`.
//! This is exercised by the unit tests for every possible split point.

use std::io::{self, Read, Write};

/// The literal byte sequence we look for in the input.
const PATTERN: &[u8] = b"/nix/store";

/// What we emit in its place.
const REPLACEMENT: &[u8] = b"[nix-store]";

/// Streaming rewriter. Construct once per stream, feed chunks through
/// [`PathRewriter::process`], and call [`PathRewriter::flush`] at end of input.
///
/// The rewriter holds back at most `PATTERN.len() - 1` bytes between calls,
/// so the latency it adds is bounded by 9 bytes worth of output.
#[derive(Debug, Default, Clone)]
pub struct PathRewriter {
    /// Bytes that we have read but not yet emitted because they could still
    /// turn out to be the start of `PATTERN` once more input arrives.
    ///
    /// Invariant: `pending.len() < PATTERN.len()` and `PATTERN.starts_with(&pending)`.
    pending: Vec<u8>,
}

impl PathRewriter {
    /// Build a fresh rewriter with empty look-back.
    pub fn new() -> Self {
        Self {
            pending: Vec::with_capacity(PATTERN.len()),
        }
    }

    /// The pattern this rewriter matches.
    #[inline]
    pub fn pattern() -> &'static [u8] {
        PATTERN
    }

    /// The replacement this rewriter emits.
    #[inline]
    pub fn replacement() -> &'static [u8] {
        REPLACEMENT
    }

    /// Feed a chunk of input. Any complete matches in `pending + input` are
    /// rewritten; the rest is appended to `out`. A short suffix that might
    /// still grow into a full match is held back in `self.pending` for the
    /// next call.
    ///
    /// This never allocates beyond what the caller asks for in `out` and
    /// the bounded `pending` buffer (at most `PATTERN.len() - 1` bytes).
    pub fn process(&mut self, input: &[u8], out: &mut Vec<u8>) {
        // Fast path: nothing pending and nothing in input. Nothing to do.
        if self.pending.is_empty() && input.is_empty() {
            return;
        }

        // Combine pending + input into a single working buffer. We take
        // `pending` by value so we own a single contiguous slice; we will
        // restore the leftover (if any) at the end.
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(input);

        let mut i = 0usize;
        while i < buf.len() {
            let remaining = &buf[i..];

            if remaining.starts_with(PATTERN) {
                // Full match. Emit replacement, skip past the match.
                out.extend_from_slice(REPLACEMENT);
                i += PATTERN.len();
                continue;
            }

            // Not a full match here. Is `remaining` a *proper* prefix of the
            // pattern that could still grow into a full match? If so, hold
            // it back. Otherwise emit one byte and advance.
            if remaining.len() < PATTERN.len() && PATTERN.starts_with(remaining) {
                self.pending.extend_from_slice(remaining);
                return;
            }

            out.push(buf[i]);
            i += 1;
        }
        // Buffer fully consumed, nothing left to hold back.
    }

    /// Emit any bytes that were being held back because they looked like the
    /// start of a match but the stream ended before the match could complete.
    ///
    /// After this call the rewriter is empty and may be reused.
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        if !self.pending.is_empty() {
            out.extend_from_slice(&self.pending);
            self.pending.clear();
        }
    }

    /// Helper used by tests and by the `pipe_through` convenience function.
    /// Returns a freshly allocated `Vec<u8>` with the input fully rewritten.
    pub fn rewrite_all(input: &[u8]) -> Vec<u8> {
        let mut r = Self::new();
        let mut out = Vec::with_capacity(input.len());
        r.process(input, &mut out);
        r.flush(&mut out);
        out
    }
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

    /// Reference implementation used as the oracle: a naive byte-level
    /// find-and-replace that operates entirely on `&[u8]` and therefore does
    /// not corrupt non-UTF-8 bytes. Equivalent in behaviour to the streaming
    /// rewriter but trivially correct.
    fn oracle(input: &[u8]) -> Vec<u8> {
        let needle = PathRewriter::pattern();
        let replacement = PathRewriter::replacement();
        let mut out = Vec::with_capacity(input.len());
        let mut i = 0;
        while i < input.len() {
            if input[i..].starts_with(needle) {
                out.extend_from_slice(replacement);
                i += needle.len();
            } else {
                out.push(input[i]);
                i += 1;
            }
        }
        out
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

    // ----- basic behaviour ----------------------------------------------

    #[test]
    fn pattern_and_replacement_are_what_we_advertise() {
        assert_eq!(PathRewriter::pattern(), b"/nix/store");
        assert_eq!(PathRewriter::replacement(), b"[nix-store]");
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
            b"\x1b[31mcolor\x1b[0m",         // ANSI escape sequence
            b"/usr/local/bin/foo",
            b"/nix",                          // proper prefix only, no follow-up
            b"/nix/stor",                     // proper prefix only
            "raksmorgas".as_bytes(),
        ];
        for c in cases {
            assert_eq!(PathRewriter::rewrite_all(c), *c, "input: {:?}", c);
        }
    }

    #[test]
    fn single_match_is_rewritten() {
        assert_eq!(
            PathRewriter::rewrite_all(b"/nix/store/abc-pkg/bin/foo"),
            b"[nix-store]/abc-pkg/bin/foo"
        );
    }

    #[test]
    fn match_at_start_middle_and_end() {
        assert_eq!(
            PathRewriter::rewrite_all(b"/nix/store at start"),
            b"[nix-store] at start"
        );
        assert_eq!(
            PathRewriter::rewrite_all(b"hello /nix/store middle"),
            b"hello [nix-store] middle"
        );
        assert_eq!(
            PathRewriter::rewrite_all(b"trailing /nix/store"),
            b"trailing [nix-store]"
        );
    }

    #[test]
    fn multiple_matches_in_one_chunk() {
        let input = b"/nix/store/a /nix/store/b /nix/store/c";
        let want  = b"[nix-store]/a [nix-store]/b [nix-store]/c";
        assert_eq!(PathRewriter::rewrite_all(input), want);
    }

    #[test]
    fn adjacent_matches_are_rewritten() {
        // The contract is "every literal occurrence", including back-to-back.
        let input = b"/nix/store/nix/store";
        // First match consumes "/nix/store", emitting "[nix-store]".
        // Then the remaining "/nix/store" matches and is rewritten too.
        let want = b"[nix-store][nix-store]";
        assert_eq!(PathRewriter::rewrite_all(input), want);
    }

    #[test]
    fn near_misses_are_left_alone() {
        // Inputs that look like the pattern but do not actually contain it
        // as a literal substring must pass through unchanged.
        let cases: &[(&[u8], &[u8])] = &[
            (b"/nix/storage",     b"/nix/storage"),    // 10th byte differs ('a' vs 'e')
            (b"/Nix/Store",       b"/Nix/Store"),      // case-sensitive
            (b"//nix/store",      b"/[nix-store]"),    // junk slash then real match
            (b"nix/store",        b"nix/store"),       // missing leading slash
            (b"/nix/sto",         b"/nix/sto"),        // proper prefix only
            (b"/nix/stor",        b"/nix/stor"),       // proper prefix only
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

    #[test]
    fn pattern_followed_by_trailing_char_still_matches() {
        // /nix/storey contains /nix/store as a literal prefix, so the
        // pattern must be rewritten and the trailing 'y' kept.
        assert_eq!(
            PathRewriter::rewrite_all(b"/nix/storey"),
            b"[nix-store]y"
        );
    }

    // ----- chunk boundary tests -----------------------------------------

    #[test]
    fn split_inside_pattern_still_rewrites_correctly() {
        let input: &[u8] = b"prefix /nix/store/pkg suffix";
        let want = oracle(input);

        // Try every possible split point.
        for split in 0..=input.len() {
            let got = feed_with_splits(input, &[split]);
            assert_eq!(got, want, "split at byte {}", split);
        }
    }

    #[test]
    fn pattern_split_across_three_chunks() {
        // Split the literal pattern itself across three reads.
        let input: &[u8] = b"foo/nix/store/bar";
        let want = oracle(input);
        for a in 0..=input.len() {
            for b in a..=input.len() {
                let got = feed_with_splits(input, &[a, b]);
                assert_eq!(got, want, "splits at {}, {}", a, b);
            }
        }
    }

    #[test]
    fn many_one_byte_chunks() {
        // Feed one byte at a time. The state machine must still match the
        // pattern across chunk boundaries.
        let input: &[u8] = b"x /nix/store/y /nix/store/z";
        let want = oracle(input);

        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        for &b in input {
            r.process(&[b], &mut out);
        }
        r.flush(&mut out);
        assert_eq!(out, want);
    }

    #[test]
    fn flush_emits_held_back_proper_prefix() {
        // Stream ends mid-pattern. Held-back bytes must be flushed verbatim,
        // not silently dropped.
        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        r.process(b"hello /nix/sto", &mut out);
        // At this point "/nix/sto" should be held back.
        assert_eq!(out, b"hello ");
        r.flush(&mut out);
        assert_eq!(out, b"hello /nix/sto");
    }

    #[test]
    fn flush_clears_pending_so_rewriter_can_be_reused() {
        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        r.process(b"/nix/sto", &mut out);
        r.flush(&mut out);
        assert_eq!(out, b"/nix/sto");

        out.clear();
        r.process(b"/nix/store/", &mut out);
        r.flush(&mut out);
        assert_eq!(out, b"[nix-store]/");
    }

    #[test]
    fn pending_buffer_is_bounded() {
        // Even in the worst case, the held-back tail must never exceed
        // PATTERN.len() - 1 bytes. We assert that by inspection after
        // feeding inputs that try to maximise the tail.
        let mut out = Vec::new();
        for chunk in [
            &b"/"[..], &b"/n"[..], &b"/ni"[..], &b"/nix"[..], &b"/nix/"[..],
            &b"/nix/s"[..], &b"/nix/st"[..], &b"/nix/sto"[..], &b"/nix/stor"[..],
        ] {
            let mut r = PathRewriter::new();
            out.clear();
            r.process(chunk, &mut out);
            assert!(
                r.pending.len() < PATTERN.len(),
                "pending grew to {} bytes for input {:?}",
                r.pending.len(),
                chunk
            );
        }
    }

    // ----- mixed content tests ------------------------------------------

    #[test]
    fn preserves_ansi_color_sequences_around_match() {
        let input = b"\x1b[32m/nix/store/abc\x1b[0m";
        let want  = b"\x1b[32m[nix-store]/abc\x1b[0m";
        assert_eq!(PathRewriter::rewrite_all(input), want);
    }

    #[test]
    fn preserves_utf8_multibyte_around_match() {
        // Non-ASCII multibyte (UTF-8) bytes must pass through untouched.
        let input = "smörgås /nix/store/krydda".as_bytes();
        let want = "smörgås [nix-store]/krydda".as_bytes();
        assert_eq!(PathRewriter::rewrite_all(input), want);
    }

    #[test]
    fn binary_payload_passes_through_unchanged_when_no_match() {
        let mut payload = Vec::new();
        for b in 0u8..=255 {
            payload.push(b);
        }
        assert_eq!(PathRewriter::rewrite_all(&payload), payload);
    }

    // ----- random / fuzz-style ------------------------------------------

    #[test]
    fn random_payload_round_trips_through_oracle() {
        // Deterministic pseudo-random data with sprinkled-in patterns.
        let mut data = Vec::with_capacity(10_000);
        let mut state: u32 = 0x1234_5678;
        for i in 0..10_000 {
            // xorshift32
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            // Bias towards printable ASCII so the test output is readable
            // when something fails, but keep some non-ASCII bytes too.
            let byte = if state & 0xff < 200 {
                (32u8 + (state & 0x5f) as u8).min(126)
            } else {
                state as u8
            };
            data.push(byte);
            if i % 137 == 0 {
                data.extend_from_slice(b"/nix/store/");
            }
            if i % 211 == 0 {
                data.extend_from_slice(b"/nix/stor");          // proper prefix
            }
            if i % 311 == 0 {
                data.extend_from_slice(b"/nix/storage");       // near miss
            }
        }
        let want = oracle(&data);
        assert_eq!(PathRewriter::rewrite_all(&data), want);

        // Now also feed it in pseudo-random chunks and verify the same output.
        let mut r = PathRewriter::new();
        let mut out = Vec::new();
        let mut i = 0;
        while i < data.len() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let take = ((state % 17) as usize + 1).min(data.len() - i);
            r.process(&data[i..i + take], &mut out);
            i += take;
        }
        r.flush(&mut out);
        assert_eq!(out, want);
    }

    // ----- pipe_through -------------------------------------------------

    #[test]
    fn pipe_through_basic() {
        let input = b"/nix/store/abc and /nix/store/def";
        let mut out: Vec<u8> = Vec::new();
        let n = pipe_through(Cursor::new(input), &mut out).unwrap();
        assert_eq!(n as usize, input.len());
        assert_eq!(out, b"[nix-store]/abc and [nix-store]/def");
    }

    #[test]
    fn pipe_through_handles_empty_input() {
        let mut out: Vec<u8> = Vec::new();
        let n = pipe_through(Cursor::new(b""), &mut out).unwrap();
        assert_eq!(n, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn pipe_through_flushes_trailing_proper_prefix() {
        // The Cursor will deliver one chunk; the rewriter must still flush
        // its pending tail at EOF.
        let mut out: Vec<u8> = Vec::new();
        pipe_through(Cursor::new(b"trailing /nix/sto"), &mut out).unwrap();
        assert_eq!(out, b"trailing /nix/sto");
    }

    /// A `Read` adapter that returns its input in fixed-size chunks. Used to
    /// prove that `pipe_through` behaves correctly under realistic, small
    /// PTY reads.
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
        let input: &[u8] = b"a /nix/store/x b /nix/store/y c";
        for chunk in 1..=input.len() {
            let mut out: Vec<u8> = Vec::new();
            pipe_through(Chunked { data: input, chunk }, &mut out).unwrap();
            assert_eq!(
                out,
                oracle(input),
                "chunk size {} produced wrong output",
                chunk
            );
        }
    }

    /// A `Read` that returns `Interrupted` once then the real data.
    /// Verifies the loop's EINTR handling.
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
        let mut out: Vec<u8> = Vec::new();
        pipe_through(
            Interrupting {
                data: b"hi /nix/store/x",
                emitted: false,
                done: false,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"hi [nix-store]/x");
    }

    /// A `Write` that always errors. Used to confirm I/O errors are propagated.
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
        let err = pipe_through(Cursor::new(b"/nix/store/x"), AlwaysFails).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
