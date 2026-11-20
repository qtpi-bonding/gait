//! Interim routing while the management verbs are still the old clap enum
//! (Tasks 6-9 port them; Task 9 deletes this module). The old parse keeps the
//! verbs, `--help`, `--version` and the bare `gait`; everything else is the
//! registry's, whose free-text fallback runs the turn.
use op_registry::cli::edit_distance;

/// Who handles a command line.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// The old clap parse and its `cmd_*` functions.
    Old,
    /// A single word one or two edits from a verb: a usage error, never a turn.
    NearMiss(String),
    /// The registry (the turn is its fallback).
    Registry,
}

/// How many leading tokens are registry globals with their values.
fn skip_globals(tokens: &[String]) -> usize {
    let mut at = 0;
    while let Some(token) = tokens.get(at) {
        match token.as_str() {
            "--pretty" | "--compact" | "--text" => at += 1,
            "--gait-dir" | "--agent-dir" => at += 2,
            t if t.starts_with("--gait-dir=") || t.starts_with("--agent-dir=") => at += 1,
            _ => break,
        }
    }
    at.min(tokens.len())
}

/// Routes `raw` (program name first). `verbs` are the old enum's command words,
/// `registry_verbs` those of the registry's ops.
pub fn route(raw: &[String], verbs: &[String], registry_verbs: &[String]) -> Route {
    let tokens = raw.get(1..).unwrap_or_default();
    let rest = &tokens[skip_globals(tokens)..];
    let Some(first) = rest.first() else {
        return Route::Old;
    };
    if matches!(first.as_str(), "-h" | "--help" | "--version") || first == "help" {
        return Route::Old;
    }
    if first.starts_with('-') {
        return Route::Registry;
    }
    if registry_verbs.contains(first) {
        return Route::Registry;
    }
    if verbs.contains(first) {
        return Route::Old;
    }
    if rest.len() == 1 && !first.chars().any(char::is_whitespace) {
        let near = verbs
            .iter()
            .chain(registry_verbs)
            .map(|word| (edit_distance(first, word), word))
            .filter(|(distance, _)| *distance <= 2)
            .min_by_key(|(distance, _)| *distance);
        if let Some((_, word)) = near {
            return Route::NearMiss(format!("`{first}`; did you mean `{word}`?"));
        }
    }
    Route::Registry
}

/// The command words of an old clap command.
pub fn verbs(command: &clap::Command) -> Vec<String> {
    command.get_subcommands().map(|c| c.get_name().to_owned()).collect()
}

/// The top-level command words of a registry.
pub fn registry_verbs(registry: &op_registry::Registry<crate::ops::GaitContext>) -> Vec<String> {
    let command = op_registry::cli::command(registry);
    let words = verbs(&command);
    words.into_iter().filter(|word| word != "help").collect()
}

/// `old` plus the registry's commands it does not have, so the help and the
/// manual list every command.
pub fn with_registry_commands(
    old: clap::Command,
    registry: &op_registry::Registry<crate::ops::GaitContext>,
) -> clap::Command {
    let have = verbs(&old);
    let mut command = old;
    for sub in op_registry::cli::command(registry).get_subcommands() {
        if !have.iter().any(|word| word == sub.get_name()) && sub.get_name() != "help" {
            command = command.subcommand(sub.clone());
        }
    }
    command
}

/// Whether `raw` asks for the root help: nothing, `--help`, `-h` or `help`,
/// after the globals.
pub fn is_root_help(raw: &[String]) -> bool {
    let tokens = raw.get(1..).unwrap_or_default();
    let rest = &tokens[skip_globals(tokens)..];
    match rest {
        [] => true,
        [only] => matches!(only.as_str(), "-h" | "--help" | "help"),
        _ => false,
    }
}

/// The usage error for [`Route::NearMiss`], exit code 2.
pub fn near_miss_exit(detail: String) -> u8 {
    let error = op_registry::cli::CliError::UnknownCommand {
        name: detail,
    };
    crate::ops::usage_error_exit(coded_error::into_anyhow(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verbs() -> Vec<String> {
        ["sessions", "stats", "log", "ls"].map(String::from).to_vec()
    }
    fn route_of(args: &[&str]) -> Route {
        let raw: Vec<String> = std::iter::once("gait").chain(args.iter().copied()).map(String::from).collect();
        route(&raw, &verbs(), &["ps".to_owned(), "persist".to_owned()])
    }

    #[test]
    fn verbs_help_and_nothing_are_old() {
        assert_eq!(route_of(&[]), Route::Old);
        assert_eq!(route_of(&["--gait-dir", "d"]), Route::Old);
        assert_eq!(route_of(&["stats", "x"]), Route::Old);
        assert_eq!(route_of(&["--gait-dir", "d", "log"]), Route::Old);
        assert_eq!(route_of(&["--help"]), Route::Old);
        assert_eq!(route_of(&["ps", "--id", "x"]), Route::Registry);
        assert_eq!(route_of(&["--gait-dir", "d", "persist", "--all"]), Route::Registry);
    }

    #[test]
    fn text_flags_and_escape_are_the_registrys() {
        assert_eq!(route_of(&["log the failures"]), Route::Registry);
        assert_eq!(route_of(&["--", "log"]), Route::Registry);
        assert_eq!(route_of(&["--inference", "ls", "x"]), Route::Registry);
        assert_eq!(route_of(&["-s", "sessions", "x"]), Route::Registry);
        assert_eq!(route_of(&["refactor"]), Route::Registry);
        assert_eq!(route_of(&["log", "the", "failures"]), Route::Old);
    }

    #[test]
    fn a_single_near_miss_word_is_a_usage_error() {
        assert!(matches!(route_of(&["sesions"]), Route::NearMiss(_)));
        assert!(matches!(route_of(&["statz"]), Route::NearMiss(_)));
        assert_eq!(route_of(&["sesions", "x"]), Route::Registry);
        assert!(matches!(route_of(&["persst"]), Route::NearMiss(_)));
    }
}
