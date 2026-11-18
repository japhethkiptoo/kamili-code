//! `kamili` executable: parses arguments, builds the engine (composition root),
//! and runs the requested channel.
#![forbid(unsafe_code)]

#[cfg(feature = "anthropic")]
mod auth;
mod ci;
mod provider;
mod runs;
mod skills;
mod terminal;

use std::io::Write;
use std::path::PathBuf;
#[cfg(any(feature = "acp", feature = "desktop"))]
use std::sync::Arc;

use clap::{Parser, Subcommand};
use kamili::{
    BuildRequest, CancelToken, Channel, Engine, Error, GraphQuery, SpecOutcome, SpecRequest,
};

#[derive(Parser)]
#[command(
    name = "kamili",
    version,
    about = "Spec-driven builds: the spec is the contract."
)]
struct Cli {
    /// Project root (where .kamili.toml and the specs live).
    #[arg(long, global = true, default_value = ".")]
    project: PathBuf,

    /// Never prompt: questions get no answer, approvals follow --yes.
    #[arg(long, global = true)]
    non_interactive: bool,

    /// Approve every approval gate automatically.
    #[arg(long, short = 'y', global = true)]
    yes: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build a task against an approved feature spec.
    Build {
        /// Feature (spec name). Not needed with --resume.
        #[arg(required_unless_present = "resume")]
        feature: Option<String>,
        /// What to build. Not needed with --resume.
        #[arg(required_unless_present = "resume")]
        task: Option<String>,
        /// Build even if the spec is still a draft.
        #[arg(long)]
        force: bool,
        /// Run only the Planner and Documentor; write only the spec.
        #[arg(long)]
        docs_only: bool,
        /// Continue a saved run where it stopped: a run id from `kamili runs`,
        /// or no value for the most recent resumable run.
        #[arg(long, value_name = "RUN", num_args = 0..=1, default_missing_value = "last",
              conflicts_with_all = ["feature", "task", "force", "docs_only"])]
        resume: Option<String>,
    },
    /// Explore the code graph agents use to find their way around the project.
    Graph {
        #[command(subcommand)]
        action: Option<GraphAction>,
        /// Rank towards this text, as a task would ("add rate limiting to login").
        #[arg(long, global = true)]
        focus: Option<String>,
        /// How many files to show (top, dot).
        #[arg(long, global = true, default_value_t = 25)]
        limit: usize,
    },
    /// List, inspect or clean up build runs (.kamili/runs).
    Runs {
        #[command(subcommand)]
        action: Option<RunsAction>,
    },
    /// Write or revise a feature spec with the Analyst.
    Spec {
        feature: String,
        /// Continue the last session for this feature.
        #[arg(long)]
        resume: bool,
    },
    /// Check or install the skills each agent requires ([skills] in .kamili.toml).
    Skills {
        #[command(subcommand)]
        action: Option<SkillsAction>,
    },
    /// Serve the ACP channel over stdio (for editors such as Zed).
    #[cfg(feature = "acp")]
    Acp,
    /// Open the desktop app for the project: specs, builds, runs and
    /// change review in a native window.
    #[cfg(feature = "desktop")]
    Desktop,
    /// Log in to Claude. With no flags, asks how: browser sign-in or API key.
    #[cfg(feature = "anthropic")]
    Login {
        /// Sign in with your browser (Claude Console account, via the `ant` CLI).
        #[arg(long, visible_alias = "ant", conflicts_with_all = ["api_key", "api_key_stdin"])]
        browser: bool,
        /// With --browser: print the sign-in URL instead of opening a browser
        /// (for SSH sessions).
        #[arg(long, requires = "browser")]
        no_browser: bool,
        /// Paste an API key (skips the menu).
        #[arg(long, conflicts_with = "api_key_stdin")]
        api_key: bool,
        /// Read the API key from stdin instead of prompting.
        #[arg(long)]
        api_key_stdin: bool,
        /// Save the key without checking it against the API.
        #[arg(long, conflicts_with = "browser")]
        no_verify: bool,
    },
    /// Remove the API key saved by `kamili login`.
    #[cfg(feature = "anthropic")]
    Logout,
    /// Show which Claude credential is in use and check that it works.
    #[cfg(feature = "anthropic")]
    Whoami,
}

