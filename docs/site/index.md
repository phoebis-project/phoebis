# Phoebis Router

Phoebis Router relays Claude Code's requests to Anthropic under the credentials of an account it holds, and picks that
account from a pool: a session sticks to one account, a rate limit fails over to a sibling, prompt caches are kept
warm, and every request lands in a ledger the web console reads. Phoebis is a genus of sulphur butterflies, migrants
of the Americas that land on the right flower. If you are setting a router up for the first time, start with the
[Quick start](quickstart.md). The rest of this page is the three listeners, the path a request takes through them, the
words the other pages use, what the router does not do and where each page fits.

## The three listeners

One `phoebis` process serves three listeners.

| Listener | Bootstrap key | Serves |
| --- | --- | --- |
| Proxy | `proxy.listen` | `/v1/messages` and its companions, the key's own usage page, plus the keepalive route |
| Admin | `admin.listen` | The console and `/admin/*` (see [API reference](api-reference.md)) |
| Metrics | `stats.metrics_listen` | `/metrics` in Prometheus text format, no credential, plain HTTP |

The proxy and the admin listener terminate TLS natively or run plain HTTP by their `tls` key. The metrics listener is
plain HTTP ([TLS on the listeners](installation.md#tls-on-the-listeners)).

The proxy listener answers `HEAD /api/hello`, `HEAD` and `GET /api/ready`, `POST /v1/messages`,
`POST /v1/messages/count_tokens`, `GET /v1/usage`, `GET /v1/models`, `GET /v1/models/{*id}` and
`POST /v0/keepalive/events` ([What the proxy accepts](client.md#what-the-proxy-accepts)).

## How a request travels

The client sends `/v1/messages` to the proxy listener with a key the router issued, in the credential header it would
have used for Anthropic. The router authenticates the key, resolves its pool and selects one account: the session's
pin when it has one, otherwise the pool's strategy ([Pools, keys and routing](routing.md)). It puts the account's
credential where the client put the key, edits the body and the headers as [The relay](relay.md) lists them, and
sends the request in the client's own header order and spelling to `api.anthropic.com`. The response head is
classified before a byte reaches the client, so a 429 on one account fails over to a sibling inside the same call.
The request, each attempt and the usage land in the ledger.

## The vocabulary

The console, the API, the bootstrap file and this site call what the router holds an account and a key the router issues
a key. The console and this site call the account kind whose credential is an Anthropic API key "API key". The API's
kind value is `api_key`. "Upstream" names the provider's host and the party that answered a request (the bootstrap
file's `upstream` block, the `upstream_error` outcome and the `upstream_unreachable` code, the `upstream_message_id`,
`upstream_request_id` and `upstream_status` fields, the `phoebis_upstream_ttfb_seconds` metric). The bootstrap file, the
one `init` writes, names two database files: the registry at `storage.path` and the ledger at `stats.ledger_path`. This
site calls the client Claude Code and the plugin that reports to the router the keepalive mod.

## What the router does not do

- Claude Code's Remote Control is unavailable behind the router.
- No upstream other than Anthropic's API. The bootstrap file's `upstream` block adds only a lab's trust anchor and
  resolver override, never another host.

## Where to start

The [Quick start](quickstart.md) is for the person with one machine who wants Claude Code answering through the router
on loopback. [The client](client.md) is for the person pointing Claude Code at the router and for the key holder. Once
a loopback router works, [A second machine](second-machine.md) covers a LAN host serving several machines.
[Installation](installation.md) is for the operator installing on a host or in the container and for the one who
needs every setting. [Pairing an account](pairing.md) is for the operator adding an account in the console or by
`curl`. An operator tuning pools and keys, or reading why a request landed where it did, reads
[Pools, keys and routing](routing.md). [Keepalive](keepalive.md) is for the operator turning cache warming on, and
[The keepalive mod](keepalive-mod.md) for the one installing and configuring the mod on a client machine.
[The relay](relay.md) says what the router changes on the wire and sends on its own. [The console](console.md) is for
the operator reading or using a view. [Statistics](statistics.md) is for the operator reading a figure's meaning or
alerting on a gauge. [Operations](operations.md) is for the operator running the router for months.
[Development](development.md) is for the contributor. The [API reference](api-reference.md) is for the operator
scripting the admin API.

## License

Phoebis Router is released under the MIT license. The text is in
[`LICENSE`](https://github.com/daystrom-labs/phoebis/blob/main/LICENSE) on GitHub and in the container image at
`/usr/share/doc/phoebis/LICENSE`.
