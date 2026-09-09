use crate::SyncContext;
use crate::config::{JoshConfig, PostPullOperation};
use crate::josh::{JoshFilter, JoshProxy, try_install_josh_filter};
use crate::utils::{ensure_clean_git_state, is_inside_ci};
use crate::utils::{get_current_head_sha, run_command_at};
use crate::utils::{run_command, stream_command};
use anyhow::{Context, Error};

pub const DEFAULT_UPSTREAM_REPO: &str = "rust-lang/rust";

const NO_REBASE_WARN: &str = "Do NOT amend/squash/rebase any of the commits produced by this tool; that can badly break future syncs.";

pub enum RustcPullError {
    /// No changes are available to be pulled.
    NothingToPull,
    /// A rustc-pull has failed, probably a git operation error has occurred.
    PullFailed(anyhow::Error),
}

impl From<anyhow::Error> for RustcPullError {
    fn from(error: Error) -> Self {
        Self::PullFailed(error)
    }
}

#[derive(Copy, Clone)]
pub enum FilterVersion {
    /// Keep empty merge commits.
    Version1,
    /// Skip empty merge commits.
    Version2,
}

impl FilterVersion {
    pub fn latest() -> Self {
        Self::Version2
    }
}

pub struct PullResult {
    pub merge_commit_message: String,
}

pub enum PushResult {
    Pushed,
    NothingToPush,
}

pub struct GitSync {
    context: SyncContext,
    proxy: JoshProxy,
    verbose: bool,
}

impl GitSync {
    pub fn new(context: SyncContext, proxy: JoshProxy, verbose: bool) -> Self {
        Self {
            context,
            proxy,
            verbose,
        }
    }

    pub fn rustc_pull(
        &self,
        upstream_repo: String,
        upstream_commit: Option<String>,
        allow_noop: bool,
    ) -> Result<PullResult, RustcPullError> {
        // The upstream commit that we want to pull
        let upstream_sha = if let Some(sha) = upstream_commit {
            sha
        } else {
            let branch = &self.context.config.upstream_branch;
            let upstream_ref = if branch == "HEAD" || branch.starts_with("refs/") {
                branch.clone()
            } else {
                format!("refs/heads/{branch}")
            };
            let out = run_command(
                [
                    "git",
                    "ls-remote",
                    &self.context.config.git_url(&upstream_repo),
                    &upstream_ref,
                ],
                self.verbose,
            )
            .context("cannot fetch upstream commit")?;
            out.split_whitespace()
                .next()
                .context("upstream branch was not advertised by the remote")?
                .to_owned()
        };

        ensure_clean_git_state(self.verbose)?;

        // Make sure josh is running.
        let josh = self
            .proxy
            .start(&self.context.config)
            .context("cannot start josh-proxy")?;
        let josh_url = josh.git_url(
            &upstream_repo,
            Some(&upstream_sha),
            &construct_josh_filter(&self.context.config),
        );

        let orig_head = get_current_head_sha(self.verbose)?;
        println!("new upstream base: {upstream_sha}");
        println!("original local HEAD: {orig_head}");
        let mut git_reset = GitResetOnDrop::new(orig_head, self.verbose);

        // Fetch given rustc commit.
        run_command(&["git", "fetch", &josh_url], self.verbose)
            .context("cannot fetch git state through Josh")?;

        // This should not add any new root commits. So count those before and after merging.
        let num_roots = || -> anyhow::Result<u32> {
            Ok(run_command(
                &["git", "rev-list", "HEAD", "--max-parents=0", "--count"],
                self.verbose,
            )
            .context("failed to determine the number of root commits")?
            .parse::<u32>()?)
        };
        let num_roots_before = num_roots()?;

        let sha_pre_merge = get_current_head_sha(self.verbose)?;

        // The filtered SHA of upstream
        let incoming_ref = run_command(["git", "rev-parse", "FETCH_HEAD"], self.verbose)?;
        println!("incoming ref: {incoming_ref}");

        let merge_message = format!(
            r#"Merge ref '{upstream_head_short}' from {upstream_repo}

Pull recent changes from https://github.com/{upstream_repo} via Josh.

Upstream ref: {upstream_repo}@{upstream_sha}
Filtered ref: {sub_org}/{sub_repo}@{incoming_ref}

This merge was created using https://github.com/rust-lang/josh-sync.
"#,
            upstream_head_short = &upstream_sha[..12],
            sub_org = self.context.config.org,
            sub_repo = self.context.config.repo,
        );

        // Merge the fetched commit.
        // It is useful to print stdout/stderr here, because it shows the git diff summary
        if let Err(error) = stream_command(
            &[
                "git",
                "merge",
                "FETCH_HEAD",
                "--no-verify",
                "--no-ff",
                "-m",
                &merge_message,
            ],
            self.verbose,
        )
        .context("FAILED to merge new commits, something went wrong")
        {
            eprintln!(
                r"The merge was unsuccessful (maybe there was a conflict?).
NOT rolling back the branch state, so you can examine it manually.
After you fix the conflicts, `git add` the changes and run `git merge --continue`."
            );
            eprintln!("{NO_REBASE_WARN}");
            git_reset.disarm();
            return Err(RustcPullError::PullFailed(error));
        }

        // Now detect if something has actually been pulled
        let current_sha = get_current_head_sha(self.verbose)?;

        // This is the easy case, no merge was performed, so we bail, unless `allow_noop` is true
        if current_sha == sha_pre_merge && !allow_noop {
            eprintln!("No merge was performed, no changes to pull were found. Rolling back.");
            return Err(RustcPullError::NothingToPull);
        }

        // But it can be more tricky - we can have only empty merge/rollup merge commits from
        // rustc, so a merge was created, but the in-tree diff can still be empty.
        // In that case we also bail, unless `allow_noop` is true.
        if self.has_empty_diff(&sha_pre_merge) && !allow_noop {
            eprintln!("Only empty changes were pulled. Rolling back.");
            return Err(RustcPullError::NothingToPull);
        }

        println!("Pull finished! Current HEAD is {current_sha}");
        println!("{NO_REBASE_WARN}");

        if !self.context.config.post_pull.is_empty() {
            println!("Running post-pull operation(s)");

            for op in &self.context.config.post_pull {
                self.run_post_pull_op(&op)?;
            }
        }

        git_reset.disarm();

        // Check that the number of roots did not change.
        if num_roots()? != num_roots_before {
            return Err(anyhow::anyhow!(
                "Josh created a new root commit. This is probably not the history you want."
            )
            .into());
        }

        Ok(PullResult {
            merge_commit_message: merge_message,
        })
    }

