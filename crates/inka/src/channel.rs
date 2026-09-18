// Release-channel identity and resolution for the CLI.
//
// The installed toolchain's channel is the source of truth for default
// selection: a released stable toolchain defaults to stable, a released
// prerelease (`-beta.N`/`-rc.N`) defaults to beta, and an unbaked dev build
// defaults to stable (but may opt into beta). Channel resolution is, highest to
// lowest: an explicit flag (`--beta`/`--stable`) -> `INKA_CHANNEL` -> the
// toolchain channel.
//
// `rc` prereleases map to the beta channel.

use std::env;

use crate::ui;

/// A release channel. `Dev` is an unbaked build (no `INKA_BUILD_VERSION`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Channel {
    Stable,
    Beta,
    Dev,
}

impl Channel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Dev => "dev",
        }
    }
}

/// Why a channel request could not be satisfied.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// Beta was requested on a stable release (not a backdoor).
    BetaOnStable,
    /// `INKA_CHANNEL` named an unknown channel.
    InvalidEnv(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::BetaOnStable => {
                write!(f, "the beta channel is not available on a stable release")
            }
            Error::InvalidEnv(m) => write!(f, "{m}"),
        }
    }
}

/// The hint shown when beta is requested on a stable release.
pub(crate) const BETA_HINT: &str = "run `inka update --beta` to switch to the beta channel";

/// The baked release version, else the crate version for a dev build.
pub(crate) fn release_version() -> &'static str {
    option_env!("INKA_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// The baked short commit hash, when the release pipeline provided one.
pub(crate) fn build_commit() -> Option<&'static str> {
    option_env!("INKA_BUILD_COMMIT")
}

/// Classify a version string: a `-beta.`/`-rc.` prerelease is beta; anything
/// else (including an unbaked fallback) is stable.
fn channel_of_version(v: &str) -> Channel {
    if v.contains("-beta.") || v.contains("-rc.") {
        Channel::Beta
    } else {
        Channel::Stable
    }
}

/// This toolchain's channel. An unbaked build is `Dev`.
pub(crate) fn toolchain_channel() -> Channel {
    match option_env!("INKA_BUILD_VERSION") {
        Some(v) => channel_of_version(v),
        None => Channel::Dev,
    }
}

/// True when this build can opt into the beta channel (`--beta`): a beta
/// release or a dev build.
pub(crate) fn is_beta_capable() -> bool {
    matches!(toolchain_channel(), Channel::Beta | Channel::Dev)
}

