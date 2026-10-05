# Phoebis Router

A router for Claude Code.

Every request lands on the right account. Phoebis is a genus of sulphur butterflies, migrants of the Americas that land
on the right flower. A client points at `phoebis` with a key the router issued; the router puts each request back on the
wire in the shape Claude Code 2.1.269 sends it, under the credentials of an account it holds itself, and picks that
account from a pool. Three listeners make up one process: the proxy that relays `/v1/messages`, the admin API with the
embedded console, and a metrics endpoint in Prometheus text format.

| Page | What it covers |
| --- | --- |
| [Installation](installation.md) | The build, the service, the commands, and every bootstrap setting. |
| [Downstream client](client.md) | The client's environment block and what each variable changes. |
| [Pairing an account](pairing.md) | Pairing an OAuth account through the console or with `curl`. |
| [Pools, keys, and routing](routing.md) | Pace, load shedding, cooldowns, failover, and overage. |
| [Keepalive](keepalive.md) | Keeping prompt caches warm: the companion, the gates, the tuning fields, the cost. |
| [Statistics](statistics.md) | The ledger, the plan meter, the views, metrics, the live feed, and the gauges. |
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
| Proxy | `proxy.listen` | `/v1/messages` and its companions, the key's own usage page, plus the keepalive routes |
| Admin | `admin.listen` | The console and `/admin/*` |
| Metrics | `stats.metrics_listen` | `/metrics` in Prometheus text format, no credential, plain HTTP |

The proxy listener answers `HEAD /api/hello`, `HEAD` and `GET /api/ready`, `POST /v1/messages`, `POST
/v1/messages/count_tokens`, `GET /v1/usage` for the key's own usage, the two keepalive companion routes, and `GET
/v1/models` with the caller's own routable models ([models listing](client.md#the-models-listing)).

## The console

The admin listener serves a React console over the same routes as the admin API. The top bar carries the theme
toggle, and the sidebar carries eleven views.

![The dashboard: the attention line, four tiles, the capacity-left rings, recent requests][dl]
![The dashboard: the attention line, four tiles, the capacity-left rings, recent requests][dd]

[dl]: assets/dashboard-light.png#only-light
[dd]: assets/dashboard-dark.png#only-dark

The sidebar shows the views in this order:

| View | Describes |
| --- | --- |
| Dashboard | The attention line, 24-hour tiles, pooled capacity left weighed by tier, recent requests and sessions. |
| Observe | The live feed (see [Statistics](statistics.md)). |
| Quota | Each account's usage windows and reset timing (see [Statistics](statistics.md)). |
| Accounts | Accounts and their credentials (see [Pairing an account](pairing.md)). |
| Pools | Pools and their members (see [Pools, keys, and routing](routing.md)). |
| Keys | Downstream keys and the pool each routes into (see [Pools, keys, and routing](routing.md)). |
| Models | A key's model rules, cooldowns, and rejections (see [Pools, keys, and routing](routing.md)). |
| Requests | Individual relayed requests (see [Statistics](statistics.md)). |
| Analytics | Hourly rollups and cost (see [Statistics](statistics.md)). |
| Sessions | Session pins and keepalive recipes (see [Keepalive](keepalive.md)). |
| Settings | Client setup, the price table, and the router's version and paths (see [Installation](installation.md)). |

The console, the API, the bootstrap file and this site call what the router holds an account and a key the router
issues a key. The console and this site call the account kind whose credential is an Anthropic API key "Anthropic key";
the API's kind value is `api_key`. "Upstream" names the provider's host and the party that answered a request (the
bootstrap file's `upstream` block, the `upstream_error` and `upstream_unreachable` codes, the `upstream_message_id`,
`upstream_request_id` and `upstream_status` fields, the `phoebis_upstream_ttfb_seconds` metric).

## What this build does not do

- Claude Code's Remote Control is unavailable behind the router.
- The listeners speak HTTP/1.1 and terminate no TLS; a reverse proxy does that in front.

## Status

Nothing in this build has talked to Anthropic yet. The wire shapes come from a capture of Claude Code 2.1.269 on
Linux x64 taken on 2026-09-13, and every test runs against a loopback fake upstream that replays those fixtures.
Until a live run against `api.anthropic.com` happens, treat the relayed shape as verified against the capture
only.
