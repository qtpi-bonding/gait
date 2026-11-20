//! Auth ops: provider credentials (API keys, subscription logins, links to
//! another tool's credentials) and the model catalog. Thin over `gait_ops::auth`.
//! No op ever returns or prints key material.
use gait_cli::cli::error::CliError;
use op_registry::{CallOptions, OpParams, Ref, UsageError};
use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;

const SIGNIN: &str = "openai-signin";
const CODEX: &str = "openai-codex";

output! {
    /// One configured provider. Never carries any part of a key.
    pub struct AuthEntry {
        /// The provider name.
        pub provider: String,
        /// `api`, `oauth`, `wellknown`, `subscription`, `external` or `unknown`.
        pub kind: String,
        /// For subscriptions and external links, their status; absent otherwise.
        pub status: Option<String>,
    }
}

output! {
    /// A provider's status word.
    pub struct AuthStatus {
        pub provider: String,
        /// The credential kind, or the subscription or external-link status; absent when the provider has no credential.
        pub status: Option<String>,
    }
}

output! {
    /// A credential that was stored or linked.
    pub struct AuthSaved {
        pub provider: String,
        /// A non-secret word: `api key`, `subscription (...)` or `external (<source>)`.
        pub display: String,
        /// `saved` or `linked`.
        pub action: String,
    }
}

output! {
    /// A credential or link that was removed.
    pub struct AuthRemoved {
        pub provider: String,
        /// `removed` or `unlinked`.
        pub action: String,
    }
}

output! {
    /// What `auth login` did: finished, or begun and waiting for the redirect URL.
    pub struct AuthLogin {
        pub provider: String,
        /// `saved` when the login finished, `pending` when the browser step is still to do.
        pub action: String,
        /// A non-secret word for the stored credential; absent while pending.
        pub display: Option<String>,
        /// Open this URL to sign in; present while pending.
        pub authorization_url: Option<String>,
        /// How long the pending login stays valid; present while pending.
        pub expires_in_seconds: Option<u64>,
    }
}

output! {
    /// What `auth logout` did.
    pub struct AuthLogout {
        pub provider: String,
        /// Always `logged_out`.
        pub action: String,
        /// For `openai-signin`: whether the app's grant was revoked remotely; absent otherwise.
        pub revoked: Option<bool>,
    }
}

output! {
    /// One model of the models.dev catalog.
    pub struct ModelItem {
        /// The model id.
        pub id: String,
        /// The provider that serves it.
        pub provider: String,
        /// The display name.
        pub name: String,
        /// The context window in tokens, when known.
        pub context_length: Option<u64>,
    }
}

/// The provider every auth op but `list` acts on.
#[derive(Debug, Clone, OpParams)]
pub struct ProviderId {
    /// The provider name, for example `openai-codex` or `anthropic`.
    #[op(subject = "provider", id_only)]
    pub id: Ref,
}

#[derive(Debug, Clone, OpParams)]
pub struct AuthListParams {}

#[derive(Debug, Clone, OpParams)]
pub struct AuthLinkParams {
    /// The provider name.
    #[op(subject = "provider", id_only)]
    pub id: Ref,
    /// The tool whose credentials are used, for example `codex`.
    pub source: String,
    /// Override that tool's home directory.
    pub home: Option<PathBuf>,
}

#[derive(Debug, Clone, OpParams)]
pub struct AuthLoginParams {
    /// The provider to sign in to: `openai-codex` (device code) or `openai-signin` (browser).
    #[op(subject = "provider", id_only)]
    pub id: Ref,
    /// Finish a pending `openai-signin` login with the URL you were redirected to, or `-` to read it from stdin.
    #[op(secret)]
    pub complete: Option<String>,
}

#[derive(Debug, Clone, OpParams)]
pub struct AuthLogoutParams {
    /// The provider to sign out of: `openai-codex` or `openai-signin`.
    #[op(subject = "provider", id_only)]
    pub id: Ref,
    /// For `openai-signin`: also forget its registration.
    pub forget: bool,
}

#[derive(Debug, Clone, OpParams)]
pub struct ModelsParams {
    /// Keep only models whose provider, id or name contains this text (case-insensitive).
    pub filter: Option<String>,
}

/// An auth failure that is not the caller's mistake: keeps a code it already
/// has, else is infrastructure (exit 69), as before.
fn failed(error: anyhow::Error) -> anyhow::Error {
    if coded_error::from_anyhow(&error).is_some() {
        error
    } else {
        CliError::failed(format!("{error:#}"))
    }
}