    pub fn rustc_push(&self, username: &str, branch: &str) -> anyhow::Result<PushResult> {
        ensure_clean_git_state(self.verbose)?;
        run_command(
            ["git", "check-ref-format", "--branch", branch],
            self.verbose,
        )?;
        let push_repo = self.context.config.push_repo(username)?;
        let josh = self.proxy.start(&self.context.config)?;
        let josh_url = josh.git_url(
            &push_repo,
            None,
            &construct_josh_filter(&self.context.config),
        );
        let branch_base = &self.context.config.upstream_branch;
        let base = if branch_base == "HEAD" || branch_base.starts_with("refs/") {
            branch_base.clone()
        } else {
            format!("refs/heads/{branch_base}")
        };
        run_command(["git", "fetch", &josh_url, &base], self.verbose)?;
        let local_head = self.local_head(&self.context.config)?;
        let common_base = run_command(
            ["git", "merge-base", "FETCH_HEAD", &local_head],
            self.verbose,
        )?;
        let changes = run_command(
            ["git", "diff", "--name-only", &common_base, &local_head],
            self.verbose,
        )?;
        if changes.is_empty() {
            return Ok(PushResult::NothingToPush);
        }

        let target_ref = format!("refs/heads/{branch}");
        let existing = run_command(
            [
                "git",
                "ls-remote",
                "--heads",
                &self.context.config.git_url(&push_repo),
                &target_ref,
            ],
            self.verbose,
        )?;
        anyhow::ensure!(
            existing.is_empty(),
            "target branch already exists: {push_repo}/{branch}"
        );
        run_command(
            [
                "git",
                "push",
                "-o",
                &format!("base={base}"),
                &format!("--force-with-lease={target_ref}:"),
                &josh_url,
                &format!("HEAD:{target_ref}"),
            ],
            self.verbose,
        )?;
        self.roundtrip_check(&self.context.config, &josh_url, branch)?;
        println!("{NO_REBASE_WARN}");
        Ok(PushResult::Pushed)
    }

    fn has_empty_diff(&self, baseline_sha: &str) -> bool {
        // `git diff --exit-code` "succeeds" if the diff is empty.
        run_command(&["git", "diff", "--exit-code", baseline_sha], self.verbose).is_ok()
    }

