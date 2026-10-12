# Quick start

You have one machine with Claude Code on it, a Claude.ai subscription or an Anthropic API key, and you want Claude
Code answering through the router on loopback. This page takes you from the binary to the first answer: install,
initialize, validate and start, then six steps in the console and the first request.

## What you end with

One `phoebis` process on your machine serving three listeners, every one on loopback: the proxy on `127.0.0.1:8317`,
which Claude Code sends its requests to, the admin listener on `127.0.0.1:8081`, which serves
[the console](console.md), and the metrics listener on `127.0.0.1:9464`. The proxy and the admin listener serve plain
HTTP, since the file `init` writes names no `tls` pair for either. Beside the bootstrap file sit the storage key and a
`data/` directory holding the registry and the ledger. In the console, one account is paired or one API key is
stored, one pool holds it, one key routes into that pool, and Claude Code presents the key on every request.

## What you need

A Linux host on `x86_64` or `aarch64` takes the release archive's static binary, which runs on any Linux of its
architecture with no shared library, glibc version or distribution in the way. Any other host builds from source
([Build](installation.md#build)). The console needs a browser, the first run a Claude.ai subscription to pair or an
Anthropic API key to store, and the first request Claude Code on the same machine.

## Install

Download the release archive for your architecture, check it and put the binary on the `PATH`
([Download a release](installation.md#download-a-release)):

```sh
release=https://github.com/daystrom-labs/phoebis/releases/download/<version>
curl -LO "$release/phoebis-<version>-x86_64-unknown-linux-musl.tar.gz"
curl -LO "$release/SHA256SUMS"
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf phoebis-<version>-x86_64-unknown-linux-musl.tar.gz
sudo install -m 0755 phoebis-<version>-x86_64-unknown-linux-musl/phoebis /usr/local/bin/phoebis
```

On an `aarch64` host the archive is `phoebis-<version>-aarch64-unknown-linux-musl.tar.gz`. To build instead,
`cargo build --release` leaves the binary at `target/release/phoebis` ([Build](installation.md#build)), and
`sudo install -m 0755 target/release/phoebis /usr/local/bin/phoebis` puts it on the `PATH`. Every command below
reads `phoebis`, the binary on the `PATH`.

## Initialize

```sh
phoebis init --config ~/.config/phoebis/config.yaml
```

`init` writes the bootstrap file at the path `--config` names, `storage.key` and `data/` beside it, and fills the file
with `data/router.sqlite` for the registry, `data/ledger.sqlite` for the ledger, the proxy on `127.0.0.1:8317`, the
admin listener on `127.0.0.1:8081` and metrics on `127.0.0.1:9464`. On a terminal it first asks
`proxy listen address [127.0.0.1:8317]: ` and `admin listen address [127.0.0.1:8081]: `, and Enter takes the default
in brackets. The key file is created at `0600`, the data directory at `0700` and the bootstrap file at `0644`, and
each lands on stderr as `wrote {path} (mode {mode}, owner {owner})` or
`created {path} (directory, mode {mode}, owner {owner})`. The key is created once and every stored credential is
sealed under it. `init` overwrites nothing and refuses an existing bootstrap file, key or database
([Commands](installation.md#commands)).

The admin secret is generated and printed once, on stdout, as `PHOEBIS_ADMIN_SECRET=<secret>`, with
`the admin secret is in PHOEBIS_ADMIN_SECRET, printed once on stdout` and `next: phoebis check-config --config {path}`
on stderr. Copy it now: the bootstrap file names the variable and never holds the secret, and the console's login is
this value. Its shape, and the line the router refuses another shape with, are on
[Environment variables](installation.md#environment-variables).

## Validate and start

```sh
export PHOEBIS_CONFIG=$HOME/.config/phoebis/config.yaml
export PHOEBIS_ADMIN_SECRET=<the printed secret>
phoebis check-config
phoebis
```

`PHOEBIS_CONFIG` names the bootstrap file for every `phoebis` call in that shell when `--config` is absent, and the
flag wins when both are given. `check-config` validates the file, the key file, the registry's key canary, the
registry's and the ledger's schema versions and the certificate pairs, taking no lock and opening no socket, and
prints `{config path} is valid` first. On a fresh install its registry line reads
`registry at {config.storage.path} does not exist yet, and the first start creates it`, and the rest of its lines are
on [Commands](installation.md#commands).

`phoebis` with no subcommand serves the three listeners until Ctrl-C or SIGTERM, and logs `listening` with the
`proxy`, `admin` and `metrics` URLs and the `profile` once every listener is bound. Without the secret in its
environment the router refuses to start with `environment variable {name} is unset or empty`
([Environment variables](installation.md#environment-variables)). Leave the router running and use a second shell for
what follows.

## First run

Six steps in the console, in this order.

1. Open `http://127.0.0.1:8081/` in the browser, enter the secret `init` printed under Admin secret and choose Log in.
   A wrong value answers `That secret does not match.` ([The console](console.md)).
2. Pair an account. Open Accounts and choose Add account. Pick "OAuth login", give the account a label and choose Start
   pairing. The pairing dialog runs two steps, Log in and paste the address, then Confirm: open the authorization link,
   sign in to Claude.ai and approve the router, paste the address the browser lands on, then confirm the email and
   organization shown. A pairing expires ten minutes after it starts. The account appears in the list with the toast
   `Paired {email}` once it is confirmed ([Through the console](pairing.md#through-the-console)).
3. Add an Anthropic key, as well or instead. Choose Add account again, pick "API key", give it a label, paste the key
   under API key and choose Add API key. The dialog's subtitle says which kind does what: "Subscription logins share
   your Claude.ai quota. API keys bill per token against a Console workspace." The router verifies the key against the
   upstream before it stores it, so a rejected key answers `400 credential rejected by the upstream` and creates no
   account ([Two kinds of account](pairing.md#two-kinds-of-account)). The console toasts `API key added` once it is
   stored.
4. Create a pool. Open Pools and choose Create pool. Give it a Name, tick the account's checkbox in the Members table
   and choose Create pool. The dialog marks Keys "after saving", since a key needs its pool to exist. The toast reads
   `Pool created` ([Pools](routing.md#pools)).
5. Create a key. Open Keys and choose Create key. Give it a Name, pick the Pool and choose Create. The Key created
   dialog shows the secret once: "Copy the secret now. The router stores only a hash, so it cannot be shown again."
   Under it: "Set it as `CLAUDE_CODE_OAUTH_TOKEN` on the client. The Settings page has the full snippet."
   ([Keys](routing.md#keys)).
6. Set the client block. Open Settings. The Downstream client configuration card renders the block for this router
   with a Copy snippet button: put it in the `env` block of the client's settings file or export it in the shell
   that launches the client, and replace the token with the secret of step 5. The card's first line reads
   `ANTHROPIC_BASE_URL=http://router.example.com:8317`, the scheme and the port from the proxy listener and a
   placeholder for the host, since the router knows no name a client reaches it at. On this install the host is
   `127.0.0.1`, so the block is:

    ```sh
    ANTHROPIC_BASE_URL=http://127.0.0.1:8317
    CLAUDE_CODE_OAUTH_TOKEN=<the key's secret>
    CLAUDE_CODE_SUBSCRIPTION_TYPE=max
    _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
    CLAUDE_CODE_GATEWAY_HINT_HEADERS=1
    DISABLE_TELEMETRY=1
    CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1
    ```

    What each line changes in Claude Code is on [The environment block](client.md#the-environment-block).

## The first request

Start Claude Code from the shell that holds the block, or with the block in its settings file, and send a prompt. The
answer comes through the router. Open the console's Requests view: the newest row is that request, its Status reads
`200` and its Account names the account that served it ([The console](console.md)). Then read the key's own figures
from the proxy listener:

```sh
curl -sS -H "Authorization: Bearer <secret>" http://127.0.0.1:8317/v1/usage
```

`GET /v1/usage` answers the requests, tokens and estimated cost of the key that presents it over the last 24 hours and
the last 7 days, per model, with the key's label and pool, and nothing about the accounts
([Your key's usage](client.md#your-keys-usage)).

## Where next

[A second machine](second-machine.md) moves the proxy listener onto a LAN host serving several machines, with TLS on
it and one key per machine. [The client](client.md) explains every line of the block, what the proxy listener accepts
and the codes it refuses with. [Operations](operations.md) covers the backup, the storage key, `phoebis status` and
upgrades. [Pools, keys and routing](routing.md) describes every field of an account, a pool and a key, and where a
request lands and why.
