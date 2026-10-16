//! Checks one file the way the launcher checks an app's installer before running it
//! (`authenticode.rs`). Windows only; .github/workflows/windows-check.yml uses it.
//!
//! Exit codes: 0 accepted, 1 refused (the reason is printed), 2 no file given or not Windows.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(windows)]
    {
        let Some(file) = std::env::args_os().nth(1) else {
            eprintln!("usage: check_signature <file>");
            return ExitCode::from(2);
        };
        let file = std::path::PathBuf::from(file);
        match artcraft_launcher_native::authenticode::verify(&file) {
            Ok(()) => {
                println!("accepted: {}", file.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                println!("refused: {e}");
                ExitCode::from(1)
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("check_signature only runs on Windows");
        ExitCode::from(2)
    }
}
