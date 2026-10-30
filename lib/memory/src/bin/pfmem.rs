//! `pfmem PID [TOP]` prints a process resident memory grouped by mapping name.

use std::process::ExitCode;

use pf_memory::smaps;

// An app's main reading a proc file once.
#[allow(clippy::disallowed_methods)]
fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let pid = args.next().unwrap_or_else(|| "self".into());
    let top = args.next().and_then(|t| t.parse().ok()).unwrap_or(12);
    let path = format!("/proc/{pid}/smaps");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("pfmem: {path} cannot be read, {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("resident memory of pid {pid} by mapping name");
    print!("{}", smaps::table(&smaps::parse(&text), top));
    ExitCode::SUCCESS
}
