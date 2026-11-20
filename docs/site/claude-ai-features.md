# claude.ai features

This page is for the operator who wants Artifacts and the claude.ai connectors for a pool's users, and for the user who
starts Claude Code through the launch script. The client sends the calls of these two features to two fixed hosts,
`api.anthropic.com` and `mcp-proxy.anthropic.com`, whatever `ANTHROPIC_BASE_URL` names, with the router key as their
bearer. The upstream answers a router key with `401`, so behind a base URL alone both features fail. A router with a
feature listener serves those calls itself: it ends the TLS for the two host names under a CA of its own, checks the
router key, and forwards each call of the two features under the sign-in of an account the operator selected. The page
covers what the features take on the router and on the client's machine, the launch script, what the router forwards and
refuses, the records, and the CA over time.

## What works and what it takes

Two features work behind the router. With Artifacts, the client's Artifact tool publishes a page to claude.ai, lists the
pages and reads one back, and the client can delete a page and change who sees it. With the claude.ai connectors, the
client reads an account's connector list and opens an MCP session with a connector.

Both take three things:

1. On the router, the `features` block, which starts the feature listener and creates the CA ([The router's
   side](#the-routers-side)).
2. For each feature, an account the operator selects for the pool or for one key ([Whose claude.ai a key acts
   under](#whose-claudeai-a-key-acts-under)).
3. On the client's machine, one of the two feature setups below, started through [the launch
   script](#the-launch-script).

The setups carry the names of the Setup switch on the console's client configuration card
([Settings](console.md#settings)):

| Setup | What leads the two hosts to the router | The two features |
| --- | --- | --- |
| "Base URL" | nothing | fail, since the client sends their calls to the upstream with the router key |
| "Base URL and proxy" | `phoebis-proxy`, a tunnel the client reaches through `HTTPS_PROXY` | work |
| "Base URL and DNS redirect" | the machine's name resolution, which answers both names with the router's address | work |

In each setup `ANTHROPIC_BASE_URL` stays at the router's proxy listener, so a model request reaches the router as [The
client](client.md) describes it. In the two feature setups the client opens TLS toward a feature host's name, the router
answers with a certificate of its feature CA for that name, and the client trusts that CA through `NODE_EXTRA_CA_CERTS`.
The router key travels inside that TLS to the router and to no upstream host, and the client holds no upstream
credential.

## Whose claude.ai a key acts under

Make this choice before the setup. A feature call runs under the claude.ai sign-in of the account selected for the
feature, whichever account serves the key's model requests. Each key whose selection names an account publishes pages
into that account, can delete a page there and change who sees it, and reaches the account's connectors and the data
behind them. Select an account whose content the pool's users may share with each other. The list of feature calls shows
which key sent which call under the account ([The records](#the-records)).

The selection is one account or none per feature, for Artifacts and for the connectors apart. A pool holds one selection
per feature, and a new pool selects none, which refuses the feature's calls. A key follows its pool, which is a new
key's choice, or overrides the pool per feature with an account or with none. The account may be a member of the pool, a
member of another pool or in no pool, and one account can serve several pools and keys. Each OAuth account of the router
can be selected. An API-key account cannot, since it holds no claude.ai sign-in.

The router uses no stand-in. Where the selected account cannot serve ([What the account's state
does](#what-the-accounts-state-does)), the call is refused, and the router tries no other account and moves no selection
by itself.

## The router's side

The `features` block of `config.yaml` holds one key, `listen`. With it the router binds a fourth listener, the feature
listener, which serves the feature calls and nothing else. Without the block it binds none, creates no CA and serves no
feature call. [features](installation.md#features) on the installation page holds the operator's steps, and these pages
give the listener port 8443. A client behind `phoebis-proxy` needs the listener's own port, reachable from the proxy's
machine. A redirect needs port 443 at the address the two names resolve to, bound there or mapped to it ([Port 443 for a
DNS redirect](installation.md#port-443-for-a-dns-redirect)).

The first start with the block creates the feature CA, two files in the data directory. Its certificate carries a
critical name constraint, which permits the DNS names `api.anthropic.com` and `mcp-proxy.anthropic.com` with the names
beneath them and no other DNS name, and which excludes each IP address. A client that trusts the CA therefore accepts
its certificates for the two feature hosts and for no other name, and that bounds what the CA's key can do in the hands
of someone who copies it. The key lives in `feature-ca-key.pem` in the data directory at mode `0600`. No route serves
it, no log line prints it and no subcommand exports it. The certificate is public: the proxy listener serves it at
`GET /v0/features/ca.pem` without a key, and the launch script fetches it there.

The fingerprint of the CA is the SHA-256 of the bytes that download serves, in lowercase hex. The operator hands it to
each user beside the base URL and the key, and the launch script compares it at each start. The router shows it in these
places:

- `check-config`, in its `features` line
- `phoebis status`, in the line `feature CA {fingerprint}, expires {date}` under the listeners ([Is it
  up?](operations.md#is-it-up))
- the console's Settings view, as "Feature CA fingerprint" with "Copy fingerprint" ([Settings](console.md#settings))
- the start's log, in the feature listener's `serving TLS` line, and at the first start in the `feature CA created` line

The operator selects the account of each feature in the pool dialog's group "claude.ai features"
([Pools](console.md#pools)) and overrides it for one key in the key dialog's group of the same name
([Keys](console.md#keys)). Each feature needs one scope in the account's stored sign-in, which [Two kinds of
account](pairing.md#two-kinds-of-account) names: the dialogs list an account without it as disabled, and a new pairing
adds it. The router tests no scope when it forwards, so a sign-in without the scope gets the upstream's own refusal,
which goes back to the client.

## Which setup fits

"Base URL and proxy" fits where other programs of the machine need the two hosts themselves, since a proxy variable is
set per process, or where the machine's name resolution is not yours to change. The proxy also keeps the client's
traffic to other hosts off the router: it tunnels each host apart from the two to that host itself.

"Base URL and DNS redirect" fits a machine, a container or a virtual machine whose programs reach the two hosts through
the router alone. The redirect holds for each program of the machine, and it needs no process beside the client.

## Base URL and proxy

`phoebis-proxy` is a small program in [a repository of its own](https://github.com/phoebis-project/phoebis-proxy), whose
pages on this site hold the [install](../proxy/install.md) from a release archive or from the image
`ghcr.io/phoebis-project/phoebis-proxy`, the [settings](../proxy/settings.md) and the [log lines](../proxy/logs.md). It
serves one kind of request, `CONNECT`, over plain HTTP, by default on `127.0.0.1:8318`. A `CONNECT` for
`api.anthropic.com` or `mcp-proxy.anthropic.com` on port 443 ends at the proxy's upstream, which is the router's feature
listener, and a `CONNECT` for each other target ends at that target. The proxy holds no router key, no account and no
CA. It ends no TLS and reads no byte of a tunnel, so the client's TLS session ends at the router and the router key
stays inside it.

The upstream is the host and the port of the feature listener as the proxy's machine reaches them, such as
`router.example.com:8443`, set through `--upstream` or `PHOEBIS_PROXY_UPSTREAM`. It names the listener's own port, so
nothing in this setup needs port 443.

On the client's machine the proxy listens on loopback and serves that machine's users, which is its default and the
default of the launch script's `PROXY_URL`. On another machine or in a container it listens beyond loopback, and there
it starts only with an allow list of source networks (`--allow` or `PHOEBIS_PROXY_ALLOW`), since a proxy that tunnels
each host is an open forward proxy for whoever reaches its port. Beside the router in one compose project its upstream
is the router's service name with the listener's port, and [phoebis-proxy](installation.md#phoebis-proxy) on the
installation page names the service the compose file carries for it.

Between a client and a proxy on another machine the `CONNECT` lines cross the network in the clear: the host name and
the port of each HTTPS call the client sends through the proxy. A feature call's router key crosses inside the client's
TLS, which the proxy cannot read.

In this setup the launch script sets `HTTPS_PROXY` to the proxy's address, so the client sends its HTTPS calls through
the proxy, the two feature hosts among them. The model requests go to the base URL's host directly, since the script
names that host in `NO_PROXY`.

## Base URL and DNS redirect

The machine's name resolution answers both feature hosts with the address of the router's feature listener, for IPv4 and
for IPv6. In a hosts file that is one line per address family, each with both names:

```text
192.0.2.10    api.anthropic.com mcp-proxy.anthropic.com
2001:db8::10  api.anthropic.com mcp-proxy.anthropic.com
```

The second line is required. A hosts file with the IPv4 line alone leaves the name's IPv6 answer to the resolver, and a
client that then connects over IPv6 reaches the upstream itself and sends it the router key. On Linux with glibc a
lookup for both families answers the IPv4 address alone from such a file, and a lookup for IPv6 alone goes to the
resolver. A resolver of the network that answers both names for both families does for each machine behind it what the
two lines do for one.

Where the router has no IPv6 address, write the second line with the IPv4-mapped form of its IPv4 address,
`::ffff:192.0.2.10` for the address above. The measured case is the router's listener on the client's own machine, with
the lines `127.0.0.1` and `::ffff:127.0.0.1`, on Linux with glibc and curl 8.14.1: each name resolved to both addresses,
a connection over IPv6 reached the IPv4 listener through the mapped address, and the launch script's check passed.

The client connects to a feature host on port 443 and to no other port, so the feature listener answers on port 443 of
that address. [Port 443 for a DNS redirect](installation.md#port-443-for-a-dns-redirect) on the installation page holds
the road for each kind of install.

A hosts file and a resolver know no program. Each program of the machine that asks for one of the two names reaches the
router: a client with a login of its own, a tool that calls the Messages API with a key of its own, a command of the
client's Bash tool. A program that trusts the system's store alone gets a certificate error, and one that trusts the
feature CA gets the router's `401`, since its credential is no router key. On Linux a private mount namespace gives one
process tree a hosts file of its own. macOS and Windows have no such file per process.

A machine whose lines are missing or incomplete would send the feature calls to the upstream with the router key, as the
"Base URL" setup does. The launch script's check stops the start where one of the two names leads to another host over
an address family the machine has a road over at that moment, so start the client through the script: the card's lines
alone run no check. The check cannot tell one case from a sound machine: the line of one family is missing, and the
machine has no road over that family while the script runs ([The fetch, the compare and the
check](#the-fetch-the-compare-and-the-check)). Where the machine gains such a road inside the session, through a changed
network or a tunnel that comes up, a new connection of the client to a feature host can leave to the upstream with the
router key.

The script sets no proxy variable in this setup and clears the shell's proxy names for the client, since a proxy of the
shell would resolve the two names on its own machine.

## The launch script

The script below starts the client in either feature setup. Save it to a file, set the seven inputs at its head, and
start the client through it in place of `claude`: each argument goes on to the client. The file then holds the key's
secret, so keep it readable by its user alone, at mode `0700`. At each start it fetches the CA's certificate, compares
its fingerprint, checks the road to both feature hosts, writes the env file for Bash commands, sets the client's
variables and starts the client. It is a POSIX `sh` script, and it needs `curl` and one of `sha256sum` and `shasum`.

```sh
#!/bin/sh
# Starts Claude Code with Artifacts and the claude.ai connectors behind a Phoebis router.
set -eu

# What the router's operator hands over, and how this machine reaches the two feature hosts.
BASE_URL='http://router.example.com:8317'
ROUTER_KEY='<key>'
PHOEBIS_FEATURE_CA_SHA256='<fingerprint>'
SETUP='proxy'                      # proxy, or redirect
PROXY_URL='http://127.0.0.1:8318'  # read under SETUP=proxy alone
PLAN='max'                         # max, or pro
BASE_CA=''                         # the private CA of the base URL's certificate, where it has one

stop() { echo "phoebis: $1" >&2; exit 1; }

# An empty key would start the client with an empty token, and a client with a login of its own can then fall back
# to that login.
case "$ROUTER_KEY" in
  ''|'<key>') stop "ROUTER_KEY holds no key. Set it to the key the router's operator handed over" ;;
esac
case "$PHOEBIS_FEATURE_CA_SHA256" in
  ''|'<fingerprint>') stop "PHOEBIS_FEATURE_CA_SHA256 holds no fingerprint. Set it to the operator's" ;;
esac
case "$SETUP" in proxy|redirect) ;; *) stop "SETUP is neither proxy nor redirect" ;; esac
case "$PLAN" in max|pro) ;; *) stop "PLAN is neither max nor pro" ;; esac

state="${XDG_STATE_HOME:-$HOME/.local/state}/phoebis"
ca="$state/feature-ca.pem"
env_file="$state/claude-env.sh"
# Each name carries the process id, so two starts at one time share neither file.
new_ca="$ca.$$"
head_file="$state/check-head.$$"
# The script's files are the user's alone. The shell's umask returns ahead of the client's start, since under 077
# each file the session and its commands create would be made 0600.
shell_umask=$(umask)
umask 077
mkdir -p "$state"
trap 'rm -f "$new_ca" "$head_file"' EXIT
# Some shells run the exit trap for a signal only where the signal has a trap of its own.
trap 'exit 1' HUP INT TERM

# A closing slash would make the fetch ask for //v0/features/ca.pem, and the router's 404 for that path reads as
# a router without a feature listener.
BASE_URL=${BASE_URL%/}

# The fetch goes straight to the router, with no proxy in between.
rc=0
status=$(curl --silent --noproxy '*' --connect-timeout 10 --max-time 30 ${BASE_CA:+--cacert "$BASE_CA"} \
  --output "$new_ca" --write-out '%{http_code}' "$BASE_URL/v0/features/ca.pem") || rc=$?
[ "$rc" -eq 0 ] || stop "the router at $BASE_URL does not answer (curl exit $rc)"
[ "$status" != 404 ] || stop "the router at $BASE_URL has no feature listener"
[ "$status" = 200 ] || stop "the CA fetch from $BASE_URL answered $status"

# A certificate fetched over plain HTTP proves nothing until its fingerprint equals the operator's. The stop prints
# no fetched value, since a fingerprint pasted from here would turn the compare into trust in the first fetch.
if command -v sha256sum >/dev/null 2>&1; then sum=$(sha256sum "$new_ca"); else sum=$(shasum -a 256 "$new_ca"); fi
[ "${sum%% *}" = "$PHOEBIS_FEATURE_CA_SHA256" ] ||
  stop "the fetched CA does not have the operator's fingerprint. Get the fingerprint from the router's operator"

# One request without a key for one feature host, over the road the client will take. The router alone answers it
# with a 401 that names auth_missing in its code header, so that answer proves the peer under each curl build,
# whatever other certificates that build trusts beside the fetched one. The last header line of that name counts,
# since a proxy's own answer to the CONNECT stands ahead of it in the file.
probe() {
  url="https://$1/"
  shift
  : > "$head_file"
  rc=0
  codes=$(curl --silent --output /dev/null --dump-header "$head_file" --connect-timeout 10 --max-time 30 \
    --write-out '%{http_code} %{http_connect}' --cacert "$new_ca" "$@" "$url") || rc=$?
  code=$(tr -d '\r' < "$head_file" | grep -i '^x-phoebis-error-code:' | tail -n 1 | sed 's/^[^:]*: *//')
  rm -f "$head_file"
}

# Both hosts, since a redirect that leaves out the second would pass a check of the first, and each connector call
# would then carry the router key to the upstream.
for host in api.anthropic.com mcp-proxy.anthropic.com; do
  if [ "$SETUP" = proxy ]; then
    probe "$host" --proxy "$PROXY_URL" --noproxy ''
    # The proxy's status for the CONNECT picks the stop, since the exit of curl cannot: where a proxy refuses the
    # CONNECT, curl 8.14.1 exits 56 and curl 8.18.0 exits 7, which is the exit of both where no proxy listens.
    case "$rc:$codes:$code" in
      0:401\ *:auth_missing) ;;
      7:*\ 000:*) stop "no proxy runs at $PROXY_URL" ;;
      *:*\ 502:*) stop "the proxy at $PROXY_URL does not reach the router's feature listener" ;;
      0:*|35:*|51:*|60:*) stop "$host leads to another host than the router's feature listener" ;;
      *) stop "the check of the road to $host failed (curl exit $rc, answer $codes)" ;;
    esac
  else
    # One probe per address family, since a name with one family's line missing can leave the machine over the
    # other.
    routers=0
    for family in ipv4 ipv6; do
      probe "$host" "--$family" --noproxy '*'
      case "$rc:$codes:$code" in
        0:401\ *:auth_missing) routers=$((routers + 1)) ;;
        6:*|7:*) ;;
        0:*|35:*|51:*|60:*) stop "$host leads to another host than the router's feature listener over $family" ;;
        *) stop "the check of the road to $host over $family failed (curl exit $rc, answer $codes)" ;;
      esac
    done
    [ "$routers" -gt 0 ] || stop "nothing answers at $host:443 as this machine resolves it"
  fi
done
mv "$new_ca" "$ca"

# The client's variable takes one file, so a private CA of the base URL and the feature CA share one. The line break
# between them keeps the second BEGIN line off the first file's END line where that file ends without one.
trust="$ca"
if [ -n "$BASE_CA" ]; then
  trust="$state/ca-bundle.pem"
  { cat "$BASE_CA"; echo; cat "$ca"; } > "$trust"
fi

# Bash commands of the session get back what the shell held before this script changed it.
names='HTTPS_PROXY NO_PROXY https_proxy no_proxy'
[ "$SETUP" = proxy ] || names='ALL_PROXY HTTP_PROXY HTTPS_PROXY all_proxy http_proxy https_proxy'
: > "$env_file"
chmod 600 "$env_file"
for name in NODE_EXTRA_CA_CERTS $names; do
  if eval "[ \"\${$name+x}\" = x ]"; then
    value=$(eval "printf '%s' \"\$$name\"" | sed "s/'/'\\\\''/g")
    printf "export %s='%s'\n" "$name" "$value" >> "$env_file"
  else
    printf 'export %s=\nunset %s\n' "$name" "$name" >> "$env_file"
  fi
done

export ANTHROPIC_BASE_URL="$BASE_URL"
export CLAUDE_CODE_OAUTH_TOKEN="$ROUTER_KEY"
export CLAUDE_CODE_SUBSCRIPTION_TYPE="$PLAN"
export _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
export CLAUDE_CODE_GATEWAY_HINT_HEADERS=1
export DISABLE_TELEMETRY=1
export CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1
export CLAUDE_CODE_OAUTH_SCOPES='user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload'
export CLAUDE_ENV_FILE="$env_file"
export NODE_EXTRA_CA_CERTS="$trust"
if [ "$SETUP" = proxy ]; then
  # The base URL's host without its port, with an IPv6 literal whole inside its brackets.
  base_host=${BASE_URL#*://}
  base_host=${base_host%%/*}
  case "$base_host" in
    \[*) base_host="${base_host%%\]*}]" ;;
    *) base_host=${base_host%%:*} ;;
  esac
  export HTTPS_PROXY="$PROXY_URL"
  # The router's host and loopback alone. An entry of the shell that matches a feature host would send the
  # router key past the proxy to the upstream. Bash commands get the shell's value back through the env file.
  export NO_PROXY="$base_host,localhost,127.0.0.1,::1"
  # The client reads the lowercase names first, so a value of the shell would win over the two lines above.
  unset https_proxy no_proxy
else
  # A proxy of the shell would resolve the two names on its own machine and send the key to the upstream.
  unset ALL_PROXY HTTP_PROXY HTTPS_PROXY all_proxy http_proxy https_proxy
fi
umask "$shell_umask"
exec claude ${1+"$@"}
```

The script is checked on Linux under `dash` 0.5.12 as `sh`, on a host whose curl is 8.14.1 with OpenSSL 3.5.7. It ran
whole there in both setups, through to the client's start. Where a proxy refuses the `CONNECT`, curl's exit depends on
curl's version: measured with the statuses `403`, `407`, `502` and `503`, it is 56 under curl 8.5.0 and 8.14.1 and 7
under curl 8.18.0, and 7 is also the exit of the three versions where nothing listens at the proxy's address. Each of
them reports the proxy's status beside its exit, and `000` where nothing listens, so the script reads that status to
tell a proxy that does not run from one that does not reach the router's feature listener. The figures for curl 8.5.0
and 8.18.0 come from the probe's curl call against loopback stubs, and the script did not run whole under either. No run
on macOS stands behind this page yet, and two points are unread there: Apple's curl with the feature CA's name
constraint under `--cacert`, and what Apple's curl reports at the proxy's address, the exit where nothing listens and
the status of a refused `CONNECT`.

### The inputs

The console's client configuration card prints the lines of a feature setup with the router's fingerprint among them.
The script reads seven inputs and sets each line of the card by itself:

| Input | Holds | The card's line it stands for |
| --- | --- | --- |
| `BASE_URL` | the URL of the router's proxy listener | `ANTHROPIC_BASE_URL` |
| `ROUTER_KEY` | the key's secret | `CLAUDE_CODE_OAUTH_TOKEN` |
| `PHOEBIS_FEATURE_CA_SHA256` | the fingerprint of the router's feature CA, from the operator | `PHOEBIS_FEATURE_CA_SHA256`, as it stands |
| `SETUP` | `proxy` for "Base URL and proxy", `redirect` for "Base URL and DNS redirect" | none, it picks the card's block |
| `PROXY_URL` | the address of `phoebis-proxy`, read under `SETUP=proxy` alone | `HTTPS_PROXY` |
| `PLAN` | `max` or `pro` | `CLAUDE_CODE_SUBSCRIPTION_TYPE` |
| `BASE_CA` | the file of a private CA behind the base URL's certificate, empty where it has none | none |

The script writes each other line of the card, with the paths the card marks, and no line of the card goes under the
inputs. A pasted proxy or trust line would reach each Bash command of the session as the shell's own value.

Ahead of the fetch the script stops where `ROUTER_KEY` or `PHOEBIS_FEATURE_CA_SHA256` is empty or still reads its
placeholder, where `SETUP` is neither `proxy` nor `redirect`, and where `PLAN` is neither `max` nor `pro`. An empty key
would start the client with an empty token, and a client with a login of its own can then fall back to that login.

The script keeps its files under `${XDG_STATE_HOME:-$HOME/.local/state}/phoebis`, readable by the user alone:
`feature-ca.pem`, `claude-env.sh`, and `ca-bundle.pem` where two CAs share one file. It works under `umask 077` and puts
the shell's umask back ahead of the client's start, since under `077` each file the session and its commands create
would be made `0600`. The two files of a start in progress carry the process id in their names and go at each exit, so
two starts at one time share neither. Each curl call gives up after 10 seconds without a connection and after 30 seconds
in all, so a name that leads to an address which drops packets does not hold the start for the system's own timeout.

### The fetch, the compare and the check

**The fetch.** The script reads `GET /v0/features/ca.pem` from the base URL's host, over its direct connection to the
router with no proxy in between. Where `BASE_CA` names a file, curl trusts that file for the fetch.

**The compare.** The script takes the SHA-256 of the fetched bytes with `sha256sum`, or with `shasum -a 256` on a
machine without `sha256sum`, such as macOS, and stops unless it equals `PHOEBIS_FEATURE_CA_SHA256`. The fetch may run
over plain HTTP, and a certificate taken that way proves nothing until its fingerprint equals the one the operator
handed over. The stop prints no fetched value: a reader who saw it could paste it into the script, which would turn the
compare into trust in the first fetch. After an operator replaced the CA, each user gets the new fingerprint from the
operator and never from a fetch.

**The check.** For each of the two feature hosts the script sends one request without a key, `GET /`, over the road the
client will take, with the fetched certificate as curl's CA file. The router alone answers that request with a `401`
whose header `x-phoebis-error-code` reads `auth_missing`, and the upstream never sends that header, so the answer proves
the peer. A handshake that passes is no such proof, since curl's `--cacert` replaces the CA file and can leave other
trust sources of the build in place. The request carries no key, so a name that leads to another host costs no
credential. The script reads the last header line of that name, since a proxy's own answer to the `CONNECT` stands ahead
of it.

The check runs for both hosts. A redirect that leaves out `mcp-proxy.anthropic.com` would pass a check of
`api.anthropic.com` alone, and each connector call would then carry the router key to the upstream. Once both hosts
pass, the fetched file becomes `feature-ca.pem`.

Under `SETUP=proxy` one probe per host goes through the proxy, which resolves the name. Under `SETUP=redirect` two
probes per host go out, one per address family, since a name with one family's line missing can leave the machine over
the other. A host passes there where at least one probe reaches the router and none reaches another host or fails
otherwise, and a family over which nothing answers stops nothing while the other reaches the router. The check has then
not proved that family's line: without a road over a family, curl ends the same way for a line that holds and for a line
that is missing. Keep both lines in the hosts file whatever families the machine reaches at the start. A resolver of the
network that answers both names for both families serves the same end.

### The stops

A stop prints one line on standard error, behind `phoebis:`, and the client does not start. The messages, in the
script's order:

| Message | What it means |
| --- | --- |
| `ROUTER_KEY holds no key. Set it to the key the router's operator handed over` | The input is empty or still reads `<key>`. |
| `PHOEBIS_FEATURE_CA_SHA256 holds no fingerprint. Set it to the operator's` | The input is empty or still reads `<fingerprint>`. |
| `SETUP is neither proxy nor redirect` | The input holds another word. |
| `PLAN is neither max nor pro` | The input holds another word. |
| `the router at {BASE_URL} does not answer (curl exit {n})` | The fetch got no HTTP answer: a wrong base URL, a router that is down, or a certificate curl does not trust, for which `BASE_CA` names the private CA. |
| `the router at {BASE_URL} has no feature listener` | The download answered `404`, as a router without the `features` block does. Ask the operator. |
| `the CA fetch from {BASE_URL} answered {status}` | The download answered another status, such as a redirect or a `502` of a front. The base URL has to reach the router's proxy listener as the client reaches it. |
| `the fetched CA does not have the operator's fingerprint. Get the fingerprint from the router's operator` | The fetched bytes are not the CA the operator named: the input is mistyped, the operator replaced the CA, or the fetch reached another server. |
| `no proxy runs at {PROXY_URL}` | Under `SETUP=proxy`, curl could not connect to the proxy's address. |
| `the proxy at {PROXY_URL} does not reach the router's feature listener` | Under `SETUP=proxy`, the proxy answered the `CONNECT` with `502`: its upstream names another address than the feature listener's, or the listener is down. |
| `{host} leads to another host than the router's feature listener` | Under `SETUP=proxy`, the peer behind the proxy gave a certificate the fetched CA did not sign or another answer than the router's `401`. The program at `PROXY_URL` is no `phoebis-proxy`, or its upstream is no feature listener of this router. |
| `the check of the road to {host} failed (curl exit {n}, answer {codes})` | Under `SETUP=proxy`, the probe ended another way, a timeout among them. The line carries curl's exit and the two status codes, the answer's and the proxy's. |
| `{host} leads to another host than the router's feature listener over {family}` | Under `SETUP=redirect`, the name's address of that family, `ipv4` or `ipv6`, is not the router's: the family's hosts line is missing or wrong. |
| `the check of the road to {host} over {family} failed (curl exit {n}, answer {codes})` | Under `SETUP=redirect`, the probe of that family ended another way, a timeout among them. |
| `nothing answers at {host}:443 as this machine resolves it` | Under `SETUP=redirect`, the name resolved to no address or nothing accepted the connection, over both families. The feature listener does not answer on port 443 of the address the name leads to. |

### The variables

The table holds the lines the script exports, in the card's order. [The environment
block](client.md#the-environment-block) on The client describes the first seven, and the other lines have their one
place here:

| Line | Value | Why |
| --- | --- | --- |
| `ANTHROPIC_BASE_URL` | `BASE_URL` | The model requests go to the router's proxy listener. |
| `CLAUDE_CODE_OAUTH_TOKEN` | `ROUTER_KEY` | The features need the key in this variable. A key in `ANTHROPIC_AUTH_TOKEN` switches them off in the client, and so does an API key of the shell that the user approves at the client's question. |
| `CLAUDE_CODE_SUBSCRIPTION_TYPE` | `PLAN` | Required at `max` or `pro`. Under another type or none the client asks for the organization's policy limits, the router refuses that read, and the client then denies Artifacts. |
| `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL` | `1` | Required here. It keeps the Artifact tool on under `DISABLE_TELEMETRY=1`. |
| `CLAUDE_CODE_GATEWAY_HINT_HEADERS` | `1` | The router requires the request class header on a model request. |
| `DISABLE_TELEMETRY` | `1` | The client posts no telemetry batch, which in a feature setup would arrive at the router and be refused. |
| `CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK` | `1` | Fast mode works, and the client sends one refused call less at each start. |
| `CLAUDE_CODE_OAUTH_SCOPES` | `user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload` | The client opens the connectors under `user:mcp_servers`. The list is what the client believes of a token it does not hold, and what the upstream grants is a fact of the feature account's stored sign-in. |
| `CLAUDE_ENV_FILE` | the path of `claude-env.sh` | Bash commands of the session get their environment back ([The env file](#the-env-file)). |
| `HTTPS_PROXY` | `PROXY_URL`, under `SETUP=proxy` alone | The client's calls for the two feature hosts go through `phoebis-proxy`. |
| `NODE_EXTRA_CA_CERTS` | the path of `feature-ca.pem`, or of `ca-bundle.pem` | The client trusts the feature CA. |
| `NO_PROXY` | the base URL's host, `localhost`, `127.0.0.1` and `::1`, under `SETUP=proxy` alone | The model requests go to the router directly, and no entry of the shell takes a feature host off the proxy. |

`PHOEBIS_FEATURE_CA_SHA256`, the card's last line, is the script's input and no variable of the client.

Under `SETUP=proxy` the client's `NO_PROXY` takes no entry of the shell. An inherited entry that covers a feature host
would send the client's call for that host past the proxy, straight to the upstream and with the router key in it. The
script filters no inherited list, since a filter would have to match each form the client honors, and one missed form
has that cost. A host the shell kept off its own proxy therefore travels through `phoebis-proxy`, which tunnels it to
itself and resolves its name on the proxy's machine. The shell's value returns to Bash commands through the env file.

The client reads the lowercase proxy names first, so under `SETUP=proxy` the script unsets `https_proxy` and `no_proxy`
for the client, where a value of the shell would win over the script's two lines. Under `SETUP=redirect` it unsets each
proxy name, `ALL_PROXY`, `HTTP_PROXY`, `HTTPS_PROXY` and their lowercase forms, since a proxy of the shell would resolve
the two names on its own machine and send the router key to the upstream.

The script puts its own file in `CLAUDE_ENV_FILE`, so an env file that the shell named there no longer reaches a Bash
command of the session. It clears no credential name: an `ANTHROPIC_AUTH_TOKEN` or an API key that the shell holds, such
as one in `ANTHROPIC_API_KEY`, stays in the client's environment. Start the script from a shell that holds neither.

In an interactive session with an `ANTHROPIC_API_KEY` in its environment, the client asks whether to use it ("Do you
want to use this API key?"). Where the user declines, which is the answer the client recommends, both features stay on:
the session holds the Artifact tool and a connector's tools. Where the user approves, the client prints "claude.ai
connectors are disabled because ANTHROPIC_API_KEY or another auth source is set and takes precedence over your claude.ai
login", and its MCP view loads no connector. Whether a headless run asks the question is unmeasured. The Artifact tool
under an approved key and a session under `ANTHROPIC_AUTH_TOKEN` are unmeasured too, and the client's code switches the
features off in both, as the row of `CLAUDE_CODE_OAUTH_TOKEN` has it.

`NODE_EXTRA_CA_CERTS` takes one path. Where `BASE_CA` names a private CA, the script writes that file and the feature CA
into `ca-bundle.pem`, with a line break between them, and names the bundle.

### The env file

Before it sets or clears a variable, the script records what the shell held under `NODE_EXTRA_CA_CERTS` and under each
proxy name it touches, and writes `claude-env.sh`, which `CLAUDE_ENV_FILE` names. The client puts the file's text ahead
of each Bash command, so a command gets back what the shell held before the script. The file has two line forms. A name
that held a value gets one line, `export NAME='<the earlier value>'`. A name that held none gets two lines,
`export NAME=` and then `unset NAME`: the client's PowerShell tool reads the assignment lines alone, and Bash ends with
the name unset.

A user has one such file, and each session of that user shares it. Each start writes it anew at mode `0600`, a start
under the other `SETUP` or from a shell with other values among them, so the file holds what the latest start recorded.
It holds no secret of the router, and an earlier proxy value can carry a company proxy's password, which is the reason
for the mode.

Under `SETUP=proxy` a Bash command then runs without the proxy and reaches each host as a shell outside the session
does. Without the file a command that calls `api.anthropic.com` by itself, such as a project's test run with an API key
of its own, would end in a certificate error or in the router's `401`, and each git, curl and package manager call of
the session would fail while the proxy is down.

Under `SETUP=redirect` the file gives a command back the trust it had and a proxy the shell held. It cannot give a
command the two upstream hosts back, since the redirect holds for each program of the machine. A command that calls
`api.anthropic.com` by itself then ends in a certificate error before it sends a credential, where without the file a
Node command would trust the feature CA, reach the router and hand it the command's own key for a `401`.

### A headless client

`claude -p` and the SDK offer the Artifact tool only under `CLAUDE_CODE_ARTIFACT=1`. An interactive session lists the
tool without the variable, so the script leaves it out. For headless use, set it in the environment the script starts
in, and the client inherits it.

### What the client's children see

| Child | Its environment | Under `SETUP=proxy` | Under `SETUP=redirect` |
| --- | --- | --- | --- |
| A Bash tool command | what the shell held before the script, through the env file | reaches each host as a shell outside the session does | reaches the router at a feature host and gets a certificate error there |
| A helper command, a hook, a stdio MCP server, the status line | the script's variables | A call to another host is tunneled. A call to a feature host reaches the router: a child that trusts the system's store alone gets a certificate error, and a Node child gets the router's `401`. | A call to a feature host reaches the router, with the same two outcomes. |
| A nested client started from a Bash command | what the shell held before the script, through the env file | It keeps `ANTHROPIC_BASE_URL` and loses the key, which the client strips from a child's environment, so it behaves as a nested client does in the "Base URL" setup. | The same for its model requests, and its feature calls reach the router and end in a certificate error. |
| A session under `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB` | the script's variables, since that mode skips the env file | as the row of a hook has it, for Bash commands too | as the row of a hook has it, for Bash commands too |
| The PowerShell tool | the assignment lines of the env file | A name that held nothing reads as empty, and whether a child takes an empty proxy variable as none is unverified. | the same |

The environment that the row of a hook gives is measured for two of its children. A `SessionStart` hook and a stdio MCP
server of the project both held `HTTPS_PROXY`, `NODE_EXTRA_CA_CERTS` and `NO_PROXY` with the script's values, and a Bash
command of the same session got the shell's values back through the env file. The hook's `CLAUDE_ENV_FILE` named a file
of the client's own for that hook, and the server's named the script's file. The row's columns for the two setups, and
its environment for a helper command and the status line, are read from the client's code.

A child that must reach a feature host by itself needs an environment of its own. A stdio MCP server that calls the
Messages API with a key of its own would send that key to the router and get a `401`. Under `SETUP=proxy` the cure is
the server's `env` block in the client's MCP configuration, which sets `HTTPS_PROXY` and `NO_PROXY` for that server back
to what they were outside the session. Under `SETUP=redirect` no setting of a child helps, since the redirect holds for
each program of the machine.

## What the router forwards

The feature listener forwards the calls of this table and refuses each other one:

| Call | Method | Host | Path | What the router does |
| --- | --- | --- | --- | --- |
| The client's first-start check | `GET`, `HEAD` | `api.anthropic.com` | `/api/hello` | answers `200` itself, without a key |
| The WebFetch tool's domain check | `GET` | `api.anthropic.com` | `/api/web/domain_info`, with the query `domain=<host>` | forwards it without a credential, without a key |
| The Artifacts MCP server | `DELETE`, `GET`, `POST` | `api.anthropic.com` | `/api/artifacts/mcp` | forwards it under the account selected for Artifacts |
| An artifact's publish, list, read, deletion and sharing | `DELETE`, `GET`, `PATCH`, `POST` | `api.anthropic.com` | under `/api/frame/`, except `POST /api/frame/track` | forwards it under the account selected for Artifacts |
| The connector list | `GET` | `api.anthropic.com` | `/v1/mcp_servers` | forwards it under the account selected for the connectors |
| A connector's MCP session | `DELETE`, `GET`, `POST` | `mcp-proxy.anthropic.com` | under `/v1/mcp/` | forwards it under the account selected for the connectors |
| each other method, host and path | | | | refuses it |

For each call the router runs its checks in one order: the two calls without a key by their exact form, the key, the
table, the feature's selection, the account's state, the ledger's standing, and then the forward. The key stands ahead
of the table, so a caller without a key learns nothing of what the router forwards. The host of a call is its `Host`
line, and no model request is served here: one that arrives on the feature listener is refused, as it belongs on
`ANTHROPIC_BASE_URL`. A feature call passes none of a model request's other checks: no client class, no drift check, no
guardrail, no model rule, no budget and no slow-down.

The router compares a row's path with the path as the client sent it, without the query, and forwards that path as it
came. A row with a whole path matches that path alone, and a row with a prefix matches a path that goes on behind the
prefix by one character or more, so `/api/frame` and `/api/framework/x` match no row. Some forms of a path match no row,
and a keyed call with one gets the `403` `feature_not_allowed`: a segment `.` or `..`, an empty segment and so a closing
slash, a backslash, an encoded slash, backslash or dot (`%2f`, `%5c` and `%2e` in either case), and a path parameter,
which a semicolon opens. The router looks for these forms in the path as sent and in each form a percent-decode of it
gives, decoded once more until no `%` with two hex digits behind it is left, so `%252e` counts as an encoded dot and
`%3b` as a semicolon. An upstream that decodes or folds such a path could otherwise serve a refused call under the
feature account's sign-in, and the client builds none of these forms. The one excepted path, `POST /api/frame/track`,
is an event report. It is matched in each case of its letters and where a path decodes to it, so `POST
/api/frame/Track` and `POST /api/frame/%74rack` are refused too.

The two calls without a key match by their whole form. `GET` and `HEAD` on `/api/hello` answer `200` with an empty body
and send nothing upstream, which is where the first-start check of an interactive client ends, so a start needs no
stored login and no earlier session. The domain check matches a query that is `domain=<value>` with a value of 1 to 512
bytes and no `&`, and no body. The router drops each `Authorization` and `x-api-key` line of it and adds none, gives the
whole call 30 seconds, and forwards 16 checks at one time, 4 from one source address. A check above either count waits
for a place inside its 30 seconds. A request on either path in another form is no call without a key and meets the key
check.

A forwarded call's head goes upstream as the client sent it, in the client's order and spelling. The router puts the
account's credential in the place of the router key, writes its own `Host`, `Content-Length` and `Connection` and leaves
out the lines the relay copies for no request ([The header edits](relay.md#the-header-edits)). It merges no beta and
generates no request id. The method, the path and the query travel as sent, and so does the body, in the coding the
client gave it, after the router read it whole.

The answer goes back as the upstream gave it: the status, the head and the body, piece by piece as the pieces arrive,
since an MCP session answers with an event stream that can idle between two events. The router frames the answer itself
and keeps these head lines of the upstream to itself: `anthropic-organization-id`, `connection`, `content-length`,
`keep-alive`, `proxy-authenticate`, `proxy-authorization`, `set-cookie`, `te`, `trailer`, `transfer-encoding` and
`upgrade`. Each other line passes, the upstream's rate-limit lines among them where it sends them, which a model answer
hides. A key holder can read the feature account's plan standing from those.

## What the account's state does

Before each forward the router reads the selected account's state. Three states refuse the call with `403`
`feature_account_unavailable`, tested in this order: the account is deleted, it is not ready, as a signed-out account
is, or it is past its subscription's end. The message closes on `(operator)` for the first two and on
`(subscription_ended)` for the third.

A paused account keeps serving the features. A pause stops the routing of model requests and nothing else, and the
router keeps a paused account's sign-in fresh. A spent window, a cooldown, a budget, overage, the utilization ceiling, a
slow-down and a session's pin do not stop a feature call either, since the router counts no feature call in a plan
window.

To stop an account's features, change the selection or disconnect the account. A deletion sets each selection that names
the account to none. A stop holds from the next call on: the feature path cancels nothing, so a call in flight runs to
its own end, a connector's long stream among them.

What a `401` of a feature host gets depends on one header. A `401` that carries `X-Mcp-Error-Code` is the connector's
word that it wants a sign-in of its own, and it goes back at once and untouched for the client to read. A `401` without
that header gets one forced refresh of the account's token and one resend of the same call under the new token, and the
second answer goes back whatever it is. Where the refresh gives no newer token, the upstream's `401` goes back. The
`401` of a call that went out under an earlier sign-in of the account goes back too, with no refresh.

After a second `401`, behind the resend, the router remembers the rejected token. Each later call under that token that
gets such a `401` gets it back at once with no further refresh, and the first call under the account's next token gets
its one refresh and its one resend again. This hold has no time limit, and a restart forgets it.

No answer of a feature host marks the account, sets a cooldown or feeds a plan window, so a feature host's answer does
not move the routing of model requests. A forced refresh that fails for good marks the account as each failed refresh
does.

## What the router refuses and what the client shows

The feature listener answers a refusal in the form [Refusal codes](client.md#refusal-codes) describes, with its code in
the header `x-phoebis-error-code`, and that table holds each code's messages. The router gives these answers of its own
on this listener:

| Answer | When |
| --- | --- |
| `401` `auth_ambiguous`, `auth_malformed`, `auth_missing` or `auth_unknown` | The call carries no valid router key, on each call apart from the two that take none, as on the proxy listener. |
| `403` `feature_not_allowed` | The table holds no row for the keyed call. |
| `403` `feature_account_unset` | No account is selected for the call's feature, which the message names. |
| `403` `feature_account_unavailable` | The selected account's state refuses ([What the account's state does](#what-the-accounts-state-does)). |
| `503` `ledger_behind` | The ledger's writer is behind ([the ledger writer](statistics.md#the-ledger-writer)), on a keyed call ahead of its body's read and on a domain check ahead of its wait for a place. Nothing goes upstream, and the answer carries `Retry-After: 5`. |
| `413` `request_too_large`, `503` `body_budget_exhausted`, `429` `key_body_reads` or `408` `request_timeout` | The read of the call's body passes a bound of the `proxy` block. |
| `502` `upstream_unreachable` | The forward failed ahead of the upstream's answer head. |
| `429` `domain_checks_at_cap` | A domain check waited out its 30 seconds for a place. The answer carries `Retry-After: 1`. |

The first five rows are the checks of a keyed call in their order, and the first that fails answers. Each `403` carries
`x-should-retry: false`.

A call's body is read under the `proxy` block's bounds, as a model request's body is ([proxy](installation.md#proxy)). A
publish over a slow line can therefore meet the `408` `request_timeout`, and the operator moves that bound with
`proxy.body_read_seconds`, 1 to 599 seconds and 60 by default. A call without a body, which is the usual feature call,
meets none of the four bounds.

The `502` answers each failure ahead of the upstream's answer head: no connection within 10 seconds, no answer head
within 600 seconds, a head the router cannot read, a connection cut before the head, and a token refresh that failed on
the way. A failure behind the head ends the client's connection, as it ends a model stream.

A client in a feature setup sends more calls to the two hosts than the table holds, and each of them ends at the router.
The next four paragraphs say what interactive sessions of Claude Code 2.1.327 on Linux showed, started through the
launch script in both feature setups. Which calls the client sends, and what it does with an answer no session got, is
read from its code at Claude Code 2.1.325.

**At a start** the client sends several reads that get the `403`, its bootstrap read and its profile read among them,
and under `max` and `pro` the reads [`CLAUDE_CODE_SUBSCRIPTION_TYPE`](client.md#claude_code_subscription_type) names. A
`403` is the one status the client stays quiet on at each of them. One read goes out without a bearer,
`GET /api/directory/server-urls`, and gets the `401` `auth_missing`, which by the client's code costs it one error line
in its log. The session shows no error line for any of these reads.

**Live updates of an artifact** do not work behind the router. After a publish the client opens a WebSocket for the
page's live updates, the watch that tells a session of a new version or a comment. The upgrade request reaches the
feature listener with no router key where the listener reads one and gets the `401` `auth_missing`, and the listener
serves no protocol upgrade. The client tries again with a growing pause, and after a few minutes, three to four in the
measured sessions, it prints `Stopped watching Artifact: "{title}" (connection lost)` with the page's title. Publish,
update in place, list and read work beside that. While the client tries, the session also reads `GET /api/frame/{id}`
again and again, and each read is a row of the feature call list ([The records](#the-records)). The client's event
report, `POST /api/frame/track`, gets the `403`, which the session shows nowhere.

**The `/usage` dialog** cannot read the account's usage. Its read gets the `403`, and the dialog's Usage tab reads
"Couldn't load usage data" and prints the router's whole answer body below it, a `permission_error` with the message
`the router does not forward this claude.ai call`. Below the body the tab shows the session's own figures: the estimated
cost, the durations, the usage by model and the prompt cache line. The tab draws no usage bars: a session with 12 model
answers behind it showed none. A key's own figures are on [Your key's usage](client.md#your-keys-usage).

**The WebFetch tool** gets its domain check through the router: a call of the tool passed, with the `200` for its check.
By the client's code the tool fails a call with "Unable to verify if domain {host} is safe to fetch" where the check
gets no `200`, and the client's setting `skipWebFetchPreflight` skips the check.

The upstream answers the call of the client's `claude-artifacts` MCP server, `POST /api/artifacts/mcp`, with `404`. The
interactive sessions got that answer twice at each start, each call a row of the feature call list with the outcome
`forwarded`, and their MCP view listed no `claude-artifacts` entry. A headless run got it too and reports a failed
connection of that server. The Artifact tool publishes, lists and reads through `/api/frame/`, so it works in both.

## The records

The router writes one row per call it sent upstream under an account, at the call's end, whatever the upstream answered
and also where the forward failed. The rows stand in the ledger's table `feature_calls`:

| Column | Holds |
| --- | --- |
| `id` | the call's id |
| `received_at` | the call's arrival |
| `key_id` | the key |
| `feature` | `artifacts` or `connectors` |
| `account_id` | the account whose sign-in the call carried |
| `method` | the client's method |
| `path` | the path without its query |
| `status` | the status that went back, and none where no upstream head arrived |
| `outcome` | `canceled`, `failed` or `forwarded` |
| `error_code` | `upstream_unreachable` where the `502` answered, and none otherwise |
| `duration_ms` | the time from the call's arrival to the answer's end |

`forwarded` stands for an upstream answer that went back whole, whatever its status, `failed` for a forward that got the
`502` or an answer that ended behind its head, and `canceled` for a client that left before the answer's end. A row
holds no header value, no query and no body, and body capture stays a setting of model requests. The path can carry ids
of the feature account's own content, an artifact's id or a connector's server id. A refused call leaves no row, and
neither does the domain check. The rows age out with the request rows ([Retention](statistics.md#retention)), and a
feature call enters no request row, no rollup, no price, no session and no analytics figure.

A session that published an artifact leaves a run of rows with one path. While the client tries to open its watch of the
page's live updates, it reads `GET /api/frame/{id}` again and again, and each read is a forwarded call with a row of its
own ([What the router refuses and what the client shows](#what-the-router-refuses-and-what-the-client-shows)). The
watch's refused upgrade request and the refused event report leave no row.

The console lists the rows in the Requests view's section [Feature calls](console.md#feature-calls), with its filters
and its empty state. `GET /admin/feature-calls` answers them newest first as `feature_calls` with `next_cursor`, under
the query members `accounts`, `cursor`, `features`, `from`, `keys`, `limit` (1 through 500, and 50 by default),
`outcomes` and `to` ([API reference](api-reference.md)).

The metrics listener counts the calls in two families ([Metrics](statistics.md#metrics)):

| Family | Labels | Counts |
| --- | --- | --- |
| `phoebis_feature_calls_total` | `feature`, `key`, `outcome` | each call the router sent upstream, the domain check among them under the feature `domain_check` |
| `phoebis_feature_refusals_total` | `code`, `feature`, `key` | each call the router refused with a `401`, a `403`, the `429` `domain_checks_at_cap` of a domain check or the `503` `ledger_behind`, under the feature `none` for a call that matched no row, for each `401` and for a refused domain check |

Neither family carries a path, a status or an account. A refused call and a domain check each write one line at `debug`
too, with the code or the outcome, the method, the path without its query and the key's label.

## The CA over time

The router issues the feature listener's certificates by itself: one per feature host, valid for seven days, with a key
that lives in memory alone. It issues the next certificate of a name about a day ahead of the current one's end and
serves it from the next connection on. That takes no step of the operator and no `phoebis tls reload`, and a client sees
no change, since it trusts the CA ([TLS on the listeners](installation.md#tls-on-the-listeners)).

The CA itself is valid for ten years, and the router neither renews nor replaces it, since a new CA has a new
fingerprint for each user's script. From 30 days ahead of the CA's end the dashboard raises `certificate_expiring` under
the subject `features.ca` ([Conditions and gauges](statistics.md#conditions-and-gauges)), and the start and
`check-config` print `features.ca.cert_file {path} expires in {n} days ({date})`.

The operator replaces the CA by removing `feature-ca.pem` and `feature-ca-key.pem` from the data directory and starting
the router again, which creates a new CA. Each user then needs the new fingerprint from the operator, in the script's
`PHOEBIS_FEATURE_CA_SHA256`. Until then the script's compare stops each start. A session that started before the
replacement trusts the old CA, so its feature calls fail until it starts again under the new fingerprint.

`phoebis backup` leaves both CA files out, as it leaves out the storage key and the certificate pairs. A restore onto a
host without them gets a new CA at its first start, with the same consequence for each user
([Restore](operations.md#restore)).

## What the features leave on the machine

A router session shares the client's config directory with a local login where the machine has one. The one thing the
features store there is the list `claudeAiMcpEverConnected`, which holds the names of connectors of the feature account.
The script's own files stand under `${XDG_STATE_HOME:-$HOME/.local/state}/phoebis` ([The inputs](#the-inputs)).

## What is not supported

- A further proxy behind `phoebis-proxy`: the proxy connects to each destination directly, whatever proxy variables its
  own environment holds.
- A launch script for PowerShell: this page gains one once a run on Windows has checked it.
- An artifact's live updates: the watch that tells a session of a page's new version or of a comment opens a WebSocket,
  and the feature listener serves no protocol upgrade ([What the router refuses and what the client
  shows](#what-the-router-refuses-and-what-the-client-shows)).
- Remote Control: it stays unavailable behind the router in each setup ([Headers and
  transport](client.md#headers-and-transport)).
- The DesignSync tool: its calls under `/v1/design/` are on no row, so one that carries a router key gets the `403`
  `feature_not_allowed`, and one that carries the token of the tool's own login gets the `401` of a call without a valid
  router key.
- The features in the "Base URL" setup: without a proxy or a redirect the client sends their calls to the upstream with
  the router key, and the router sees none of them.
