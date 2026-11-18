//! Claude credentials for the CLI: `kamili login`, `logout`, `whoami`, and the
//! resolution used when building the engine.
//!
//! Resolution order (first match wins), matching Anthropic's own tools:
//!
//! 1. `ANTHROPIC_API_KEY`
//! 2. `ANTHROPIC_AUTH_TOKEN` (OAuth bearer token)
//! 3. The key saved by `kamili login`
//! 4. The active `ant` CLI profile (browser sign-in: `kamili login --browser`)
//!
//! Empty environment variables are treated as unset.

use std::fmt;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kamili::{AnthropicLlm, Credential, CredentialSource, Error, LlmError};
use serde::{Deserialize, Serialize};

use crate::provider::mask;

const CREDENTIALS_FILE: &str = "credentials.toml";
/// Re-ask `ant` for a token after this long; it refreshes expired ones itself.
const ANT_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    EnvApiKey,
    EnvAuthToken,
    Stored,
    AntProfile,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::EnvApiKey => "ANTHROPIC_API_KEY environment variable",
            Source::EnvAuthToken => "ANTHROPIC_AUTH_TOKEN environment variable",
            Source::Stored => "key saved by `kamili login`",
            Source::AntProfile => "browser sign-in (`ant` CLI profile)",
        })
    }
}

/// The credential the CLI will use, and where it came from.
pub struct Resolved {
    pub source: Source,
    pub credentials: Arc<dyn CredentialSource>,
    /// Masked form for display.
    pub hint: String,
}

// ---------------------------------------------------------------------------
// Stored key
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct StoredCredentials {
    api_key: String,
}

/// User-level credential store, outside any project tree.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// `$KAMILI_CONFIG_DIR`, else `$XDG_CONFIG_HOME/kamili`, else
    /// `~/.config/kamili` (`%APPDATA%\kamili` on Windows).
    pub fn default_location() -> Result<Self, Error> {
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let dir = var("KAMILI_CONFIG_DIR")
            .or_else(|| var("XDG_CONFIG_HOME").map(|d| d.join("kamili")))
            .or_else(|| {
                if cfg!(windows) {
                    var("APPDATA").map(|d| d.join("kamili"))
                } else {
                    var("HOME").map(|d| d.join(".config").join("kamili"))
                }
            })
            .ok_or_else(|| {
                Error::Config("cannot locate a config directory; set KAMILI_CONFIG_DIR".into())
            })?;
        Ok(Self { dir })
    }

    #[cfg(test)]
    pub fn at(dir: &std::path::Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(CREDENTIALS_FILE)
    }

    pub fn load(&self) -> Result<Option<String>, Error> {
        let path = self.path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Config(format!("{}: {e}", path.display()))),
        };
        let stored: StoredCredentials =
            toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        Ok(Some(stored.api_key).filter(|k| !k.is_empty()))
    }

    /// Saves the key with owner-only permissions (0600 file, 0700 dir on Unix).
    pub fn save(&self, api_key: &str) -> Result<PathBuf, Error> {
        let io = |e: std::io::Error| Error::Config(format!("{}: {e}", self.dir.display()));
        fs::create_dir_all(&self.dir).map_err(io)?;
        let text = toml::to_string(&StoredCredentials {
            api_key: api_key.to_string(),
        })
        .map_err(|e| Error::Config(e.to_string()))?;
        let path = self.path();
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(0o600);
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        let mut file = options.open(&path).map_err(io)?;
        // `mode` only applies on creation; tighten a pre-existing file too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(io)?;
        }
        file.write_all(text.as_bytes()).map_err(io)?;
        Ok(path)
    }

    /// Returns whether a stored key existed.
    pub fn remove(&self) -> Result<bool, Error> {
        match fs::remove_file(self.path()) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Config(format!("{}: {e}", self.path().display()))),
        }
    }
}

// ---------------------------------------------------------------------------
// `ant` CLI profile
// ---------------------------------------------------------------------------

/// OAuth token from the active `ant` profile, cached briefly.
#[derive(Default)]
struct AntProfile {
    cached: Mutex<Option<(String, Instant)>>,
}

