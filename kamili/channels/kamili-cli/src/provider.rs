//! Picks the model client from `[provider]` in `.kamili.toml`, for every
//! channel the CLI runs (terminal, CI and ACP).
//!
//! - `anthropic` (default): Claude credentials from `kamili login` (see `auth`).
//! - `openai`: any OpenAI-compatible API (NVIDIA NIM, OpenRouter, Ollama, …),
//!   with the key read from the environment variable named in `api_key_env`.
//!   No Anthropic credential is needed.

// Slim builds compile some helpers without using them.
#![cfg_attr(not(all(feature = "anthropic", feature = "openai")), allow(dead_code))]

use std::sync::Arc;

use kamili::{Config, Error, Llm, LlmError, ProviderKind};

pub async fn model_client(config: &Config) -> Result<Arc<dyn Llm>, Error> {
    match config.provider.kind {
        ProviderKind::Anthropic => anthropic_client(config).await,
        ProviderKind::Openai => openai_client(config, &|k| std::env::var(k).ok()),
    }
}

#[cfg(feature = "anthropic")]
async fn anthropic_client(config: &Config) -> Result<Arc<dyn Llm>, Error> {
    let llm = crate::auth::model_client(&crate::auth::Store::default_location()?).await?;
    Ok(Arc::new(llm.with_base_url(&config.provider.base_url)))
}

#[cfg(not(feature = "anthropic"))]
async fn anthropic_client(_: &Config) -> Result<Arc<dyn Llm>, Error> {
    Err(Error::Unsupported(
        "built without the `anthropic` feature; set [provider] kind = \"openai\"".into(),
    ))
}

#[cfg(feature = "openai")]
fn openai_client(
    config: &Config,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Arc<dyn Llm>, Error> {
    Ok(Arc::new(openai_llm(config, env)?))
}

#[cfg(not(feature = "openai"))]
fn openai_client(_: &Config, _: &dyn Fn(&str) -> Option<String>) -> Result<Arc<dyn Llm>, Error> {
    Err(Error::Unsupported(
        "built without the `openai` feature; no OpenAI-compatible client".into(),
    ))
}

#[cfg(feature = "openai")]
fn openai_llm(
    config: &Config,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<kamili::OpenAiCompatLlm, Error> {
    let p = &config.provider;
    if p.base_url.trim().is_empty() {
        return Err(Error::Config(
            "[provider] kind = \"openai\" needs base_url, e.g. \"https://integrate.api.nvidia.com/v1\""
                .into(),
        ));
    }
    check_models(config)?;
    Ok(kamili::OpenAiCompatLlm::new(
        &p.base_url,
        api_key(config, env)?,
    )?)
}

/// The key from `api_key_env`; `None` when no variable is configured.
fn api_key(config: &Config, env: &dyn Fn(&str) -> Option<String>) -> Result<Option<String>, Error> {
    let var = config.provider.api_key_env.trim();
    if var.is_empty() {
        return Ok(None);
    }
    match env(var).filter(|v| !v.trim().is_empty()) {
        Some(key) => Ok(Some(key.trim().to_string())),
        None => Err(LlmError::Auth(format!(
            "{var} is not set; export it with your provider's API key \
             (for editors, add it to the agent server's `env`)"
        ))
        .into()),
    }
}

/// The defaults are Claude model IDs, which an OpenAI-compatible provider
/// will not know. Catch that before the first call rather than mid-run.
fn check_models(config: &Config) -> Result<(), Error> {
    let m = &config.models;
    let roles = [
        ("analyst", &m.analyst),
        ("architect", &m.architect),
        ("planner", &m.planner),
        ("worker", &m.worker),
        ("tester", &m.tester),
        ("documentor", &m.documentor),
    ];
    let claude: Vec<String> = roles
        .iter()
        .filter(|(_, id)| id.starts_with("claude-"))
        .map(|(role, id)| format!("{role} = \"{id}\""))
        .collect();
    if claude.is_empty() {
        return Ok(());
    }
    Err(Error::Config(format!(
        "[provider] kind = \"openai\", but [models] still has Claude model IDs ({}); \
         set every role in [models] to a model ID from your provider",
        claude.join(", ")
    )))
}

/// `sk-ant-…wxyz`: never more than a known prefix and the last four characters.
pub fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() < 12 {
        return "…".into();
    }
    let prefix = ["sk-ant-", "nvapi-", "sk-or-", "sk-"]
        .into_iter()
        .find(|p| secret.starts_with(p))
        .unwrap_or("");
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}…{tail}")
}