    fn run_post_pull_op(&self, op: &PostPullOperation) -> anyhow::Result<()> {
        let head = get_current_head_sha(self.verbose)?;
        run_command(op.cmd.iter().map(|s| s.as_str()).collect::<Vec<_>>(), true)?;
        if !self.has_empty_diff(&head) {
            println!(
                "`{}` changed something, committing with message `{}`",
                op.cmd.join(" "),
                op.commit_message
            );
            run_command(["git", "add", "-u"], self.verbose)?;
            run_command(["git", "commit", "-m", &op.commit_message], self.verbose)?;
        }

        Ok(())
    }

    fn local_head(&self, config: &JoshConfig) -> anyhow::Result<String> {
        if let Some(subtree_filter) = &config.subtree_filter {
            let josh_filter = get_josh_filter(self.verbose, self.proxy.is_external())?;
            josh_filter.run(
                &[subtree_filter, "HEAD"],
                &std::env::current_dir().unwrap(),
                self.verbose,
            )?;
            run_command(&["git", "rev-parse", "FILTERED_HEAD"], self.verbose)
                .context("failed to get FILTERED_HEAD")
        } else {
            get_current_head_sha(self.verbose)
        }
    }

    fn roundtrip_check(
        &self,
        config: &JoshConfig,
        josh_url: &str,
        branch: &str,
    ) -> anyhow::Result<()> {
        run_command_at(
            &["git", "fetch", josh_url, branch],
            &std::env::current_dir().unwrap(),
            self.verbose,
        )?;
        let head = self.local_head(config)?;
        let fetch_head = run_command(&["git", "rev-parse", "FETCH_HEAD"], self.verbose)?;
        if head != fetch_head {
            return Err(anyhow::anyhow!(
                "Josh created a non-roundtrip push! Do NOT merge this upstream!\n\
                Expected {head}, got {fetch_head}."
            ));
        }
        println!(
            "Confirmed that the push round-trips back to {} properly. Please create an upstream PR.",
            self.context.config.repo
        );
        Ok(())
    }
}

// This is called only when the `subtree-filter` is set.
fn get_josh_filter(verbose: bool, external_proxy: bool) -> anyhow::Result<JoshFilter> {
    if let Ok(path) = which::which("josh-filter") {
        return Ok(JoshFilter::from_path(path));
    }
    anyhow::ensure!(
        !external_proxy && !is_inside_ci(),
        "josh-filter must be provisioned on PATH for subtree-filter checks"
    );
    println!("Updating/installing josh-filter binary...");
    match try_install_josh_filter(verbose) {
        Some(filter) => Ok(filter),
        None => Err(anyhow::anyhow!("Could not install josh-filter")),
    }
}

/// Restores HEAD to `reset_to` on drop, unless `disarm` is called first.
struct GitResetOnDrop {
    disarmed: bool,
    reset_to: String,
    verbose: bool,
}