#[async_trait]
impl CredentialSource for AntProfile {
    async fn credential(&self) -> Result<Credential, LlmError> {
        if let Some((token, at)) = self.cached.lock().expect("ant cache").clone()
            && at.elapsed() < ANT_TOKEN_TTL
        {
            return Ok(Credential::OAuthToken(token));
        }
        let token = ant_access_token().await?;
        *self.cached.lock().expect("ant cache") = Some((token.clone(), Instant::now()));
        Ok(Credential::OAuthToken(token))
    }
}

/// `ant auth print-credentials --access-token`: prints the bare token of the
/// active profile, refreshing it if needed.
async fn ant_access_token() -> Result<String, LlmError> {
    let out = tokio::process::Command::new("ant")
        .args(["auth", "print-credentials", "--access-token"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| LlmError::Auth(format!("cannot run `ant`: {e}")))?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || token.is_empty() {
        return Err(LlmError::Auth(
            "no active `ant` profile; run `kamili login --browser`".into(),
        ));
    }
    Ok(token)
}

fn ant_installed() -> bool {
    std::process::Command::new("ant")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Sources 1–3: environment and stored key. Pure, for testing.
fn pick(
    env: impl Fn(&str) -> Option<String>,
    stored: Option<String>,
) -> Option<(Source, Credential)> {
    let env = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(key) = env("ANTHROPIC_API_KEY") {
        return Some((Source::EnvApiKey, Credential::ApiKey(key)));
    }
    if let Some(token) = env("ANTHROPIC_AUTH_TOKEN") {
        return Some((Source::EnvAuthToken, Credential::OAuthToken(token)));
    }
    stored.map(|key| (Source::Stored, Credential::ApiKey(key)))
}

pub async fn resolve(store: &Store) -> Result<Resolved, Error> {
    if let Some((source, credential)) = pick(|k| std::env::var(k).ok(), store.load()?) {
        let hint = match &credential {
            Credential::ApiKey(k) | Credential::OAuthToken(k) => mask(k),
        };
        return Ok(Resolved {
            source,
            credentials: Arc::new(credential),
            hint,
        });
    }
    if ant_installed() {
        let profile = AntProfile::default();
        if profile.credential().await.is_ok() {
            return Ok(Resolved {
                source: Source::AntProfile,
                credentials: Arc::new(profile),
                hint: "active profile".into(),
            });
        }
    }
    Err(
        LlmError::Auth("not logged in; run `kamili login` (or set ANTHROPIC_API_KEY)".into())
            .into(),
    )
}

pub async fn model_client(store: &Store) -> Result<AnthropicLlm, Error> {
    Ok(AnthropicLlm::new(resolve(store).await?.credentials)?)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Where Console API keys are created.
const CONSOLE_KEYS_URL: &str = "https://platform.claude.com/settings/keys";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `ant auth login`: OAuth sign-in to a Claude Console account.
    Browser {
        open: bool,
    },
    /// Prompt for a key (hidden input).
    ApiKey,
    /// Open the Console's key page, then prompt for the new key.
    CreateKey,
    ApiKeyStdin,
}

pub struct LoginArgs {
    /// `None`: ask (or fail if stdin is not a terminal).
    pub method: Option<Method>,
    pub no_verify: bool,
}

pub async fn login(store: &Store, args: LoginArgs) -> Result<(), Error> {
    let method = match args.method {
        Some(m) => m,
        None => choose_method()?,
    };
    match method {
        Method::Browser { open } => login_with_browser(store, open).await,
        Method::ApiKey => login_with_key(store, false, args.no_verify).await,
        Method::ApiKeyStdin => login_with_key(store, true, args.no_verify).await,
        Method::CreateKey => {
            eprintln!("Create a key in the Console, then paste it here.");
            open_url(CONSOLE_KEYS_URL);
            login_with_key(store, false, args.no_verify).await
        }
    }
}

/// Interactive menu. Browser sign-in is the default when `ant` is installed.
fn choose_method() -> Result<Method, Error> {
    if !std::io::stdin().is_terminal() {
        return Err(Error::Config(
            "stdin is not a terminal; use `kamili login --browser`, or pipe a key with `--api-key-stdin`"
                .into(),
        ));
    }
    let ant = ant_installed();
    let browser_note = if ant { "" } else { " (needs the `ant` CLI)" };
    let default = if ant { 1 } else { 3 };
    eprintln!("How do you want to log in to Claude?");
    eprintln!("  1) Sign in with your browser (Claude Console account){browser_note}");
    eprintln!("  2) Paste an existing API key");
    eprintln!("  3) Create an API key in your browser, then paste it");
    loop {
        eprint!("Choice [{default}]: ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        let n = std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| Error::Config(format!("reading choice: {e}")))?;
        if n == 0 {
            return Err(Error::Config("login cancelled".into()));
        }
        match parse_choice(&line, default) {
            Some(m) => return Ok(m),
            None => eprintln!("enter 1, 2 or 3"),
        }
    }
}

fn parse_choice(line: &str, default: u8) -> Option<Method> {
    let choice = match line.trim() {
        "" => default,
        s => s.parse().ok()?,
    };
    match choice {
        1 => Some(Method::Browser { open: true }),
        2 => Some(Method::ApiKey),
        3 => Some(Method::CreateKey),
        _ => None,
    }
}

/// Best effort: always prints the URL, then tries the platform's opener.
fn open_url(url: &str) {
    eprintln!("Opening {url}");
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    let opened = cmd
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !opened {
        eprintln!("(could not open a browser; open the link yourself)");
    }
}

async fn login_with_key(store: &Store, from_stdin: bool, no_verify: bool) -> Result<(), Error> {
    let key = read_api_key(from_stdin)?;
    if !key.starts_with("sk-ant-") {
        eprintln!("warning: this does not look like an Anthropic API key (expected `sk-ant-…`)");
    }
    if no_verify {
        eprintln!("skipping verification (--no-verify)");
    } else {
        eprint!("verifying key… ");
        AnthropicLlm::with_api_key(key.clone())?.verify().await?;
        eprintln!("ok");
    }
    let path = store.save(&key)?;
    println!(
        "logged in with {} (saved to {})",
        mask(&key),
        path.display()
    );
    warn_if_env_overrides();
    Ok(())
}

/// Browser sign-in to a Claude Console account. Delegates to Anthropic's `ant`
/// CLI, which runs the OAuth flow and keeps (and refreshes) the token in its
/// own store; Kamili never sees or saves it.
async fn login_with_browser(store: &Store, open: bool) -> Result<(), Error> {
    if !ant_installed() {
        return Err(Error::Config(
            "browser sign-in uses Anthropic's `ant` CLI, which is not installed.\n\
             Install it:\n  \
               macOS:   brew install anthropics/tap/ant\n  \
               with Go: go install github.com/anthropics/anthropic-cli/v2/cmd/ant@latest\n\
             then run `kamili login --browser` again, or use `kamili login --api-key`"
                .into(),
        ));
    }
    let mut args = vec!["auth", "login"];
    if !open {
        args.push("--no-browser");
    }
    eprintln!("Starting browser sign-in (`ant {}`)…", args.join(" "));
    let status = std::process::Command::new("ant")
        .args(&args)
        .status()
        .map_err(|e| Error::Config(format!("cannot run `ant auth login`: {e}")))?;
    if !status.success() {
        return Err(LlmError::Auth(
            "sign-in did not complete; on a machine without a browser, try `kamili login --browser --no-browser`".into(),
        )
        .into());
    }
    eprint!("verifying sign-in… ");
    AnthropicLlm::new(Arc::new(AntProfile::default()))?
        .verify()
        .await?;
    eprintln!("ok");
    println!("logged in with your Claude Console account (browser sign-in)");
    if store.load()?.is_some() {
        if confirm("A saved API key takes precedence over the sign-in. Remove it?")? {
            store.remove()?;
            eprintln!("removed saved key");
        } else {
            eprintln!("note: the saved key stays in use; run `kamili logout` to use the sign-in");
        }
    }
    warn_if_env_overrides();
    Ok(())
}

/// y/N prompt; "no" when stdin is not a terminal.
fn confirm(question: &str) -> Result<bool, Error> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| Error::Config(format!("reading answer: {e}")))?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

fn read_api_key(from_stdin: bool) -> Result<String, Error> {
    let key = if from_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| Error::Config(format!("reading stdin: {e}")))?;
        buf
    } else if std::io::stdin().is_terminal() {
        eprintln!("Paste an API key from the Claude Console (input is hidden).");
        rpassword::prompt_password("Anthropic API key: ")
            .map_err(|e| Error::Config(format!("reading key: {e}")))?
    } else {
        return Err(Error::Config(
            "stdin is not a terminal; pipe the key with `kamili login --api-key-stdin`".into(),
        ));
    };
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(Error::Config("no API key given".into()));
    }
    Ok(key)
}

