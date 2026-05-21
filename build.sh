#!/usr/bin/env bash
#
# build.sh - Local build & install for `nix-pretty`.
#
# This script performs a complete LOCAL release cycle:
#
#   1. Bumps the package version in Cargo.toml (patch by default).
#   2. Builds an optimized release binary via `cargo build --release`,
#      auto-entering `nix-shell` when one is available so the pinned
#      toolchain is used.
#   3. Commits the version bump (Cargo.toml + Cargo.lock) with a
#      "chore(release): local build vX.Y.Z" message.
#   4. Installs `target/release/nix-pretty` to an OS-appropriate
#      location on $PATH.
#
# It deliberately does NOT create a git tag and does NOT push.
# Tagging is reserved for official releases that go through a
# separate, human-driven workflow.
#
# USAGE:
#   ./build.sh [OPTIONS]
#
# OPTIONS:
#   --patch              Bump the patch version (default).
#   --minor              Bump the minor version; reset patch to 0.
#   --major              Bump the major version; reset minor & patch to 0.
#   --no-bump            Do not change Cargo.toml at all.
#   --no-install         Build only; do not copy the binary to $PATH.
#   --no-commit          Skip the version-bump commit (also implies the
#                        commit is skipped when --no-bump is in effect).
#   --no-nix-shell       Do not auto-enter `nix-shell` even if available.
#   --prefix DIR         Override the install directory.
#                        Equivalent to setting PREFIX=DIR in the environment.
#   -h, --help           Show this help and exit.
#
# ENVIRONMENT:
#   PREFIX               Install directory. Default: /usr/local/bin on
#                        both macOS and Linux. This is in the default
#                        $PATH on both OSes and does not assume any
#                        package manager (Homebrew, MacPorts, ...).
#                        If the directory is not writable by the current
#                        user, `sudo` is used for the final copy step.
#
# EXAMPLES:
#   ./build.sh                          # bump patch, build, commit, install
#   ./build.sh --minor                  # 0.1.4 -> 0.2.0
#   ./build.sh --no-bump --no-commit    # just rebuild & reinstall current src
#   ./build.sh --prefix "$HOME/.local/bin"
#

set -euo pipefail

# ---------------------------------------------------------------------------
# Pretty logging.
# ---------------------------------------------------------------------------

if [ -t 1 ]; then
    C_BOLD=$'\033[1m'
    C_BLUE=$'\033[34m'
    C_GREEN=$'\033[32m'
    C_YELLOW=$'\033[33m'
    C_RED=$'\033[31m'
    C_RESET=$'\033[0m'
else
    C_BOLD=""; C_BLUE=""; C_GREEN=""; C_YELLOW=""; C_RED=""; C_RESET=""
fi

log()  { printf '%s==>%s %s\n' "$C_BLUE$C_BOLD" "$C_RESET" "$*"; }
ok()   { printf '%s✓%s %s\n'   "$C_GREEN"        "$C_RESET" "$*"; }
warn() { printf '%s!%s %s\n'   "$C_YELLOW"       "$C_RESET" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$C_RED$C_BOLD" "$C_RESET" "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Argument parsing.
# ---------------------------------------------------------------------------

BUMP="patch"          # patch | minor | major | none
DO_INSTALL=1
DO_COMMIT=1
USE_NIX_SHELL=1
PREFIX="${PREFIX:-}"  # honor env var; --prefix overrides below

print_help() {
    cat <<'HELP'
build.sh - Local build & install for `nix-pretty`.

USAGE:
  ./build.sh [OPTIONS]

OPTIONS:
  --patch              Bump the patch version (default).
  --minor              Bump the minor version; reset patch to 0.
  --major              Bump the major version; reset minor & patch to 0.
  --no-bump            Do not change Cargo.toml at all.
  --no-install         Build only; do not copy the binary to $PATH.
  --no-commit          Skip the version-bump commit.
  --no-nix-shell       Do not auto-enter `nix-shell` even if available.
  --prefix DIR         Override the install directory (also: PREFIX env var).
  -h, --help           Show this help and exit.

DEFAULT INSTALL DIRECTORY (when PREFIX/--prefix is not given):
  /usr/local/bin on both macOS and Linux. In the default $PATH on both
  OSes; no package-manager assumption (Homebrew, MacPorts, ...).
  Uses sudo automatically when the destination is not writable.

EXAMPLES:
  ./build.sh                          # bump patch, build, commit, install
  ./build.sh --minor                  # 0.1.4 -> 0.2.0
  ./build.sh --no-bump --no-commit    # just rebuild & reinstall current src
  ./build.sh --prefix "$HOME/.local/bin"

This script does NOT create a git tag and does NOT push.
Tagging is reserved for official releases.
HELP
}

# Keep a copy of original args so we can forward them through the
# nix-shell re-exec without losing the user's intent.
ORIGINAL_ARGS=("$@")

while [ $# -gt 0 ]; do
    case "$1" in
        --patch)        BUMP="patch";   shift ;;
        --minor)        BUMP="minor";   shift ;;
        --major)        BUMP="major";   shift ;;
        --no-bump)      BUMP="none";    shift ;;
        --no-install)   DO_INSTALL=0;   shift ;;
        --no-commit)    DO_COMMIT=0;    shift ;;
        --no-nix-shell) USE_NIX_SHELL=0; shift ;;
        --prefix)
            [ $# -ge 2 ] || die "--prefix requires a directory argument"
            PREFIX="$2"; shift 2 ;;
        --prefix=*)
            PREFIX="${1#--prefix=}"; shift ;;
        -h|--help)
            print_help; exit 0 ;;
        --)
            shift; break ;;
        *)
            die "unknown argument: $1 (use --help for usage)" ;;
    esac
