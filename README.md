# Josh sync

Synchronize a repository and its Josh-filtered mirrors using the upstream
[rust-lang/josh-sync](https://github.com/rust-lang/josh-sync/tree/a52ea5c02ec17bd0556ab99b0b4297846c1a0154)
merge and round-trip checks. Josh's native `base` push option translates mirror
history into a reconciliation branch; the selected upstream branch supplies the
base. No committed tracking file or separate upstream checkout is needed.

## Install

Install the checked-out revision:

```sh
cargo install --locked --path .
```

Provision an external Josh proxy and set `JOSH_PROXY_URL`, `--proxy-url`, or the
TOML `proxy-url`. CLI/environment selection overrides TOML. CI never installs or
starts Josh. `subtree-filter` configurations also require `josh-filter` on PATH.

## Configure

A trusted shared configuration can contain only the upstream settings:

```toml
upstream-repo = "piktur/finance"
push-repo = "piktur/finance"
filter-version = 2
```

Pass the mirror, filter, and common branch at runtime, or declare `org`, `repo`,
`path`/`filter`, and `upstream-branch` in TOML. Exactly one of `path` and `filter`
is required after overrides. See [the example](josh-sync.example.toml) for
filter compatibility and post-pull options. `josh-sync init` creates only the
configuration file. `github-url` defaults to `https://github.com`; local Git HTTP
fixtures can override it to match the proxy's remote.

## Synchronize

From a clean mirror checkout derived from the same Josh-filtered history:

```sh
josh-sync pull --config-path /trusted/josh-sync.toml \
  --mirror piktur/example --filter ':/apps/example' \
  --upstream-branch feature/topic

josh-sync push feature/sync/topic --config-path /trusted/josh-sync.toml \
  --mirror piktur/example --filter ':/apps/example' \
  --upstream-branch feature/topic
```

Both directions select `feature/topic` as the upstream base. Pull merges its
filtered history locally; the caller publishes that result for review against
the mirror's `feature/topic`. Push creates `feature/sync/topic` in `push-repo`;
the caller opens a PR against the upstream `feature/topic`. Existing target
branches are rejected. Neither operation updates the upstream base branch.

Both commands exit `2` when there are no changes in the requested direction. Pull's
`--allow-noop` retains its upstream behavior and exits successfully instead.
Conflicts remain in the checkout for resolution; merge sync PRs without squash
or rebase. Git supplies authentication and hook configuration, so automation
should use its existing trusted Git configuration.
