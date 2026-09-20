# phoebis

A local relay for Claude Code. A downstream client points at `phoebis` with a key the router issued; `phoebis` puts the
request on the wire to Anthropic, under credentials it holds itself, and picks
those credentials from a pool of accounts with sticky sessions, quota-driven failover, prompt-cache keepalive,
per-request statistics, and a web console. One process serves three listeners: the proxy that relays
`/v1/messages`, the admin API with the embedded console, and a metrics endpoint in Prometheus text format.

The documentation lives under [`docs/site/`](docs/site/index.md) and is published at
<https://daystrom-labs.github.io/phoebis/> once GitHub Pages is enabled for the repository
([how](docs/site/operations.md#publishing-this-site)).

## Build and run

```sh
cargo build --release
install -d -m 0700 ~/.config/phoebis
[ -e ~/.config/phoebis/storage.key ] ||
  (umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > ~/.config/phoebis/storage.key)
sed -e "s#/run/secrets/router-storage-key#$HOME/.config/phoebis/storage.key#" \
  -e 's/dev_loopback_http: false/dev_loopback_http: true/' -e '/^  origin:/d' \
  deploy/config.example.yaml > config.yaml
export ROUTER_ADMIN_SECRET=$(openssl rand -base64 36) ROUTER_VIEWER_SECRET=$(openssl rand -base64 36)
target/release/phoebis --config config.yaml
```

The key is created once and never replaced: every stored credential is sealed under it and there is no re-key.
That runs the proxy on `127.0.0.1:8317`, the admin listener with the console on `127.0.0.1:8081`, and metrics on
`127.0.0.1:9464`, with both databases under `./data/`. The build embeds the console and needs Node 20 or newer;
`PHOEBIS_SKIP_UI_BUILD=1` builds without it. `phoebis check-config --config config.yaml` validates a file without opening
the store or a socket. Installing as a service, and every setting of the file, is on the
[installation page](docs/site/installation.md).

## Where things are

| Page | What it covers |
| --- | --- |
| [Installation](docs/site/installation.md) | The build, the service, the commands, and every bootstrap setting. |
| [Downstream client](docs/site/client.md) | The client's environment block, each variable, and the key's usage page. |
| [Pairing an account](docs/site/pairing.md) | Pairing an OAuth account through the console or with `curl`. |
| [Pools, keys, and routing](docs/site/routing.md) | Accounts, pools, keys, selection, cooldowns, failover, overage. |
| [Keepalive](docs/site/keepalive.md) | Keeping prompt caches warm: the companion, the gates, the tuning, the cost. |
| [Statistics and alerts](docs/site/statistics.md) | The ledger, the views, metrics, the live feed, webhook alerts. |
| [Operations](docs/site/operations.md) | Backup, the storage key, upgrades, re-pinning a client build, publishing. |
| [Changelog](docs/site/changelog.md) | What changed for an operator, by date. |

The browser pass over the console is in
[web/e2e/README.md](web/e2e/README.md).

## Status

Nothing in this build has talked to Anthropic yet. The wire shapes come from a capture of Claude Code 2.1.269 on
Linux x64, and every test runs against a loopback fake upstream that replays those fixtures. A first live run
against `api.anthropic.com` is still ahead, and until it happens, treat the relayed shape as verified against the
capture only.
