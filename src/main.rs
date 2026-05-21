//! `nix-pretty` command-line entry point.
//!
//! Reads the shell to run from the `NIX_PRETTY_SHELL` environment variable
//! (defaulting to `bash`, which is resolved through `PATH`), forwards any
//! remaining argv to the shell, and hands control over to the PTY layer.
//!
//! If no arguments are passed, the shell is started in interactive mode
//! (`-i`), which is what most users want when entering a `nix-shell`.

use std::env;
use std::process::ExitCode;

const USAGE: &str = "\
nix-pretty - wrap a shell so /nix/store paths are rewritten to [nix-store]

USAGE:
  nix-pretty [SHELL_ARGS...]

ENVIRONMENT:
  NIX_PRETTY_SHELL   Path or name of the shell to spawn (default: bash).
                     Resolved through $PATH the same way `execvp` does.

EXAMPLES:
  nix-pretty                              # interactive bash, output rewritten
  nix-pretty -c 'nix-build .'             # one-shot command, output rewritten
  NIX_PRETTY_SHELL=/bin/bash nix-pretty   # pin a specific shell binary
";

fn main() -> ExitCode {
    let mut argv: Vec<String> = env::args().skip(1).collect();

    // Tiny built-in --help / -h, so users can discover the env var.
    if argv.first().map(|s| s.as_str()) == Some("--help")
        || argv.first().map(|s| s.as_str()) == Some("-h")
    {
        print!("{}", USAGE);
        return ExitCode::SUCCESS;
    }

    let shell = env::var("NIX_PRETTY_SHELL").unwrap_or_else(|_| "bash".to_string());
    if argv.is_empty() {
        argv.push("-i".to_string());
    }

    #[cfg(unix)]
    {
        match nix_pretty::pty::run(&shell, &argv) {
            Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
            Err(e) => {
                eprintln!("nix-pretty: {}", e);
                ExitCode::from(1)
            }
        }
    }

    #[cfg(not(unix))]
    {
        eprintln!("nix-pretty: this build does not support non-Unix platforms");
        ExitCode::from(2)
    }
}