impl GitResetOnDrop {
    fn new(current_sha: String, verbose: bool) -> Self {
        Self {
            disarmed: false,
            reset_to: current_sha,
            verbose,
        }
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for GitResetOnDrop {
    fn drop(&mut self) {
        if !self.disarmed {
            eprintln!("Reverting HEAD to {}", self.reset_to);
            run_command(&["git", "reset", "--hard", &self.reset_to], self.verbose)
                .expect(&format!("cannot reset current branch to {}", self.reset_to));
        }
    }
}

fn construct_josh_filter(config: &JoshConfig) -> String {
    let filter = match (&config.path, &config.filter) {
        (Some(path), None) => format!(":/{path}"),
        (None, Some(filter)) => filter.clone(),
        _ => panic!("Config contains both path and a filter"),
    };
    match config.filter_version {
        // Keep backwards compatibility with repositories that started with a legacy version of
        // Josh.
        FilterVersion::Version1 => {
            // Convert old :rev syntax
            let filter = convert_rev_syntax(&filter);
            // Keep empty merges
            wrap_compat(&filter)
        }
        // Use the current default behavior of Josh.
        FilterVersion::Version2 => filter,
    }
}

/// Converts filters from old `:rev(sha:filter)` syntax to new
/// `:rev(<=sha:filter)` syntax. Null SHAs (40 zeros) become `_`.
/// Only touches SHAs inside `:rev(...)` blocks.
fn convert_rev_syntax(input: &str) -> String {
    let rev_block = regex::Regex::new(r":rev\([^)]*\)").unwrap();
    let entry = regex::Regex::new(
        r"(?x)
        ([,(])                # delimiter before entry
        (0{40}|[0-9a-f]{40})  # full SHA
        :                     # colon separator
    ",
    )
    .unwrap();

    rev_block
        .replace_all(input, |block: &regex::Captures| {
            entry
                .replace_all(&block[0], |caps: &regex::Captures| {
                    let delim = &caps[1];
                    let sha = &caps[2];
                    if sha.chars().all(|c| c == '0') {
                        format!("{delim}_:")
                    } else {
                        format!("{delim}<={sha}:")
                    }
                })
                .into_owned()
        })
        .into_owned()
}

/// Wraps a filter with the backwards compatibility meta options for
/// trivial merge preservation and CRLF normalization in gpgsig headers.
///
/// `:your/filter` becomes
/// `:~(history="keep-trivial-merges",gpgsig="norm-lf")[:your/filter]`
fn wrap_compat(filter: &str) -> String {
    format!(":~(history=\"keep-trivial-merges\",gpgsig=\"norm-lf\")[{filter}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rev_block_unchanged() {
        assert_eq!(convert_rev_syntax(":/some/path"), ":/some/path");
    }

    #[test]
    fn single_sha_gets_prefix() {
        assert_eq!(
            convert_rev_syntax(":rev(3a1f5e2b9c8d4e7f6a0b1c2d3e4f5a6b7c8d9e0f:/some/path)"),
            ":rev(<=3a1f5e2b9c8d4e7f6a0b1c2d3e4f5a6b7c8d9e0f:/some/path)",
        );
    }

    #[test]
    fn null_sha_becomes_underscore() {
        assert_eq!(
            convert_rev_syntax(":rev(0000000000000000000000000000000000000000:/some/path)"),
            ":rev(_:/some/path)",
        );
    }

    #[test]
    fn multiple_entries_in_rev_block() {
        assert_eq!(
            convert_rev_syntax(
                ":rev(3a1f5e2b9c8d4e7f6a0b1c2d3e4f5a6b7c8d9e0f:/p1,\
                 e4c7a2d8f1b3e5a9d6c0f2b4a7e1d3c5f8a0b6e9:/p2,\
                 0000000000000000000000000000000000000000:/p3)"
            ),
            ":rev(<=3a1f5e2b9c8d4e7f6a0b1c2d3e4f5a6b7c8d9e0f:/p1,\
             <=e4c7a2d8f1b3e5a9d6c0f2b4a7e1d3c5f8a0b6e9:/p2,\
             _:/p3)",
        );
    }

    #[test]
    fn already_converted_syntax_unchanged() {
        assert_eq!(
            convert_rev_syntax(":rev(<=3a1f5e2b9c8d4e7f6a0b1c2d3e4f5a6b7c8d9e0f:/some/path)"),
            ":rev(<=3a1f5e2b9c8d4e7f6a0b1c2d3e4f5a6b7c8d9e0f:/some/path)",
        );
    }

    #[test]
    fn underscore_syntax_unchanged() {
        assert_eq!(
            convert_rev_syntax(":rev(_:/some/path)"),
            ":rev(_:/some/path)",
        );
    }

    #[test]
    fn sha_outside_rev_block_unchanged() {
        assert_eq!(
            convert_rev_syntax("3a1f5e2b9c8d4e7f6a0b1c2d3e4f5a6b7c8d9e0f:/some/path"),
            "3a1f5e2b9c8d4e7f6a0b1c2d3e4f5a6b7c8d9e0f:/some/path",
        );
    }

    #[test]
    fn wrap_compat_simple_filter() {
        assert_eq!(
            wrap_compat(":/some/path"),
            ":~(history=\"keep-trivial-merges\",gpgsig=\"norm-lf\")[:/some/path]",
        );
    }

    #[test]
    fn wrap_compat_rev_filter() {
        assert_eq!(
            wrap_compat(
                ":rev(75dd959a3a40eb5b4574f8d2e23aa6efbeb33573:prefix=src/tools/miri):/src/tools/miri"
            ),
            ":~(history=\"keep-trivial-merges\",gpgsig=\"norm-lf\")\
             [:rev(75dd959a3a40eb5b4574f8d2e23aa6efbeb33573:prefix=src/tools/miri):/src/tools/miri]",
        );
    }

    #[test]
    fn multiple_rev_blocks() {
        assert_eq!(
            convert_rev_syntax(
                ":rev(3a1f5e2b9c8d4e7f6a0b1c2d3e4f5a6b7c8d9e0f:/p1)\
                 :rev(e4c7a2d8f1b3e5a9d6c0f2b4a7e1d3c5f8a0b6e9:/p2)"
            ),
            ":rev(<=3a1f5e2b9c8d4e7f6a0b1c2d3e4f5a6b7c8d9e0f:/p1)\
             :rev(<=e4c7a2d8f1b3e5a9d6c0f2b4a7e1d3c5f8a0b6e9:/p2)",
        );
    }
}