#[derive(Subcommand)]
enum GraphAction {
    /// The most important files and their key symbols (default).
    Top,
    /// The repo map an agent would get for --focus.
    Map {
        /// Size in bytes (default: [context] repo_map_bytes).
        #[arg(long)]
        bytes: Option<usize>,
    },
    /// Where a name is declared and used.
    Symbol { name: String },
    /// What a file uses, and what uses it.
    File { path: String },
    /// Files that use code defined in these files.
    Impact {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Graphviz DOT of the top files: `kamili graph dot | dot -Tsvg > graph.svg`.
    Dot,
}

#[derive(Subcommand)]
enum RunsAction {
    /// Runs, newest first, with state and token use (default).
    List,
    /// Print a run's full record.
    Show {
        /// Run id, or `last`.
        id: String,
    },
    /// Delete one run (finished or not) and its working copy.
    Discard {
        /// Run id, or `last` for the most recent resumable run.
        id: String,
    },
    /// Delete working copies of finished runs.
    Clean {
        /// Also delete the finished runs' records.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum SkillsAction {
    /// Show each required skill and whether it is installed (default).
    List,
    /// Install missing skills from skills.sh with `npx skills add`.
    Install,
}

#[tokio::main]
async fn main() {
    let code = match run(Cli::parse()).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            i32::from(e.exit_code())
        }
    };
    // Exit directly: a prompt cancelled by Ctrl+C may leave a thread blocked
    // reading stdin, which would otherwise hold up runtime shutdown.
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

/// First Ctrl+C cancels the build at its next safe point (the run stays
/// resumable); a second one quits immediately.
fn cancel_on_ctrl_c() -> CancelToken {
    let token = CancelToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\ncancelling at the next safe point… (Ctrl+C again to quit now)");
            t.cancel();
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        }
    });
    token
}

