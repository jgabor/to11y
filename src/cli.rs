//! The command line: `to11y <tool>` runs one tool and prints its result as
//! one JSON document, and `to11y tools` lists the tools. With no arguments,
//! to11y is the MCP server instead.

use std::io::{self, Write};
use std::process::ExitCode;

use serde_json::{Map, Value};

use crate::env::Env;
use crate::model::Harness;
use crate::providers::antigravity;
use crate::tools::{self, Tools};

pub const HELP: &str = "\
to11y: subscription usage limits, pacing, reset times, and token usage for
coding agents.

With no arguments, it is an MCP server over stdio. Add it to a coding agent
as a stdio server whose command is `to11y`, with no arguments.

Usage:
  to11y                   Serve MCP on stdin and stdout.
  to11y tools [--pretty]  List the tools, as MCP's tools/list does.
  to11y <tool> [options]  Run one tool and print its result as JSON.

Options for a tool:
  --harness NAME  claude-code, codex, droid, or antigravity. Defaults to
                  TO11Y_HARNESS. The *_all_harnesses tools take none.
  --period P      5h, 24h, 7d, or 30d, for the token tools. Defaults to 24h.
  --pretty        Indent the JSON.

Exit status: 0 for a result, including a stale one, 1 when the result is
an error, and 2 for a bad command line.

Options:
  -h, --help     Show this help.
  -V, --version  Show the version.
";

#[derive(Debug, PartialEq)]
pub enum Command {
    Serve,
    Help,
    Version,
    List {
        pretty: bool,
    },
    Run {
        tool: String,
        arguments: Map<String, Value>,
        pretty: bool,
    },
}

/// The command in `args`, the arguments after the program's name. An error
/// is a sentence for stderr.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some((first, rest)) = args.split_first() else {
        return Ok(Command::Serve);
    };
    match first.as_str() {
        "-h" | "--help" | "-V" | "--version" if !rest.is_empty() => {
            Err(format!("{first} takes nothing after it."))
        }
        "-h" | "--help" => Ok(Command::Help),
        "-V" | "--version" => Ok(Command::Version),
        option if option.starts_with('-') => Err(format!("Unknown option {option}.")),
        "tools" => {
            let (_, pretty, help) = flags(rest, &["pretty"])?;
            Ok(if help {
                Command::Help
            } else {
                Command::List { pretty }
            })
        }
        tool if tools::exists(tool) => {
            let (arguments, pretty, help) = flags(rest, &["harness", "period", "pretty"])?;
            if help {
                return Ok(Command::Help);
            }
            Ok(Command::Run {
                tool: tool.to_owned(),
                arguments,
                pretty,
            })
        }
        other => Err(format!(
            "Unknown tool {other}. Run to11y tools to list them."
        )),
    }
}

/// The flags after a tool name: the ones with a value as tool arguments,
/// then whether `--pretty` and `--help` were given.
fn flags(args: &[String], allowed: &[&str]) -> Result<(Map<String, Value>, bool, bool), String> {
    let mut arguments = Map::new();
    let (mut pretty, mut help) = (false, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" {
            help = true;
            continue;
        }
        let Some(flag) = arg.strip_prefix("--") else {
            return Err(format!("Unexpected argument {arg}."));
        };
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (flag, None),
        };
        if !allowed.contains(&name) {
            return Err(format!("Unknown option --{name}."));
        }
        if name == "pretty" {
            if inline.is_some() {
                return Err("--pretty takes no value.".to_owned());
            }
            if pretty {
                return Err("--pretty is given twice.".to_owned());
            }
            pretty = true;
            continue;
        }
        let value = match inline {
            Some(value) => value,
            None => args
                .next()
                .filter(|value| !value.starts_with("--"))
                .ok_or_else(|| format!("--{name} needs a value."))?,
        };
        if value.is_empty() {
            return Err(format!("--{name} needs a value."));
        }
        if arguments
            .insert(name.to_owned(), Value::String(value.to_owned()))
            .is_some()
        {
            return Err(format!("--{name} is given twice."));
        }
    }
    Ok((arguments, pretty, help))
}

