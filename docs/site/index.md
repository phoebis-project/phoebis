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