async fn run(cli: Cli) -> Result<(), Error> {
    // Account commands need no project or engine.
    #[cfg(feature = "anthropic")]
    match cli.command {
        Command::Login {
            browser,
            no_browser,
            api_key,
            api_key_stdin,
            no_verify,
        } => {
            let method = if browser {
                Some(auth::Method::Browser { open: !no_browser })
            } else if api_key_stdin {
                Some(auth::Method::ApiKeyStdin)
            } else if api_key {
                Some(auth::Method::ApiKey)
            } else {
                None
            };
            let args = auth::LoginArgs { method, no_verify };
            return auth::login(&auth::Store::default_location()?, args).await;
        }
        Command::Logout => return auth::logout(&auth::Store::default_location()?),
        Command::Whoami => {
            // The project's [provider] decides which credential matters.
            return provider::whoami(&kamili::load_config(&cli.project)?).await;
        }
        _ => {}
    }

    // ACP: the editor picks the project per session, so engines are built
    // per session. Nothing but protocol messages may go to stdout.
    #[cfg(feature = "acp")]
    if let Command::Acp = cli.command {
        return kamili_acp::serve(Arc::new(EngineForProject)).await;
    }

    let root = cli
        .project
        .canonicalize()
        .map_err(|e| Error::Config(format!("project {}: {e}", cli.project.display())))?;
    let config = kamili::load_config(&root)?;

    // Desktop: the window owns the main thread until it closes; engine jobs
    // run on this tokio runtime. Engines are built per job, so a missing
    // credential shows in the window rather than stopping it from opening.
    #[cfg(feature = "desktop")]
    if let Command::Desktop = cli.command {
        drop(config);
        return kamili_desktop::run(
            Arc::new(EngineForProject),
            root,
            tokio::runtime::Handle::current(),
        );
    }

    // Skills and runs commands need the project, but no model client.
    if let Command::Skills { action } = &cli.command {
        return match action {
            None | Some(SkillsAction::List) => skills::list(&config, &root),
            Some(SkillsAction::Install) => skills::install(&config, &root),
        };
    }
    if let Command::Graph {
        action,
        focus,
        limit,
    } = &cli.command
    {
        let (focus, limit) = (focus.clone(), *limit);
        let query = match action {
            None | Some(GraphAction::Top) => GraphQuery::Overview { focus, limit },
            Some(GraphAction::Map { bytes }) => GraphQuery::Map {
                focus,
                bytes: bytes.unwrap_or(config.context.repo_map_bytes),
            },
            Some(GraphAction::Symbol { name }) => GraphQuery::Symbol { name: name.clone() },
            Some(GraphAction::File { path }) => GraphQuery::File { path: path.clone() },
            Some(GraphAction::Impact { paths }) => GraphQuery::Impact {
                paths: paths.clone(),
            },
            Some(GraphAction::Dot) => GraphQuery::Dot { focus, limit },
        };
        print!("{}", kamili::explore_graph(&config, &root, &query)?);
        return Ok(());
    }
    if let Command::Runs { action } = &cli.command {
        return match action {
            None | Some(RunsAction::List) => runs::list(&root),
            Some(RunsAction::Show { id }) => runs::show(&root, id),
            Some(RunsAction::Clean { all }) => runs::clean(&root, *all),
            Some(RunsAction::Discard { id }) => runs::discard(&root, id),
        };
    }

    let llm = provider::model_client(&config).await?;
    let engine = Engine::new(config, llm)?;

    let channel: Box<dyn Channel> = if cli.non_interactive {
        Box::new(ci::CiChannel {
            approve_all: cli.yes,
        })
    } else {
        Box::new(terminal::TerminalChannel {
            auto_approve: cli.yes,
        })
    };

    match cli.command {
        Command::Build {
            feature,
            task,
            force,
            docs_only,
            resume,
        } => {
            let req = BuildRequest {
                project_root: root,
                feature: feature.unwrap_or_default(),
                task: task.unwrap_or_default(),
                force,
                docs_only,
                resume,
                cancel: Some(cancel_on_ctrl_c()),
            };
            let out = engine.build(req, channel.as_ref()).await?;
            println!(
                "done: {} file(s) changed{}, {} attempt(s) (run {})",
                out.files_changed.len(),
                if out.spec_changed {
                    ", spec updated"
                } else {
                    ""
                },
                out.attempts,
                out.run_id
            );
        }
        Command::Spec { feature, resume } => {
            let req = SpecRequest {
                project_root: root,
                feature: feature.clone(),
                resume,
            };
            match engine.spec_session(req, channel.as_ref()).await? {
                SpecOutcome::Approved { path } => println!("approved: {}", path.display()),
                SpecOutcome::Rejected => println!("draft rejected; nothing written"),
                SpecOutcome::Abandoned => {
                    println!("paused; continue with `kamili spec {feature} --resume`")
                }
                _ => {}
            }
        }
        #[cfg(feature = "acp")]
        Command::Acp => unreachable!("handled above"),
        #[cfg(feature = "desktop")]
        Command::Desktop => unreachable!("handled above"),
        Command::Skills { .. } | Command::Runs { .. } | Command::Graph { .. } => {
            unreachable!("handled above")
        }
        #[cfg(feature = "anthropic")]
        Command::Login { .. } | Command::Logout | Command::Whoami => unreachable!("handled above"),
    }
    Ok(())
}

/// Builds an engine for a project (an editor session or a desktop job): its
/// `.kamili.toml`, and the model client its `[provider]` asks for.
#[cfg(any(feature = "acp", feature = "desktop"))]
struct EngineForProject;

#[cfg(any(feature = "acp", feature = "desktop"))]
impl EngineForProject {
    async fn build(project_root: &std::path::Path) -> Result<Engine, Error> {
        let config = kamili::load_config(project_root)?;
        let llm = provider::model_client(&config).await?;
        Engine::new(config, llm)
    }
}

#[cfg(feature = "acp")]
#[async_trait::async_trait]
impl kamili_acp::EngineFactory for EngineForProject {
    async fn engine(&self, project_root: &std::path::Path) -> Result<Engine, Error> {
        Self::build(project_root).await
    }
}

#[cfg(feature = "desktop")]
#[async_trait::async_trait]
impl kamili_desktop::EngineFactory for EngineForProject {
    async fn engine(&self, project_root: &std::path::Path) -> Result<Engine, Error> {
        Self::build(project_root).await
    }
}