/// Runs a command other than `Serve`.
pub fn run(command: Command, mut env: Env) -> ExitCode {
    let (text, code) = match command {
        Command::Serve => unreachable!("main serves MCP itself"),
        Command::Help => (HELP.trim_end().to_owned(), ExitCode::SUCCESS),
        Command::Version => (
            format!("to11y {}", env!("CARGO_PKG_VERSION")),
            ExitCode::SUCCESS,
        ),
        Command::List { pretty } => (render(&tools::list(), pretty), ExitCode::SUCCESS),
        Command::Run {
            tool,
            arguments,
            pretty,
        } => {
            let arguments = Value::Object(arguments);
            match tools::check(&tool, &arguments) {
                Err(message) => return usage(&message),
                Ok(needs_harness)
                    if needs_harness
                        && arguments.get("harness").is_none()
                        && env.harness.as_deref().and_then(Harness::parse).is_none() =>
                {
                    let names: Vec<&str> = Harness::ALL.iter().map(|h| h.name()).collect();
                    return usage(&format!(
                        "{tool} needs --harness, or TO11Y_HARNESS, set to one of: {}.",
                        names.join(", ")
                    ));
                }
                Ok(_) => {}
            }
            env.cli = true;
            let result = Tools::new(env).call(&tool, &arguments, None);
            let value: Value = result["content"][0]["text"]
                .as_str()
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or(Value::Null);
            let code = if result["isError"] == true {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
            antigravity::wait_for_agy();
            (render(&value, pretty), code)
        }
    };
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
    {
        Ok(()) => code,
        // The reader stopped reading, as `| head` does. Nothing is lost.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => code,
        Err(error) => {
            eprintln!("to11y: couldn't write the result: {error}.");
            ExitCode::FAILURE
        }
    }
}

fn render(value: &Value, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(value).expect("a Value serializes")
    } else {
        value.to_string()
    }
}

/// A bad command line: the reason on stderr, and exit status 2.
pub fn usage(message: &str) -> ExitCode {
    eprintln!("to11y: {message} Run to11y --help.");
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    fn run_of(text: &str) -> (String, Value, bool) {
        match parse(&args(text)).unwrap() {
            Command::Run {
                tool,
                arguments,
                pretty,
            } => (tool, Value::Object(arguments), pretty),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_arguments_serve_mcp() {
        assert_eq!(parse(&[]), Ok(Command::Serve));
    }

    #[test]
    fn help_and_version_stand_alone() {
        assert_eq!(parse(&args("--help")), Ok(Command::Help));
        assert_eq!(parse(&args("-V")), Ok(Command::Version));
        assert!(parse(&args("--help usage_limits")).is_err());
        assert_eq!(parse(&args("usage_limits --help")), Ok(Command::Help));
        assert_eq!(parse(&args("tools -h")), Ok(Command::Help));
    }

    #[test]
    fn a_flag_takes_its_value_after_a_space_or_an_equals_sign() {
        let (tool, arguments, pretty) =
            run_of("token_usage_recent --harness codex --period=7d --pretty");
        assert_eq!(tool, "token_usage_recent");
        assert_eq!(arguments, json!({"harness": "codex", "period": "7d"}));
        assert!(pretty);
        let (_, arguments, pretty) = run_of("usage_limits_all_harnesses");
        assert_eq!((arguments, pretty), (json!({}), false));
    }

    #[test]
    fn a_bad_command_line_says_what_is_wrong() {
        for (line, says) in [
            ("--json", "Unknown option --json."),
            ("cursor_usage", "Unknown tool cursor_usage."),
            ("usage_limits --harness", "--harness needs a value."),
            (
                "usage_limits --harness --pretty",
                "--harness needs a value.",
            ),
            ("usage_limits --harness=", "--harness needs a value."),
            (
                "usage_limits --harness codex --harness droid",
                "--harness is given twice.",
            ),
            ("usage_limits --pretty --pretty", "--pretty is given twice."),
            ("usage_limits --pretty=yes", "--pretty takes no value."),
            ("usage_limits codex", "Unexpected argument codex."),
            ("usage_limits --model x", "Unknown option --model."),
            ("tools --harness codex", "Unknown option --harness."),
        ] {
            let error = parse(&args(line)).unwrap_err();
            assert!(error.starts_with(says), "{line}: {error}");
        }
    }

    #[test]
    fn the_help_fits_in_80_columns() {
        assert!(HELP.lines().all(|line| line.chars().count() <= 80));
    }
}