done

# ---------------------------------------------------------------------------
# Move to the repo root (where this script lives).
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

[ -f Cargo.toml ] || die "Cargo.toml not found in $SCRIPT_DIR"
[ -d .git ] || die "$SCRIPT_DIR is not a git repository"

# ---------------------------------------------------------------------------
# Auto re-exec inside nix-shell so we get the pinned Rust toolchain.
#
# `IN_NIX_SHELL` is set automatically by nix-shell, so this is a clean
# one-shot escape hatch with no risk of recursion.
# ---------------------------------------------------------------------------

if [ "$USE_NIX_SHELL" = "1" ] \
   && [ -z "${IN_NIX_SHELL:-}" ] \
   && [ -f shell.nix ] \
   && command -v nix-shell >/dev/null 2>&1; then
    log "Re-executing inside nix-shell to pick up the pinned toolchain"
    # Quote each original arg safely so it survives the shell -> shell hop.
    quoted=""
    if [ ${#ORIGINAL_ARGS[@]} -gt 0 ]; then
        quoted="$(printf ' %q' "${ORIGINAL_ARGS[@]}")"
    fi
    exec nix-shell --run "./build.sh --no-nix-shell${quoted}"
fi

# ---------------------------------------------------------------------------
# Tool sanity checks.
# ---------------------------------------------------------------------------

command -v cargo >/dev/null 2>&1 \
    || die "cargo not found in PATH. Run inside nix-shell or install Rust."
command -v git   >/dev/null 2>&1 || die "git not found in PATH."
command -v awk   >/dev/null 2>&1 || die "awk not found in PATH."
command -v install >/dev/null 2>&1 || die "install(1) not found in PATH."

# ---------------------------------------------------------------------------
# OS detection -> default install prefix.
#
# We deliberately use /usr/local/bin on both macOS and Linux. It is on the
# default $PATH on both OSes, it is the conventional "third-party
# user-installed binary" directory, and it makes no assumption about any
# package manager being present (Homebrew, MacPorts, ...). The sudo
# fallback below handles the typical "owned by root" case transparently.
# ---------------------------------------------------------------------------

UNAME_S="$(uname -s)"
case "$UNAME_S" in
    Darwin) OS="macos" ;;
    Linux)  OS="linux" ;;
    *) die "unsupported OS: $UNAME_S (this project targets macOS and Linux only)" ;;
esac

if [ -z "$PREFIX" ]; then
    PREFIX="/usr/local/bin"
fi

# ---------------------------------------------------------------------------
# Read and (optionally) bump the version in Cargo.toml.
#
# We rewrite the file with awk into a temp sibling and `mv` it into place,
# which avoids the BSD-vs-GNU `sed -i` portability mess.
# ---------------------------------------------------------------------------

read_version() {
    # Match the first `version = "X.Y.Z"` line inside the [package] table.
    awk '
        /^\[/ { in_pkg = ($0 == "[package]") ? 1 : 0; next }
        in_pkg && /^version[[:space:]]*=[[:space:]]*"/ {
            # Extract the contents between the first pair of double-quotes.
            sub(/^[^"]*"/, "")
            sub(/".*$/, "")
            print
            exit
        }
    ' Cargo.toml
}

bump_version() {
    local kind="$1" current major minor patch new
    current="$(read_version)"
    [ -n "$current" ] || die "could not read current version from Cargo.toml"

    # Strip any pre-release / build-metadata suffix before splitting.
    local core="${current%%[-+]*}"
    IFS='.' read -r major minor patch <<<"$core"

    [[ "$major" =~ ^[0-9]+$ ]] || die "non-numeric major in version '$current'"
    [[ "$minor" =~ ^[0-9]+$ ]] || die "non-numeric minor in version '$current'"
    [[ "$patch" =~ ^[0-9]+$ ]] || die "non-numeric patch in version '$current'"

    case "$kind" in
        major) major=$((major + 1)); minor=0; patch=0 ;;
        minor) minor=$((minor + 1));            patch=0 ;;
        patch) patch=$((patch + 1)) ;;
        *) die "unknown bump kind: $kind" ;;
    esac
    new="${major}.${minor}.${patch}"

    awk -v new="$new" '
        BEGIN { in_pkg = 0; done = 0 }
        /^\[/ { in_pkg = ($0 == "[package]") ? 1 : 0 }
        !done && in_pkg && /^version[[:space:]]*=[[:space:]]*"/ {
            print "version = \"" new "\""
            done = 1
            next
        }
        { print }
    ' Cargo.toml > Cargo.toml.tmp
    mv Cargo.toml.tmp Cargo.toml

    printf '%s' "$new"
}