/// `kamili whoami`: the provider this project uses, its credential, and a
/// token-free check that it works.
pub async fn whoami(config: &Config) -> Result<(), Error> {
    match config.provider.kind {
        ProviderKind::Anthropic => {
            if !config.provider.base_url.is_empty() {
                println!("provider: anthropic ({})", config.provider.base_url);
            }
            whoami_anthropic(config).await
        }
        ProviderKind::Openai => whoami_openai(config).await,
    }
}

#[cfg(feature = "anthropic")]
async fn whoami_anthropic(config: &Config) -> Result<(), Error> {
    crate::auth::whoami(
        &crate::auth::Store::default_location()?,
        &config.provider.base_url,
    )
    .await
}

#[cfg(not(feature = "anthropic"))]
async fn whoami_anthropic(config: &Config) -> Result<(), Error> {
    anthropic_client(config).await.map(|_| ())
}

#[cfg(feature = "openai")]
async fn whoami_openai(config: &Config) -> Result<(), Error> {
    let env = |k: &str| std::env::var(k).ok();
    let llm = openai_llm(config, &env)?;
    println!("provider: openai-compatible ({})", llm.base_url());
    match api_key(config, &env)? {
        Some(key) => println!(
            "credential: {} environment variable ({})",
            config.provider.api_key_env.trim(),
            mask(&key)
        ),
        None => println!("credential: none (no api_key_env set)"),
    }
    eprint!("verifying… ");
    llm.verify().await?;
    eprintln!("ok");
    Ok(())
}

#[cfg(not(feature = "openai"))]
async fn whoami_openai(config: &Config) -> Result<(), Error> {
    openai_client(config, &|_| None).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nvidia(models: &str) -> Config {
        toml::from_str(&format!(
            "[provider]\nkind = \"openai\"\nbase_url = \"https://integrate.api.nvidia.com/v1\"\n\
             api_key_env = \"NVIDIA_API_KEY\"\n{models}"
        ))
        .unwrap()
    }

    const OPEN_MODELS: &str = "[models]\nanalyst = \"a\"\narchitect = \"a\"\nplanner = \"a\"\n\
                               worker = \"w\"\ntester = \"w\"\ndocumentor = \"d\"\n";

    #[cfg(feature = "openai")]
    #[test]
    fn openai_needs_no_anthropic_credential() {
        // Only the provider's own key is looked up.
        let env = |k: &str| (k == "NVIDIA_API_KEY").then(|| "nvapi-test-key-1234".to_string());
        assert!(openai_client(&nvidia(OPEN_MODELS), &env).is_ok());
    }

    #[test]
    fn missing_key_is_an_auth_error() {
        // Auth errors become ACP's auth-required error in the editor.
        let err = api_key(&nvidia(OPEN_MODELS), &|_| None).unwrap_err();
        assert!(matches!(err, Error::Llm(ref m) if m.contains("NVIDIA_API_KEY")));
        let blank = |_: &str| Some("  ".to_string());
        assert!(api_key(&nvidia(OPEN_MODELS), &blank).is_err());
    }

    #[test]
    fn no_key_env_means_no_key() {
        let mut cfg = nvidia(OPEN_MODELS);
        cfg.provider.api_key_env.clear();
        assert_eq!(api_key(&cfg, &|_| None).unwrap(), None);
    }

    #[test]
    fn claude_model_ids_are_rejected_for_openai() {
        let err = check_models(&nvidia("")).unwrap_err().to_string();
        assert!(err.contains("analyst = \"claude-opus-5-5\""), "{err}");
        assert!(check_models(&nvidia(OPEN_MODELS)).is_ok());
    }

    #[cfg(feature = "openai")]
    #[test]
    fn openai_needs_base_url() {
        let mut cfg = nvidia(OPEN_MODELS);
        cfg.provider.base_url.clear();
        let env = |_: &str| Some("k".to_string());
        assert!(matches!(openai_client(&cfg, &env), Err(Error::Config(_))));
    }

    #[test]
    fn mask_hides_all_but_last_four() {
        assert_eq!(mask("sk-ant-api03-abcdefghWXYZ"), "sk-ant-…WXYZ");
        assert_eq!(mask("nvapi-abcdefghijklWXYZ"), "nvapi-…WXYZ");
        assert_eq!(mask("short"), "…");
        assert_eq!(mask("oauth-token-123456789"), "…6789");
    }
}
