//! to11y: subscription usage limits, pacing, reset times, and token usage
//! for coding agents, served over MCP on stdio.

mod base64;
mod cache;
mod creds;
mod dbus;
mod env;
mod http;
mod mcp;
mod model;
mod pace;
mod providers;
#[cfg(test)]
mod testing;
mod time;
mod tokens;
mod tools;

use std::io::Write;
use std::process::ExitCode;

const HELP: &str = "\
to11y: subscription usage limits, pacing, reset times, and token usage for
coding agents.

It is an MCP server over stdio. Add it to a coding agent as a stdio server
whose command is `to11y`, with no arguments.

Options:
  -h, --help     Show this help.
  -V, --version  Show the version.
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => {}
        Some("-h" | "--help") => return print(HELP),
        Some("-V" | "--version") => {
            return print(&format!("to11y {}\n", env!("CARGO_PKG_VERSION")));
        }
        Some(other) => {
            eprintln!("to11y: unknown argument {other}. Run to11y --help.");
            return ExitCode::from(2);
        }
    }
    let tools = tools::Tools::new(env::Env::from_process());
    let server = mcp::Server::new(tools, std::io::stdout());
    server.serve(std::io::stdin().lock());
    ExitCode::SUCCESS
}

fn print(text: &str) -> ExitCode {
    match std::io::stdout().write_all(text.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
