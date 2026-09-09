use crate::config::JoshConfig;
use crate::utils::{is_inside_ci, is_null_sha, run_command_by_path};
use anyhow::Context;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const JOSH_PORT: u16 = 42042;
/// Version of `josh-proxy` that should be downloaded for the user.
const JOSH_VERSION: &str = "r26.07.19";

pub enum JoshProxy {
    Local(PathBuf),
    External(String),
}

impl JoshProxy {
    pub fn from_path(path: PathBuf) -> Self {
        Self::Local(path)
    }

    pub fn from_url(url: String) -> anyhow::Result<Self> {
        let (scheme, rest) = url
            .split_once("://")
            .context("proxy URL requires http:// or https://")?;
        anyhow::ensure!(
            matches!(scheme, "http" | "https")
                && !rest.split('/').next().unwrap_or_default().is_empty()
                && !url.contains(['?', '#', '@']),
            "proxy URL must be an HTTP(S) base URL without credentials, query, or fragment"
        );
        Ok(Self::External(url.trim_end_matches('/').to_string()))
    }

    pub fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    pub fn start(&self, config: &JoshConfig) -> anyhow::Result<RunningJoshProxy> {
        let path = match self {
            Self::External(url) => {
                return Ok(RunningJoshProxy {
                    process: None,
                    url: url.clone(),
                });
            }
            Self::Local(path) => path,
        };
        anyhow::ensure!(!is_inside_ci(), "CI requires an external josh-proxy URL");
        // Determine cache directory.
        let user_dirs =
            directories::ProjectDirs::from("org", &config.full_repo_name(), "rustc-josh")
                .context("cannot determine cache directory for Josh")?;
        let local_dir = user_dirs.cache_dir().to_owned();

        // Start josh, silencing its output.
        let josh = std::process::Command::new(path)
            .arg("--local")
            .arg(local_dir)
            .args([
                &format!("--remote={}", config.github_url.trim_end_matches('/')),
                &format!("--port={JOSH_PORT}"),
                "--no-background",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start josh-proxy, make sure it is installed")?;

        // Wait until the port is open. We try every 10ms until 1s passed.
        for _ in 0..100 {
            // This will generally fail immediately when the port is still closed.
            let addr = SocketAddr::from(([127, 0, 0, 1], JOSH_PORT));
            let josh_ready = TcpStream::connect_timeout(&addr, Duration::from_millis(1));

            if josh_ready.is_ok() {
                println!("josh up and running");
                return Ok(RunningJoshProxy {
                    process: Some(josh),
                    url: format!("http://localhost:{JOSH_PORT}"),
                });
            }

            // Not ready yet.
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("Even after waiting for 1s, josh-proxy is still not available.");
    }
}

pub struct JoshFilter {
    path: PathBuf,
}

impl JoshFilter {
    pub fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn run<'a, Args: AsRef<[&'a str]>>(
        &self,
        args: Args,
        workdir: &Path,
        verbose: bool,
    ) -> anyhow::Result<()> {
        let args = args.as_ref();
        let output = run_command_by_path(&self.path, args, workdir, true, verbose)?;
        if is_null_sha(&output) {
            return Err(anyhow::anyhow!(
                "josh-filter returned null SHA, filter may not match any content"
            ));
        }
        Ok(())
    }
}

fn josh_install_directory() -> PathBuf {
    // Install binaries globally (into CARGO_HOME) on CI to ensure better (rust-)cache usage
    if is_inside_ci()
        && let Some(dirs) = directories::UserDirs::new()
    {
        return dirs.home_dir().join(".cargo");
    }

    let Some(user_dirs) = directories::ProjectDirs::from("org", "rust-lang", "rustc-josh") else {
        eprintln!(
            "Cannot determine user directory for Josh installation, falling back to local directory"
        );
        return PathBuf::from(".josh-sync").join("cargo");
    };
    let local_dir = user_dirs.data_local_dir();
    local_dir.join("josh-sync").join("cargo")
}

#[derive(Copy, Clone, Debug)]
enum JoshProgram {
    Proxy,
    Filter,
}

pub fn try_install_josh_proxy(verbose: bool) -> Option<JoshProxy> {
    try_install_josh_program(JoshProgram::Proxy, verbose).map(JoshProxy::from_path)
}

pub fn try_install_josh_filter(verbose: bool) -> Option<JoshFilter> {
    try_install_josh_program(JoshProgram::Filter, verbose).map(JoshFilter::from_path)
}

/// Try to install (or update) a josh CLI program in a local installation directory.
/// Ensures that we use the correct version.
fn try_install_josh_program(program: JoshProgram, verbose: bool) -> Option<PathBuf> {
    if is_inside_ci() {
        return None;
    }
    let install_dir = josh_install_directory();
    let (krate, binary) = match program {
        JoshProgram::Proxy => ("josh-proxy", "josh-proxy"),
        JoshProgram::Filter => ("josh-cli", "josh-filter"),
    };
    let path = install_dir.join("bin").join(binary);
    println!(
        "Updating/installing {binary} binary into `{}`...",
        path.display()
    );

    let mut args = vec![
        "+stable",
        "install",
        "--locked",
        "--git",
        "https://github.com/josh-project/josh",
        "--tag",
        JOSH_VERSION,
        "--root",
        install_dir.to_str()?,
    ];

    args.push(krate);

    run_command_by_path(
        &Path::new("cargo"),
        &args,
        &std::env::current_dir().unwrap(),
        false,
        verbose,
    )
    .unwrap_or_else(|e| panic!("cannot install {binary}: {e:?}"));
    if path.is_file() { Some(path) } else { None }
}

/// Create a wrapper that represents a running instance of `josh-proxy` and stops it on drop.
pub struct RunningJoshProxy {
    process: Option<std::process::Child>,
    url: String,
}

impl RunningJoshProxy {
    pub fn git_url(&self, repo: &str, commit: Option<&str>, filter: &str) -> String {
        let commit = commit.map(|c| format!("@{c}")).unwrap_or_default();
        let filter = urlencoding::encode(filter);
        format!("{}/{repo}.git{commit}{filter}.git", self.url)
    }
}

impl Drop for RunningJoshProxy {
    fn drop(&mut self) {
        let Some(process) = &mut self.process else {
            return;
        };
        if cfg!(unix) {
            // Try to gracefully shut it down.
            Command::new("kill")
                .args(["-s", "INT", &process.id().to_string()])
                .output()
                .expect("failed to SIGINT josh-proxy");
            // Sadly there is no "wait with timeout"... so we just give it some time to finish.
            // We try every 10ms until 1s passed.
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(10));
                // Now hopefully it is gone.
                if process
                    .try_wait()
                    .expect("failed to wait for josh-proxy")
                    .is_some()
                {
                    return;
                }
            }
        }
        // If that didn't work (or we're not on Unix), kill it hard.
        eprintln!(
            "I have to kill josh-proxy the hard way, let's hope this does not \
            break anything."
        );
        process.kill().expect("failed to SIGKILL josh-proxy");
    }
}