CURRENT_VERSION="$(read_version)"
[ -n "$CURRENT_VERSION" ] || die "could not read current version from Cargo.toml"

if [ "$BUMP" = "none" ]; then
    NEW_VERSION="$CURRENT_VERSION"
    log "Version: $CURRENT_VERSION (no bump)"
else
    NEW_VERSION="$(bump_version "$BUMP")"
    log "Version: $CURRENT_VERSION -> $NEW_VERSION (bump: $BUMP)"
fi

# ---------------------------------------------------------------------------
# Build release binary.
#
# We deliberately omit `--locked` here: bumping the version in Cargo.toml
# requires Cargo.lock to be updated to match, which `cargo build` does
# automatically on its own.
# ---------------------------------------------------------------------------

log "Building release binary (cargo build --release)"
cargo build --release

BIN_SRC="target/release/nix-pretty"
[ -x "$BIN_SRC" ] || die "expected binary not found at $BIN_SRC after build"

ok "Built $BIN_SRC"

# ---------------------------------------------------------------------------
# Commit the version bump (only Cargo.toml + Cargo.lock).
# ---------------------------------------------------------------------------

if [ "$DO_COMMIT" = "1" ] && [ "$BUMP" != "none" ]; then
    log "Committing version bump to v$NEW_VERSION"
    git add Cargo.toml Cargo.lock
    if git diff --cached --quiet -- Cargo.toml Cargo.lock; then
        warn "no changes staged for Cargo.toml/Cargo.lock; skipping commit"
    else
        git commit -m "chore(release): local build v${NEW_VERSION}"
        ok "Committed: chore(release): local build v${NEW_VERSION}"
    fi
elif [ "$BUMP" = "none" ]; then
    log "Skipping commit (--no-bump implies no version-bump commit)"
else
    log "Skipping commit (--no-commit)"
fi

# ---------------------------------------------------------------------------
# Install the binary.
# ---------------------------------------------------------------------------

if [ "$DO_INSTALL" = "1" ]; then
    DST="$PREFIX/nix-pretty"
    log "Installing to $DST"

    # Make sure the destination directory exists; create with sudo if needed.
    if [ ! -d "$PREFIX" ]; then
        if mkdir -p "$PREFIX" 2>/dev/null; then
            :
        else
            warn "creating $PREFIX requires elevated privileges"
            sudo mkdir -p "$PREFIX"
        fi
    fi

    # Pick whichever path can actually write to $PREFIX.
    if [ -w "$PREFIX" ]; then
        install -m 0755 "$BIN_SRC" "$DST"
    else
        warn "writing to $PREFIX requires elevated privileges; using sudo"
        sudo install -m 0755 "$BIN_SRC" "$DST"
    fi

    ok "Installed nix-pretty v${NEW_VERSION} to $DST"

    # Friendly PATH hint when the install directory is not on PATH.
    case ":$PATH:" in
        *":$PREFIX:"*) : ;;
        *) warn "$PREFIX is not on your \$PATH; add it to use nix-pretty by name" ;;
    esac

    # Show the installed version as a final sanity check.
    if "$DST" --help >/dev/null 2>&1; then
        ok "Binary at $DST runs cleanly"
    else
        warn "installed binary did not respond to --help; inspect it manually"
    fi
else
    log "Skipping install (--no-install)"
fi

# ---------------------------------------------------------------------------
# Summary.
# ---------------------------------------------------------------------------

log "Done."
printf '    version : %s\n' "$NEW_VERSION"
printf '    binary  : %s\n' "$SCRIPT_DIR/$BIN_SRC"
if [ "$DO_INSTALL" = "1" ]; then
    printf '    install : %s\n' "$PREFIX/nix-pretty"
fi
if [ "$DO_COMMIT" = "1" ] && [ "$BUMP" != "none" ]; then
    printf '    commit  : %s\n' "chore(release): local build v${NEW_VERSION}"
fi
printf '    tag     : (not created - tags are reserved for official releases)\n'
