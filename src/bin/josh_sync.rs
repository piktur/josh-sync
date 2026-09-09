use anyhow::Context;
use clap::Parser;
use josh_sync::SyncContext;
use josh_sync::config::{JoshConfig, load_config};
use josh_sync::josh::{JoshProxy, try_install_josh_proxy};
use josh_sync::sync::{DEFAULT_UPSTREAM_REPO, FilterVersion, GitSync, PushResult, RustcPullError};
use josh_sync::utils::{get_current_head_sha, is_inside_ci, prompt};
use std::path::{Path, PathBuf};

const DEFAULT_CONFIG_PATH: &str = "josh-sync.toml";

#[derive(clap::Parser)]
struct Args {
    #[clap(subcommand)]
    cmd: Command,
}

#[derive(clap::Parser)]
enum Command {
    /// Initialize a config file for this repository.
    Init,
    /// Pull changes from the configured upstream repository.
    /// This creates new commits that should be then merged into this subtree repository.
    Pull {
        /// Override the upstream repository from which we pull changes.
        /// Can be used to test changes that have not yet reached the configured upstream.
        #[clap(long)]
        upstream_repo: Option<String>,

        /// Override the upstream commit to pull instead of the configured branch.
        #[clap(long)]
        upstream_commit: Option<String>,

        /// By default, the `pull` command will exit with status code 2 if there is nothing to pull,
        /// and reset git to the original state.
        /// If you instead want to exit successfully and keep the intermediate changes
        /// in that case, pass this flag.
        #[clap(long)]
        allow_noop: bool,
        #[clap(flatten)]
        shared: SharedArgs,
    },
    /// Push changes into a new branch of push-repo or the given user's upstream fork.
    Push {
        /// Branch that should be pushed to your remote
        branch: String,

        /// GitHub username owning the fork (optional when push-repo is configured).
        username: Option<String>,
        #[clap(flatten)]
        shared: SharedArgs,
    },
}

#[derive(clap::Parser)]
struct SharedArgs {
    /// Path to the josh-sync TOML config file.
    #[clap(long, default_value(DEFAULT_CONFIG_PATH))]
    config_path: PathBuf,

    /// Override the mirror's GitHub organization/repository.
    #[clap(long)]
    mirror: Option<String>,

    /// Override the configured Josh filter.
    #[clap(long)]
    filter: Option<String>,

    /// Branch used as the upstream source and reconciliation base.
    #[clap(long)]
    upstream_branch: Option<String>,

    /// Path to a local josh-proxy binary (outside CI only).
    /// Without a proxy URL or binary, it will be installed outside CI.
    ///
    /// Warning: if you use a custom Josh version, ensure that it works properly!
    #[clap(long, conflicts_with = "proxy_url")]
    josh_proxy: Option<PathBuf>,

    /// URL of an externally provisioned josh-proxy; overrides the TOML proxy-url.
    #[clap(long, env = "JOSH_PROXY_URL")]
    proxy_url: Option<String>,

