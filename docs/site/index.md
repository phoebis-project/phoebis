# Phoebis Router

`phoebis` is a local relay for Claude Code. A client points at it with a key the router issued; the router puts each
request on the wire to Anthropic, under the credentials of an account it holds
itself, and picks that account from a pool. Three listeners make up one process: the proxy that relays
`/v1/messages`, the admin API with the embedded console, and a metrics endpoint in Prometheus text format.

| Page | What it covers |
| --- | --- |
| [Installation](installation.md) | The build, the service, the three commands, and every bootstrap setting. |
| [Downstream client](client.md) | The client's environment block and what each variable changes. |
| [Pairing an account](pairing.md) | Pairing an OAuth account through the console or with `curl`. |
| [Pools, keys, and routing](routing.md) | Accounts, pools, keys, selection, cooldowns, failover, and overage. |
| [Keepalive](keepalive.md) | Keeping prompt caches warm: the companion, the gates, the tuning fields, the cost. |
| [Statistics and alerts](statistics.md) | The ledger, the views, metrics, the live feed, and webhook alerts. |
| [Operations](operations.md) | Backup, the storage key, upgrades, re-pinning a client build, publishing. |
| [Changelog](changelog.md) | What changed for an operator, by date. |

## How a request travels

The client sends `/v1/messages` to the proxy listener with the router key in the credential header it would have
used for Anthropic. The router authenticates the key, resolves its pool, and selects one account: the session's
pin when it has one, otherwise the pool's strategy (see [Pools, keys, and routing](routing.md)). It puts the
account's credential where the client put the key, adjusts `anthropic-beta` to what the account may claim, and
sends the request in the client's own header order and spelling to `api.anthropic.com`.
The response head is classified before a byte reaches the client, so a 429 on one account fails over to a
sibling inside the same call. The request, each attempt, and the usage land in the ledger.

## The three listeners

| Listener | Bootstrap key | Serves |
| --- | --- | --- |
| Proxy | `proxy.listen` | `/v1/messages` and its companions, plus the keepalive routes |
| Admin | `admin.listen` | The console and `/admin/*` |
| Metrics | `stats.metrics_listen` | `/metrics` in Prometheus text format, no credential, plain HTTP |

The proxy listener answers `HEAD /api/hello`, `POST /v1/messages`, `POST /v1/messages/count_tokens`, and the two
keepalive companion routes. A `GET /v1/models` on it answers 404 on purpose: model discovery would invite a
client to pick a model from an account's entitlements rather than from its own configuration.

## The console

The admin listener serves a React console over the same routes as the admin API. The top bar carries the theme
toggle and the role badge, and the sidebar carries eleven views.

![The dashboard: four tiles, the remaining-quota cards, recent requests](assets/dashboard-light.png#only-light)
![The dashboard: four tiles, the remaining-quota cards, recent requests](assets/dashboard-dark.png#only-dark)

The sidebar shows the views in this order:

| View | Describes |
| --- | --- |
| Dashboard | Shown above: 24-hour tiles, remaining subscription quota, and recent requests and sessions. |
| Observe | The live feed (see [Statistics and alerts](statistics.md)). |
| Quota | Each account's usage windows and reset timing (see [Statistics and alerts](statistics.md)). |
| Upstreams | Accounts and their credentials (see [Pools, keys, and routing](routing.md)). |
| Pools | Pools and their members (see [Pools, keys, and routing](routing.md)). |
| API keys | Downstream keys and the pool each routes into (see [Pools, keys, and routing](routing.md)). |
| Request logs | Individual relayed requests (see [Statistics and alerts](statistics.md)). |
| Analytics | Hourly rollups and cost (see [Statistics and alerts](statistics.md)). |
| Sessions | Session pins and keepalive recipes (see [Keepalive](keepalive.md)). |
| Alerts | Webhook alert rules and their events (see [Statistics and alerts](statistics.md)). |
| Settings | The effective bootstrap values (see [Installation](installation.md)). |

The console calls an account an "upstream" everywhere the API and the bootstrap file call it an "account"; the
rest of this site keeps to "account".

## What this build does not do

- Claude Code's Remote Control is unavailable behind the router.
- Model discovery over the proxy answers 404.
- The listeners speak HTTP/1.1 and terminate no TLS; a reverse proxy does that in front.

## Status

Nothing in this build has talked to Anthropic yet. The wire shapes come from a capture of Claude Code 2.1.269 on
Linux x64 taken on 2026-09-13, and every test runs against a loopback fake upstream that replays those fixtures.
Until a live run against `api.anthropic.com` happens, treat the relayed shape as verified against the capture
only.
