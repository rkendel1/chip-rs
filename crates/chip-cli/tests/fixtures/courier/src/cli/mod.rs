//! The `courier` command line. It talks to an in-process loopback server, so it works anywhere
//! and never needs a network; it exists to exercise configuration, retries and the journal.

pub mod args;
pub mod commands;
pub mod output;

use crate::error::CourierError;

/// Runs the command line and returns the process exit code.
pub fn run(argv: &[String], env: &[(String, String)]) -> i32 {
    match dispatch(argv, env) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) => {
            eprintln!("courier: {e}");
            e.kind.exit_code()
        }
    }
}

pub fn dispatch(argv: &[String], env: &[(String, String)]) -> Result<String, CourierError> {
    let parsed = args::parse(argv)?;
    commands::execute(&parsed, env)
}
