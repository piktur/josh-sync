use anyhow::Context;
use clap::Parser;
use rustc_josh_sync::SyncContext;
use rustc_josh_sync::config::{JoshConfig, load_config};
use rustc_josh_sync::josh::{JoshProxy, try_install_josh_proxy};
use rustc_josh_sync::sync::{
    DEFAULT_UPSTREAM_REPO, FilterVersion, GitSync, PushResult, RustcPullError,
};
use rustc_josh_sync::utils::{get_current_head_sha, is_inside_ci, prompt};
use std::path::{Path, PathBuf};

const DEFAULT_CONFIG_PATH: &str = "josh-sync.toml";
const DEFAULT_RUST_VERSION_PATH: &str = "rust-version";

#[derive(clap::Parser)]
struct Args {
    #[clap(subcommand)]
    cmd: Command,
}

#[derive(clap::Parser)]
enum Command {
    /// Initialize a config file and an empty `rust-version` file for this repository.
    Init,
    /// Pull changes from the configured upstream repository.
    /// This creates new commits that should be then merged into this subtree repository.
    Pull {
        /// Override the upstream repository from which we pull changes.
        /// Can be used to perform experimental pulls e.g. to test changes in the subtree repository
        /// that have not yet been merged in `rust-lang/rust`.
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

    /// Path to a file storing the last synchronized rustc commit.
    #[clap(long, default_value(DEFAULT_RUST_VERSION_PATH))]
    rust_version_path: PathBuf,

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
                org: "rust-lang".to_string(),
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

            if !Path::new(DEFAULT_RUST_VERSION_PATH).is_file() {
                std::fs::write(DEFAULT_RUST_VERSION_PATH, "")
                    .context("cannot write rust-version file")?;
                println!("Created empty rust-version file at {DEFAULT_RUST_VERSION_PATH}");
            } else {
                println!("{DEFAULT_RUST_VERSION_PATH} already exists, not doing anything with it");
            }
        }
        Command::Pull {
            upstream_repo,
            upstream_commit,
            allow_noop,
            shared,
        } => {
            let ctx = load_context(&shared.config_path, &shared.rust_version_path)?;
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
            let ctx = load_context(&shared.config_path, &shared.rust_version_path)?;
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

            // Open PR with `subtree update` title to silence the `no-merges` triagebot check
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

fn load_context(config_path: &Path, rust_version_path: &Path) -> anyhow::Result<SyncContext> {
    let config = load_config(&config_path)
        .context("cannot load config. Run the `init` command to initialize it.")?;
    let rust_version = std::fs::read_to_string(&rust_version_path)
        .inspect_err(|err| eprintln!("Cannot load rust-version file: {err:?}"))
        .map(|version| version.trim().to_string())
        .map(Some)
        .unwrap_or_default();
    Ok(SyncContext {
        config,
        last_upstream_sha_path: rust_version_path.to_path_buf(),
        last_upstream_sha: rust_version,
    })
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