/// The first line of `input`, trimmed.
fn first_line<R: std::io::Read>(input: R) -> std::io::Result<String> {
    let mut line = String::new();
    std::io::BufReader::new(input).read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

/// A callback URL given as itself or as `-` for one trimmed line of `stdin`.
pub fn read_callback_arg<R: std::io::Read>(arg: &str, stdin: R) -> anyhow::Result<String> {
    if arg != "-" {
        return Ok(arg.to_owned());
    }
    let value = first_line(stdin)?;
    if value.is_empty() {
        return Err(CliError::usage("no callback URL provided on stdin"));
    }
    Ok(value)
}

/// List the configured providers and what kind of credential each has; never any key text.
pub async fn list(
    _ctx: &super::GaitContext,
    _params: AuthListParams,
    _opts: CallOptions,
) -> anyhow::Result<Vec<AuthEntry>> {
    Ok(gait_ops::auth::list()
        .map_err(failed)?
        .into_iter()
        .map(|e| AuthEntry { provider: e.provider, kind: e.kind, status: e.status })
        .collect())
}

/// Show whether a provider has a credential and of what kind, without revealing it.
pub async fn status(
    _ctx: &super::GaitContext,
    params: ProviderId,
    _opts: CallOptions,
) -> anyhow::Result<AuthStatus> {
    let provider = params.id.as_raw();
    let status = gait_ops::auth::status(provider).map_err(failed)?;
    Ok(AuthStatus { provider: provider.to_owned(), status })
}

/// Store an API key for a provider. The key is read from the terminal prompt, or one line of stdin; it is never an argument.
pub async fn add(
    _ctx: &super::GaitContext,
    params: ProviderId,
    _opts: CallOptions,
) -> anyhow::Result<AuthSaved> {
    let provider = params.id.as_raw();
    if provider == SIGNIN {
        return Err(CliError::invalid_input(
            "uses ChatGPT sign-in; run gait auth login --id openai-signin",
        ));
    }
    if provider == CODEX {
        return Err(CliError::invalid_input(
            "openai-codex uses a ChatGPT subscription; run: gait auth login --id openai-codex",
        ));
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("{provider} API key: ");
    }
    let key = first_line(stdin.lock()).map_err(|e| CliError::failed(format!("cannot read the key: {e}")))?;
    let out = gait_ops::auth::add(provider, key).map_err(failed)?;
    Ok(AuthSaved { provider: out.provider, display: out.display, action: "saved".into() })
}

/// Use another tool's credentials for a provider without copying them.
pub async fn link(
    _ctx: &super::GaitContext,
    params: AuthLinkParams,
    _opts: CallOptions,
) -> anyhow::Result<AuthSaved> {
    let out = gait_ops::auth::link(params.id.as_raw(), &params.source, params.home).map_err(failed)?;
    Ok(AuthSaved { provider: out.provider, display: out.display, action: "linked".into() })
}

/// Drop the link to another tool's credentials; that tool's own credentials are not touched.
pub async fn unlink(
    _ctx: &super::GaitContext,
    params: ProviderId,
    _opts: CallOptions,
) -> anyhow::Result<AuthRemoved> {
    let out = gait_ops::auth::unlink(params.id.as_raw()).map_err(failed)?;
    Ok(AuthRemoved { provider: out.provider, action: "unlinked".into() })
}

/// Remove a stored API key. Removing a provider with no key still succeeds.
pub async fn remove(
    _ctx: &super::GaitContext,
    params: ProviderId,
    _opts: CallOptions,
) -> anyhow::Result<AuthRemoved> {
    let out = gait_ops::auth::remove(params.id.as_raw()).map_err(failed)?;
    Ok(AuthRemoved { provider: out.provider, action: "removed".into() })
}

fn saved(out: gait_ops::auth::AuthWriteOutcome) -> AuthLogin {
    AuthLogin {
        provider: out.provider,
        action: "saved".into(),
        display: Some(out.display),
        authorization_url: None,
        expires_in_seconds: None,
    }
}

/// Sign in to a subscription provider: a device code for `openai-codex`, a browser step for `openai-signin`.
pub async fn login(
    _ctx: &super::GaitContext,
    params: AuthLoginParams,
    opts: CallOptions,
) -> anyhow::Result<AuthLogin> {
    let provider = params.id.as_raw();
    if params.complete.is_some() && provider != SIGNIN {
        return Err(coded_error::into_anyhow(UsageError::InvalidField {
            name: "complete".into(),
            reason: "only valid with `auth login --id openai-signin`".into(),
        }));
    }
    if provider == SIGNIN {
        if let Some(arg) = params.complete.as_deref() {
            let callback = read_callback_arg(arg, std::io::stdin()).map_err(failed)?;
            return gait_ops::auth::login_complete(provider, callback).await.map(saved).map_err(failed);
        }
        let start = gait_ops::auth::login_begin(provider).await.map_err(failed)?;
        if std::io::stdin().is_terminal() {
            opts.notices.notice(start.authorization_url.clone());
            eprint!("Paste the URL you were redirected to: ");
            let callback = read_callback_arg("-", std::io::stdin()).map_err(failed)?;
            return gait_ops::auth::login_complete(provider, callback).await.map(saved).map_err(failed);
        }
        return Ok(AuthLogin {
            provider: start.provider,
            action: "pending".into(),
            display: None,
            authorization_url: Some(start.authorization_url),
            expires_in_seconds: Some(start.expires_in_seconds),
        });
    }
    if provider != CODEX {
        return Err(CliError::invalid_input(
            "subscription login is currently available only for openai-codex",
        ));
    }
    let notices = opts.notices.clone();
    gait_ops::auth::login_codex(|flow| match flow {
        gait_auth::LoginFlow::DeviceCode { verification_url, user_code, .. } => {
            notices.notice(format!("Open {verification_url} and enter code: {user_code}"));
            notices.notice("Waiting for approval...");
        }
        _ => unreachable!("Codex subscription login uses a device code"),
    })
    .await
    .map(saved)
    .map_err(failed)
}

/// Sign out of a subscription provider. API keys are removed with `auth remove`.
pub async fn logout(
    _ctx: &super::GaitContext,
    params: AuthLogoutParams,
    opts: CallOptions,
) -> anyhow::Result<AuthLogout> {
    let provider = params.id.as_raw();
    if params.forget && provider != SIGNIN {
        return Err(coded_error::into_anyhow(UsageError::InvalidField {
            name: "forget".into(),
            reason: "only valid with `auth logout --id openai-signin`".into(),
        }));
    }
    if provider == SIGNIN {
        let out = gait_ops::auth::logout_signin(params.forget).await.map_err(failed)?;
        if !out.revoked {
            opts.notices.notice("Disconnect the app in ChatGPT Settings.");
        }
        return Ok(AuthLogout { provider: out.provider, action: "logged_out".into(), revoked: Some(out.revoked) });
    }
    if provider != CODEX {
        return Err(CliError::invalid_input(format!(
            "subscription logout is currently available only for openai-codex; use `gait auth remove --id {provider}` for API keys"
        )));
    }
    let out = gait_ops::auth::logout_codex().await.map_err(failed)?;
    Ok(AuthLogout { provider: out.provider, action: "logged_out".into(), revoked: None })
}

/// List the models of the models.dev catalog, optionally filtered by `--filter`.
pub async fn models(
    _ctx: &super::GaitContext,
    params: ModelsParams,
    _opts: CallOptions,
) -> anyhow::Result<Vec<ModelItem>> {
    let out = gait_ops::auth::models(params.filter.as_deref())
        .await
        .map_err(|e| CliError::failed_with("Error fetching models", e))?;
    Ok(out
        .render_filtered()
        .into_iter()
        .map(|m| ModelItem {
            id: m.id.clone(),
            provider: m.provider.clone(),
            name: m.name.clone(),
            context_length: m.context_length,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_callback_arg_dash_reads_stdin_and_trims() {
        let result =
            read_callback_arg("-", std::io::Cursor::new(b"  https://callback.example/x  \nrest\n")).unwrap();
        assert_eq!(result, "https://callback.example/x");
    }

    #[test]
    fn read_callback_arg_literal_passthrough() {
        let value = "https://callback.example/x?code=1";
        assert_eq!(read_callback_arg(value, std::io::Cursor::new(b"ignored\n")).unwrap(), value);
    }

    #[test]
    fn read_callback_arg_dash_rejects_empty_line_and_eof() {
        assert!(read_callback_arg("-", std::io::Cursor::new(b"\n")).is_err());
        assert!(read_callback_arg("-", std::io::Cursor::new(b"")).is_err());
        assert!(read_callback_arg("-", std::io::Cursor::new(b"  \t\r\n")).is_err());
    }

    #[test]
    fn read_callback_arg_dash_handles_crlf_and_missing_newline() {
        assert_eq!(
            read_callback_arg("-", std::io::Cursor::new(b" https://callback\r\n")).unwrap(),
            "https://callback"
        );
        assert_eq!(
            read_callback_arg("-", std::io::Cursor::new(b"https://callback")).unwrap(),
            "https://callback"
        );
    }
}