/// Parse `INKA_CHANNEL` strictly. Empty/unset is `None`; `stable`/`beta`
/// (case-insensitive) parse; any other value is a hard error.
pub(crate) fn env_channel() -> Result<Option<Channel>, String> {
    match env::var("INKA_CHANNEL") {
        Ok(v) => {
            let v = v.trim();
            if v.is_empty() {
                return Ok(None);
            }
            match v.to_ascii_lowercase().as_str() {
                "stable" => Ok(Some(Channel::Stable)),
                "beta" => Ok(Some(Channel::Beta)),
                other => Err(format!(
                    "invalid INKA_CHANNEL '{other}' (expected \"stable\" or \"beta\")"
                )),
            }
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err("INKA_CHANNEL is not valid UTF-8".into()),
    }
}

/// Resolve the `--beta`/`--stable` flags into a requested channel, rejecting a
/// contradictory pair.
pub(crate) fn flag_request(beta: bool, stable: bool) -> Result<Option<Channel>, String> {
    match (beta, stable) {
        (true, true) => Err("--beta and --stable are mutually exclusive".into()),
        (true, false) => Ok(Some(Channel::Beta)),
        (false, true) => Ok(Some(Channel::Stable)),
        (false, false) => Ok(None),
    }
}

/// Resolve the effective channel for `build`/`run`/`doctor`: the explicit flag
/// wins, then the environment, then the toolchain default. Requesting beta on a
/// stable release is refused (it is not a backdoor; `inka update --beta` is the
/// channel switcher). Returns `Stable` or `Beta`, never `Dev`.
pub(crate) fn resolve(requested: Option<Channel>) -> Result<Channel, Error> {
    let env = env_channel().map_err(Error::InvalidEnv)?;
    resolve_with(toolchain_channel(), requested, env)
}

/// `resolve` with the toolchain channel and environment injected (testable).
fn resolve_with(
    toolchain: Channel,
    requested: Option<Channel>,
    env: Option<Channel>,
) -> Result<Channel, Error> {
    match requested.or(env) {
        Some(Channel::Beta) if toolchain == Channel::Stable => Err(Error::BetaOnStable),
        Some(c) => Ok(c),
        None => Ok(match toolchain {
            Channel::Beta => Channel::Beta,
            _ => Channel::Stable,
        }),
    }
}

/// `resolve`, reporting a hard error and exiting 2 on failure.
pub(crate) fn resolve_or_exit(requested: Option<Channel>) -> Channel {
    match resolve(requested) {
        Ok(c) => c,
        Err(Error::BetaOnStable) => {
            // Only a stable release rejects beta; a beta/dev build is capable.
            debug_assert!(!is_beta_capable());
            ui::log_error(Error::BetaOnStable.to_string());
            ui::hint(BETA_HINT);
            std::process::exit(2);
        }
        Err(Error::InvalidEnv(msg)) => {
            ui::log_error(msg);
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_classification() {
        assert_eq!(channel_of_version("0.8.1"), Channel::Stable);
        assert_eq!(channel_of_version("0.8.1-beta.2"), Channel::Beta);
        assert_eq!(channel_of_version("0.8.1-rc.10"), Channel::Beta);
    }

    #[test]
    fn resolve_flag_wins_then_env_then_toolchain() {
        // Explicit flag beats env and toolchain.
        assert_eq!(
            resolve_with(Channel::Beta, Some(Channel::Stable), Some(Channel::Beta)).unwrap(),
            Channel::Stable
        );
        // Env beats toolchain.
        assert_eq!(
            resolve_with(Channel::Dev, None, Some(Channel::Beta)).unwrap(),
            Channel::Beta
        );
        // Toolchain default.
        assert_eq!(
            resolve_with(Channel::Beta, None, None).unwrap(),
            Channel::Beta
        );
        assert_eq!(
            resolve_with(Channel::Dev, None, None).unwrap(),
            Channel::Stable
        );
        assert_eq!(
            resolve_with(Channel::Stable, None, None).unwrap(),
            Channel::Stable
        );
    }

    #[test]
    fn stable_release_refuses_beta() {
        // `--beta` on a stable release.
        assert_eq!(
            resolve_with(Channel::Stable, Some(Channel::Beta), None).unwrap_err(),
            Error::BetaOnStable
        );
        // `INKA_CHANNEL=beta` on a stable release is not a backdoor.
        assert_eq!(
            resolve_with(Channel::Stable, None, Some(Channel::Beta)).unwrap_err(),
            Error::BetaOnStable
        );
        // `--stable`/env stable are fine.
        assert_eq!(
            resolve_with(Channel::Stable, Some(Channel::Stable), None).unwrap(),
            Channel::Stable
        );
    }

    #[test]
    fn beta_and_dev_allow_beta() {
        assert!(resolve_with(Channel::Beta, Some(Channel::Beta), None).is_ok());
        assert!(resolve_with(Channel::Dev, Some(Channel::Beta), None).is_ok());
        assert_eq!(
            resolve_with(Channel::Beta, Some(Channel::Stable), None).unwrap(),
            Channel::Stable
        );
    }

    #[test]
    fn current_build_is_dev_in_tests() {
        // Tests run unbaked, so the toolchain is a dev build with a stable
        // default and beta capability.
        assert_eq!(toolchain_channel(), Channel::Dev);
        assert!(is_beta_capable());
        assert_eq!(
            resolve_with(toolchain_channel(), None, None).unwrap(),
            Channel::Stable
        );
    }

    #[test]
    fn invalid_env_is_reported() {
        assert_eq!(
            resolve_with(Channel::Dev, None, None).unwrap(),
            Channel::Stable,
            "sanity"
        );
        let err = Error::InvalidEnv("invalid INKA_CHANNEL 'x'".to_string());
        assert!(err.to_string().contains("invalid INKA_CHANNEL"));
    }
}
