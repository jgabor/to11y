//! to11y: subscription usage limits, pacing, reset times, and token usage
//! for coding agents, served over MCP on stdio or run from the command line.

mod base64;
mod cache;
mod cli;
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

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Option<Vec<String>> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string().ok())
        .collect();
    let Some(args) = args else {
        return cli::usage("Arguments must be UTF-8.");
    };
    let command = match cli::parse(&args) {
        Ok(command) => command,
        Err(message) => return cli::usage(&message),
    };
    let env = env::Env::from_process();
    if command != cli::Command::Serve {
        return cli::run(command, env);
    }
    let tools = tools::Tools::new(env);
    let server = mcp::Server::new(tools, std::io::stdout());
    server.serve(std::io::stdin().lock());
    ExitCode::SUCCESS
}
