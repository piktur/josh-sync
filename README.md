# Josh sync utilities
This repository contains a binary utility for performing [Josh](https://github.com/josh-project/josh)
synchronizations (pull and push) between a configured upstream repository and its subtrees.

## Configurable upstream fork

This fork retains the synchronization algorithm from upstream revision
`a52ea5c02ec17bd0556ab99b0b4297846c1a0154`. Set `upstream-repo` and
`upstream-branch` in `josh-sync.toml` to select the source; `org`/`repo` still
identify the mirror. `push-repo` selects the upstream repository or fork that
receives a new branch. With it configured, use `rustc-josh-sync push <branch>`;
the existing optional username selects `<username>/<upstream-repository-name>`.

Set `proxy-url`, `--proxy-url`, or `JOSH_PROXY_URL` to use an externally managed
Josh proxy. CI requires this mode and never installs or starts Josh. Provision
`josh-filter` on PATH when using `subtree-filter`. The proxy's configured remote
must match `github-url` (default `https://github.com`), which also allows local
Git HTTP fixtures. CLI/environment proxy selection overrides TOML.

`--rust-version-path` selects the tracking file; `RUSTC_GIT` selects a separate
upstream checkout for push preparation. Keep both checkouts isolated per job.
Push refuses existing target branches and protects creation against races.
Preserve the generated merge history: do not squash or rebase sync commits.

## Installation
You can install the binary `rustc-josh-sync` tool using the following command:

```bash
$ cargo install --locked --path .
```

## Creating config file

First, create a configuration file for a given subtree repo using `rustc-josh-sync init`. The config will be created under the path `josh-sync.toml`. Modify the file to fill in the name of the subtree repository (e.g. `stdarch`) and its relative path in the main `rust-lang/rust` repository (e.g. `library/stdarch`).

If you need to specify a more complex Josh `filter`, use `filter` field in the configuration file instead of the `path` field.

The `init` command will also create an empty `rust-version` file (if it doesn't already exist) that stores the last upstream `rustc` SHA that was synced in the subtree.

The [`josh-sync.example.toml`](josh-sync.example.toml) file contains all the things that can be configured.

## Performing pull

A pull operation fetches changes to the subtree subdirectory that were performed in `rust-lang/rust` and merges them into the subtree repository. After performing a pull, a pull request is sent against the *subtree repository*. We *pull from rustc*.

1) Checkout the latest default branch of the subtree
2) Create a new branch that will be used for the subtree PR, e.g. `pull`
3) Run `rustc-josh-sync pull`
4) Send a PR to the subtree repository

- Note that `rustc-josh-sync` can do this for you if you have the [gh](https://cli.github.com/) CLI tool installed.

You can also configure a set of postprocessing operations to be performed after a successful pull using the `post-pull` configuration.

## Performing push

A push operation takes changes performed in the subtree repository and merges them into the subtree subdirectory of the `rust-lang/rust` repository. After performing a push, a push request is sent against the *rustc repository*. We *push to rustc*.

1) Checkout the latest default branch of the subtree
2) Run `rustc-josh-sync push <branch> <your-github-username>`

- The branch with the push contents will be created in `https://github.com/<your-github-username>/rust` fork, in the `<branch>` branch.

3) Send a PR to [rust-lang/rust]

## Automation

Use the organization's reusable [Josh sync workflow](https://github.com/piktur/finance/blob/feature/ci-opt-mirror-routing/.github/workflows/josh-sync.yml).
Nix provisions the binary and authenticated proxy on the homeserver. The workflow
opens pull requests in either direction and preserves merge history.

## Git peculiarities

NOTE: If you use Git/SSH protocol to push to your fork of [rust-lang/rust],
ensure that you have this entry in your Git config,
else the 2 steps that follow would prompt for a username and password:

```
[url "git@github.com:"]
insteadOf = "https://github.com/"
```

### Minimal git config

For simplicity (ease of implementation purposes), the josh-sync script simply calls out to system git. This means that the git invocation may be influenced by global (or local) git configuration.

You may observe "Nothing to pull" even if you *know* rustc-pull has something to pull if your global git config sets `fetch.prunetags = true` (and possibly other configurations may cause unexpected outcomes).

To minimize the likelihood of this happening, you may wish to keep a separate *minimal* git config that *only* has `[user]` entries from global git config, then repoint system git to use the minimal git config instead. E.g.

```
GIT_CONFIG_GLOBAL=/path/to/minimal/gitconfig GIT_CONFIG_SYSTEM='' rustc-josh-sync ...
```

[rust-lang/rust]: (https://github.com/rust-lang/rust)