pub fn logout(store: &Store) -> Result<(), Error> {
    if store.remove()? {
        println!("removed saved key ({})", store.path().display());
    } else {
        println!("no saved key");
    }
    if ant_installed() {
        eprintln!(
            "note: an `ant` profile, if any, is untouched; run `ant auth logout` to remove it"
        );
    }
    warn_if_env_overrides();
    Ok(())
}

/// `base_url`: the project's `[provider] base_url` (empty = Anthropic's API).
pub async fn whoami(store: &Store, base_url: &str) -> Result<(), Error> {
    let resolved = resolve(store).await?;
    println!("credential: {} ({})", resolved.source, resolved.hint);
    eprint!("verifying… ");
    AnthropicLlm::new(resolved.credentials)?
        .with_base_url(base_url)
        .verify()
        .await?;
    eprintln!("ok");
    Ok(())
}

fn warn_if_env_overrides() {
    for var in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        if std::env::var(var).is_ok_and(|v| !v.is_empty()) {
            eprintln!("note: {var} is set and takes precedence over saved credentials");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn precedence_env_key_then_token_then_stored() {
        let stored = Some("sk-ant-stored".to_string());
        let all = [
            ("ANTHROPIC_API_KEY", "sk-ant-env"),
            ("ANTHROPIC_AUTH_TOKEN", "tok"),
        ];
        assert_eq!(
            pick(env(&all), stored.clone()).unwrap().0,
            Source::EnvApiKey
        );
        assert_eq!(
            pick(env(&all[1..]), stored.clone()).unwrap().0,
            Source::EnvAuthToken
        );
        assert_eq!(pick(env(&[]), stored).unwrap().0, Source::Stored);
        assert!(pick(env(&[]), None).is_none());
    }

    #[test]
    fn empty_env_vars_are_ignored() {
        let vars = [("ANTHROPIC_API_KEY", ""), ("ANTHROPIC_AUTH_TOKEN", "")];
        let (source, cred) = pick(env(&vars), Some("sk-ant-stored".into())).unwrap();
        assert_eq!(source, Source::Stored);
        assert_eq!(cred, Credential::ApiKey("sk-ant-stored".into()));
    }

    #[test]
    fn store_round_trip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(&dir.path().join("kamili"));
        assert_eq!(store.load().unwrap(), None);

        let path = store.save("sk-ant-api03-abcdefgh").unwrap();
        assert_eq!(
            store.load().unwrap().as_deref(),
            Some("sk-ant-api03-abcdefgh")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let dir_mode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }

        assert!(store.remove().unwrap());
        assert!(!store.remove().unwrap());
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn menu_choice_parsing() {
        assert_eq!(parse_choice("\n", 1), Some(Method::Browser { open: true }));
        assert_eq!(parse_choice("", 3), Some(Method::CreateKey));
        assert_eq!(parse_choice(" 2 \n", 1), Some(Method::ApiKey));
        assert_eq!(parse_choice("1", 3), Some(Method::Browser { open: true }));
        assert_eq!(parse_choice("4", 1), None);
        assert_eq!(parse_choice("browser", 1), None);
    }
}
