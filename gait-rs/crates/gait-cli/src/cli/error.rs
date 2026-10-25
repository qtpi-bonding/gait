//! The one error type a CLI command returns for a failure it has already
//! worded for the user. `main` prints the message once and maps the class to
//! an exit code (`exit_code`); commands never call `std::process::exit`.
use std::process::ExitCode;

#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.cli")]
pub enum CliError {
    /// The command line is wrong: a missing or contradictory argument.
    #[error("{0}")]
    #[coded(code = "usage", class = Logical)]
    Usage(String),
    /// An argument or input file is present but not acceptable.
    #[error("{0}")]
    #[coded(code = "invalid_input", class = Logical)]
    InvalidInput(String),
    /// A session, file, entry or directory the user named does not exist.
    #[error("{0}")]
    #[coded(code = "not_found", class = Logical)]
    NotFound(String),
    /// The operation failed on our side (io, a failed spawn, an internal error).
    #[error("{0}")]
    #[coded(code = "failed", class = Infrastructure)]
    Failed(String),
}

impl CliError {
    pub fn usage(message: impl Into<String>) -> anyhow::Error {
        coded_error::into_anyhow(Self::Usage(message.into()))
    }
    pub fn invalid_input(message: impl Into<String>) -> anyhow::Error {
        coded_error::into_anyhow(Self::InvalidInput(message.into()))
    }
    pub fn not_found(message: impl Into<String>) -> anyhow::Error {
        coded_error::into_anyhow(Self::NotFound(message.into()))
    }
    pub fn failed(message: impl Into<String>) -> anyhow::Error {
        coded_error::into_anyhow(Self::Failed(message.into()))
    }
}

/// What a command returns: `Ok(())` is exit 0. Commands with their own
/// success exit codes (`gait run`, `gait workflow run`, `gait -e`) return
/// `Result<ExitCode>` instead.
pub type CliResult = anyhow::Result<()>;

/// Print `err` once to stderr, exactly as the command worded it, and return
/// its exit code: Logical 65, Infrastructure 69, Transient 75, Unknown 70.
pub fn report(err: &anyhow::Error) -> ExitCode {
    eprintln!("{err:#}");
    ExitCode::from(coded_error::cli::exit_code_anyhow(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_error_codes_are_unique() {
        coded_error::check_codes("gait.cli.", &[CliError::ALL_CODES]).unwrap();
    }

    #[test]
    fn cli_error_exit_codes_follow_the_class() {
        use coded_error::cli::exit_code_anyhow;
        assert_eq!(exit_code_anyhow(&CliError::usage("x")), 65);
        assert_eq!(exit_code_anyhow(&CliError::invalid_input("x")), 65);
        assert_eq!(exit_code_anyhow(&CliError::not_found("x")), 65);
        assert_eq!(exit_code_anyhow(&CliError::failed("x")), 69);
        assert_eq!(exit_code_anyhow(&anyhow::anyhow!("uncoded")), 70);
    }

    #[test]
    fn cli_error_display_is_the_message_verbatim() {
        let err = CliError::usage("gait: needs a session id");
        assert_eq!(format!("{err:#}"), "gait: needs a session id");
    }
}
