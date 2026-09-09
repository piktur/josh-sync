use crate::sync::FilterVersion;
use anyhow::Context;
use std::path::Path;

#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct JoshConfig {
    #[serde(default = "default_org")]
    pub org: String,
    pub repo: String,
    #[serde(default = "default_upstream_repo")]
    pub upstream_repo: String,
    #[serde(default = "default_upstream_branch")]
    pub upstream_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(default = "default_github_url")]
    pub github_url: String,
    /// Relative path where the subtree is located in the upstream repository.
    /// For example `src/doc/rustc-dev-guide`.
    pub path: Option<String>,
    /// Optional filter specification for Josh.
    /// It cannot be used together with `path`.
    pub filter: Option<String>,
    /// Operation(s) that should be performed after a pull.
    /// Can be used to post-process the state of the repository after a pull happens.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_pull: Vec<PostPullOperation>,
    /// Optional subtree filter applied to the local `HEAD` during round-trip check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtree_filter: Option<String>,
    /// Optional filter version that determines which post-processing will be applied to the
    /// specified filter.
    ///
    /// This exists for backwards compatibility with repositories using an older filter syntax.
    #[serde(
        default = "default_filter_version",
        skip_serializing_if = "skip_serializing_filter_version",
        with = "filter_version"
    )]
    pub filter_version: FilterVersion,
}

impl JoshConfig {
    pub fn full_repo_name(&self) -> String {
        format!("{}/{}", self.org, self.repo)
    }

    pub fn git_url(&self, repo: &str) -> String {
        format!("{}/{repo}", self.github_url.trim_end_matches('/'))
    }

    pub fn push_repo(&self, username: &str) -> anyhow::Result<String> {
        if let Some(repo) = &self.push_repo {
            return Ok(repo.clone());
        }
        anyhow::ensure!(
            !username.is_empty(),
            "push requires a username or push-repo"
        );
        let (_, repo) = self
            .upstream_repo
            .split_once('/')
            .context("invalid upstream-repo")?;
        Ok(format!("{username}/{repo}"))
    }

    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        let config = toml::to_string_pretty(self).context("cannot serialize config")?;
        std::fs::write(path, config).context("cannot write config")?;
        Ok(())
    }
}

mod filter_version {
    use crate::sync::FilterVersion;
    use serde::de::{Error, Unexpected};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        version: &FilterVersion,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let num: u32 = match version {
            FilterVersion::Version1 => 1,
            FilterVersion::Version2 => 2,
        };
        num.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<FilterVersion, D::Error> {
        let num = u32::deserialize(deserializer)?;
        match num {
            1 => Ok(FilterVersion::Version1),
            2 => Ok(FilterVersion::Version2),
            v => Err(D::Error::invalid_value(
                Unexpected::Unsigned(v as u64),
                &"1 or 2",
            )),
        }
    }
}

fn default_filter_version() -> FilterVersion {
    FilterVersion::Version1
}

fn skip_serializing_filter_version(version: &FilterVersion) -> bool {
    match version {
        FilterVersion::Version1 => true,
        FilterVersion::Version2 => false,
    }
}

/// Execute an operation after a pull, and if something changes in the local git state,
/// perform a commit.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct PostPullOperation {
    /// Execute a command with these arguments
    /// At least one argument has to be present.
    /// You can run e.g. bash if you want to do more complicated stuff.
    pub cmd: Vec<String>,
    /// If the git state has changed after `cmd`, add all changes to the index (`git add -u`)
    /// and create a commit with the following commit message.
    pub commit_message: String,
}

fn default_org() -> String {
    String::from("rust-lang")
}

fn default_upstream_repo() -> String {
    crate::sync::DEFAULT_UPSTREAM_REPO.to_string()
}

fn default_upstream_branch() -> String {
    "HEAD".to_string()
}

fn default_github_url() -> String {
    "https://github.com".to_string()
}

pub fn load_config(path: &Path) -> anyhow::Result<JoshConfig> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("cannot load config file from {}", path.display()))?;
    let config: JoshConfig = toml::from_str(&data).context("cannot load config as TOML")?;
    if config.path.is_some() == config.filter.is_some() {
        return if config.path.is_some() {
            Err(anyhow::anyhow!("Cannot specify both `path` and `filter`"))
        } else {
            Err(anyhow::anyhow!("Must specify one of `path` and `filter`"))
        };
    }

    Ok(config)
}
