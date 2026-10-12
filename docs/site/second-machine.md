# A second machine

Your loopback router works and you want one LAN host serving Claude Code on several machines. This page covers the
shape, the proxy listener on the LAN, TLS on it, one key per client machine with the base URL each machine sets, the
console reached from your own machine, and the other clients the router serves.

## The shape

One host runs the router. Its proxy listener binds a LAN address, with a certificate pair or without one, and every
client machine points Claude Code at it with a key of its own. One pool can serve them all: the pool holds the
accounts, and each machine's key routes into it. The admin listener stays on loopback, and you reach the console from
your own machine through a tunnel. The metrics listener stays on `127.0.0.1:9464` as `init` wrote it, plain HTTP with
no `tls` key. On the host the router runs as a service. A client machine holds nothing of the router but the client
block with the LAN address and its key.

## The proxy listener on the LAN

The proxy listener's address is `proxy.listen` in the bootstrap file, `127.0.0.1:8317` as `init` writes it when no
flag names another ([proxy](installation.md#proxy)). The router host runs `init` once. A fresh host passes
`--proxy-listen 0.0.0.0:8317` on the `init` line, or its own LAN address in place of `0.0.0.0`. A host whose
loopback router already works edits `proxy.listen` in its own file and restarts, since `init` refuses an existing
bootstrap file with `{path} already exists, and init never overwrites a configuration`, and the start binds the
address once. A client machine runs no `init`. On a terminal, `init` with a listener off loopback and no pair asks
`{name} on {listen} leaves loopback. tls: files, or none for plain HTTP [none]: `, and `--non-interactive` takes
plain HTTP without asking ([Commands](installation.md#commands)).

The edit on the working host is the one key:

```yaml title="config.yaml"
proxy:
  listen: 0.0.0.0:8317
```

Run `phoebis check-config` after the edit or the `init`: it prints `proxy listens on {config.proxy.listen} over plain
HTTP`, or `proxy listens on {addr} over TLS (certificate {cert_file}, expires {date})` with a pair, and the start's
`listening` line names each listener's scheme and address the same way.

## TLS on the proxy

A pair you hold terminates TLS on the proxy listener: `--proxy-cert PATH` with `--proxy-key PATH` on the `init` line,
each requiring the other, or the `tls` key under `proxy` in the file, a mapping with `cert_file` and `key_file`. The
certificate is PEM with the leaf first, the key the matching private key in PEM. `init` does not read the pair.
`check-config` and the start do. The router generates no certificate, and a Tailscale certificate, one from a LAN CA
or one an ACME client renews all serve ([TLS on the listeners](installation.md#tls-on-the-listeners)).

Without a pair the listener serves plain HTTP on the address named, loopback or not, and no address requires a pair.
On a network address that sends in the clear what a certificate would cover, which on the proxy is the client's key on
every request. That is your trust decision for that network, and the file records it by naming no pair.

A certificate from a public CA needs nothing on the clients. A certificate from a private CA (a LAN CA, a Tailscale
certificate's issuer, a CA you run) needs each client machine to trust that CA: copy its certificate there and set
`NODE_EXTRA_CA_CERTS=<path>` beside `ANTHROPIC_BASE_URL` in the client's block. The client checks the CA before
anything else, so the leaf needs no pin.

## One key per machine

Create one key per client machine on the Keys view, as the Quick start's [First run](quickstart.md#first-run) creates
the first, named for the machine ([Keys](routing.md#keys)). Each key has its own secret, shown once in the Key created
dialog with "Copy the secret now. The router stores only a hash, so it cannot be shown again.", its own usage page,
since `GET /v1/usage` on the proxy listener answers the figures of the key that presents it and of no other
([Your key's usage](client.md#your-keys-usage)), and its own rows on the Requests view, where the Key column names
it.

Revoking a key cuts off that machine and no other, since every other machine presents a key of its own. Its session
pins are its own as well: moving a key to another pool ends its current pins and no other key's, and the Pools view
counts the pins under the pool's keys on each account.

Each machine's block is the Settings card's with its own secret in `CLAUDE_CODE_OAUTH_TOKEN` and the first line's
placeholder host replaced by the name that reaches the listener from that machine. The card derives the scheme from
whether the proxy listener terminates TLS and the port from the address it binds, so the first line reads
`ANTHROPIC_BASE_URL=https://<host>:8317` with a pair and `ANTHROPIC_BASE_URL=http://<host>:8317` without. The scheme
and the port change too where a TLS proxy or a port mapping stands between Claude Code and the listener
([The environment block](client.md#the-environment-block)). The scheme has to match the listener: a plain-HTTP
request to a listener that terminates TLS gets no HTTP answer, since the handshake fails and the connection closes,
and `curl` reports `curl: (35)` ([TLS on the listeners](installation.md#tls-on-the-listeners)).

## The console from your machine

The admin listener stays on `127.0.0.1:8081` as `init` wrote it, and you reach the console through an SSH tunnel from
your own machine:

```sh
ssh -L 8081:127.0.0.1:8081 <host>
```

Then open `http://127.0.0.1:8081/` in the browser on your machine. The listener issues its session cookie without
`Secure` on plain HTTP, which costs nothing behind the tunnel. The alternative is the admin listener on the LAN
with a pair of its own: `--admin-listen` with `--admin-cert` and `--admin-key` on the `init` line, or `admin.listen`
and the `tls` key under `admin` in the file ([admin](installation.md#admin)). A plain admin listener on a network
address sends the admin secret at login and the session cookie on every call in the clear, so a LAN address on it
wants the pair.

On the host the router runs under the service unit of [Install as a service](installation.md#install-as-a-service),
as the `phoebis` user, with the secret in the environment file `init --env-file` writes.

## Other clients

The Desktop app reads the same settings file as the terminal, so the client block in that file's `env` block reaches
it on macOS and Windows, which is untested. The keepalive mod runs wherever Claude Code loads the plugin: the Agent
SDK, `claude -p`,
a cloud session that carries the plugin, the Desktop app, the terminal and the VS Code extension's chat panel. With
its options unset it reports to `ANTHROPIC_BASE_URL` under the block's key, so the block needs no line of its own for
it, and its install lines are on [The keepalive mod](keepalive-mod.md).