    /// Print executed commands.
    #[clap(long, short = 'v', env = "JOSH_SYNC_VERBOSE")]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.cmd {
        Command::Init => {
            let config = JoshConfig {
                org: "<organization>".to_string(),
                repo: "<repository-name>".to_string(),
                upstream_repo: DEFAULT_UPSTREAM_REPO.to_string(),
                upstream_branch: "HEAD".to_string(),
                push_repo: None,
                proxy_url: None,
                github_url: "https://github.com".to_string(),
                path: Some("<relative-subtree-path>".to_string()),
                filter: None,
                post_pull: vec![],
                subtree_filter: None,
                filter_version: FilterVersion::latest(),
            };
            config
                .write(Path::new(DEFAULT_CONFIG_PATH))
                .context("cannot write config")?;
            println!("Created config file at {DEFAULT_CONFIG_PATH}");
        }
        Command::Pull {
            upstream_repo,
            upstream_commit,
            allow_noop,
            shared,
        } => {
            let ctx = load_context(&shared)?;
            let josh = get_josh_proxy(&shared, &ctx.config)?;
            let sync = GitSync::new(ctx.clone(), josh, shared.verbose);
            match sync.rustc_pull(
                upstream_repo.unwrap_or_else(|| ctx.config.upstream_repo.clone()),
                upstream_commit,
                allow_noop,
            ) {
                Ok(result) => {
                    if !maybe_create_gh_pr(
                        &ctx.config.full_repo_name(),
                        &format!("{} pull update", ctx.config.upstream_repo),
                        &result.merge_commit_message,
                    )? {
                        println!(
                            "Now push the current branch to {} (either a fork or the main repo) and create a PR",
                            ctx.config.repo
                        );
                    }
                }
                Err(RustcPullError::NothingToPull) => {
                    eprintln!("Nothing to pull");
                    if !allow_noop {
                        std::process::exit(2);
                    }
                }
                Err(RustcPullError::PullFailed(error)) => {
                    eprintln!("Pull failure: {error:?}");
                    if !shared.verbose {
                        eprintln!("Rerun with `-v` to see executed commands");
                    }
                    std::process::exit(1);
                }
            }
        }
        Command::Push {
            username,
            branch,
            shared,
        } => {
            let ctx = load_context(&shared)?;
            let josh = get_josh_proxy(&shared, &ctx.config)?;
            let sync = GitSync::new(ctx.clone(), josh, shared.verbose);
            match sync
                .rustc_push(username.as_deref().unwrap_or_default(), &branch)
                .context("cannot perform push")
            {
                Ok(PushResult::NothingToPush) => {
                    eprintln!("Nothing to push");
                    std::process::exit(2);
                }
                Ok(PushResult::Pushed) => {}
                Err(error) => {
                    if !shared.verbose {
                        eprintln!("Rerun with `-v` to see executed commands");
                    }
                    return Err(error);
                }
            }

            let title = format!("{} subtree update", ctx.config.repo);
            let head = get_current_head_sha(shared.verbose)?;

            let merge_msg = format!(
                r#"Subtree update of `{repo}` to https://github.com/{full_repo}/commit/{head}.

Created using https://github.com/rust-lang/josh-sync."#,
                repo = ctx.config.repo,
                full_repo = ctx.config.full_repo_name(),
            );

            let push_repo = ctx
                .config
                .push_repo(username.as_deref().unwrap_or_default())?;
            let push_owner = push_repo.split_once('/').context("invalid push-repo")?.0;
            let upstream_repo = &ctx.config.upstream_repo;
            println!(
                r#"You can create the upstream PR using the following URL:
https://github.com/{upstream_repo}/compare/{push_owner}:{branch}?quick_pull=1&title={}&body={}"#,
                urlencoding::encode(&title),
                urlencoding::encode(&merge_msg)
            );
        }
    }

    Ok(())
}

fn load_context(args: &SharedArgs) -> anyhow::Result<SyncContext> {
    let mut config = load_config(&args.config_path)
        .context("cannot load config. Run the `init` command to initialize it.")?;
    if let Some(mirror) = &args.mirror {
        let (org, repo) = mirror
            .split_once('/')
            .context("mirror must be organization/repository")?;
        config.org = org.to_string();
        config.repo = repo.to_string();
    }
    if let Some(filter) = &args.filter {
        config.path = None;
        config.filter = Some(filter.clone());
    }
    if let Some(branch) = &args.upstream_branch {
        config.upstream_branch = branch.clone();
    }
    config.validate()?;
    Ok(SyncContext { config })
}

fn maybe_create_gh_pr(repo: &str, title: &str, description: &str) -> anyhow::Result<bool> {
    if which::which("gh").is_ok()
        && prompt(
            &format!("Do you want to create a {repo} pull PR using the `gh` tool?"),
            false,
        )
    {
        std::process::Command::new("gh")
            .args(&[
                "pr",
                "create",
                "--title",
                title,
                "--body",
                description,
                "--repo",
                repo,
            ])
            .spawn()?
            .wait()?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn get_josh_proxy(args: &SharedArgs, config: &JoshConfig) -> anyhow::Result<JoshProxy> {
    if let Some(url) = args.proxy_url.as_ref().or(config.proxy_url.as_ref()) {
        anyhow::ensure!(
            args.josh_proxy.is_none(),
            "cannot specify both a proxy URL and local binary"
        );
        return JoshProxy::from_url(url.clone());
    }
    anyhow::ensure!(
        !is_inside_ci(),
        "CI requires --proxy-url, JOSH_PROXY_URL, or proxy-url in the config"
    );
    match &args.josh_proxy {
        Some(path) => {
            println!("Using josh-proxy binary from {}", path.display());
            Ok(JoshProxy::from_path(path.clone()))
        }
        None => match try_install_josh_proxy(args.verbose) {
            Some(proxy) => Ok(proxy),
            None => Err(anyhow::anyhow!("Could not install josh-proxy")),
        },
    }
}
