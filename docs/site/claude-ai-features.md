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
the router alone. The redirect holds for each program of the machine, or on Linux under `PRIVATE_HOSTS=yes` for the
programs of one start alone. It needs no process beside the client, apart from the relay that a private hosts file
takes ([A hosts file for one start](#a-hosts-file-for-one-start)).

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

The client resolves `api.anthropic.com` itself ahead of the dial for an artifact's live updates, and it refuses the dial
where the name leads to a carrier-NAT, link-local, private or unique-local address ([What the router refuses and what
the client shows](#what-the-router-refuses-and-what-the-client-shows)). A router on a home or office network has such an
address. Each feature call still works there, since the guard stands on that one dial, and a loopback address passes it.

On macOS a redirect to such an address serves each feature call and leaves live updates unserved, and [Base URL and
proxy](#base-url-and-proxy) is the road to them. On Windows the redirect does the same, and "Base URL and proxy" is
their road there too. On a Linux machine that keeps `PRIVATE_HOSTS=no` with such an address, the redirect serves each
feature call and leaves live updates unserved, and "Base URL and proxy" or [a hosts file for one
start](#a-hosts-file-for-one-start) is the road to them.

A machine whose lines are missing or incomplete would send the feature calls to the upstream with the router key, as the
"Base URL" setup does. The launch script's check stops the start where one of the two names leads to another host over
an address family the machine has a road over at that moment, so start the client through the script: the card's lines
alone run no check. The check cannot tell one case from a sound machine: the line of one family is missing, and the
machine has no road over that family while the script runs ([The fetch, the compare and the
check](#the-fetch-the-compare-and-the-check)). Where the machine gains such a road inside the session, through a changed
network or a tunnel that comes up, a new connection of the client to a feature host can leave to the upstream with the
router key. So under `PRIVATE_HOSTS=no` the script names each address family it could not prove at each start, in a
notice behind `phoebis:` that stops nothing ([The stops](#the-stops)). A start under `PRIVATE_HOSTS=yes` wrote both
lines itself and prints no notice.

The script sets no proxy variable in this setup and clears the shell's proxy names for the client, since a proxy of the
shell would resolve the two names on its own machine.

### A hosts file for one start

On Linux the launch script's input `PRIVATE_HOSTS=yes` gives each start a hosts file of its own, which leads both names
to loopback for the client and its children alone, and a relay on loopback passes port 443 on to the router ([A relay
on loopback](#a-relay-on-loopback)). The script reads the input under `SETUP=redirect` alone, and under its default,
`no`, a start keeps the machine's own name resolution.

Behind its stops for the inputs and ahead of the fetch, such a start writes two files into the script's directory and
calls `sudo unshare --mount --propagation slave`. Inside that new mount namespace a root shell binds the private file,
`hosts.<pid>`, over `/etc/hosts` and runs the script a second time through `setpriv`, under the user and group ids that
`sudo` names in `SUDO_UID` and `SUDO_GID`. The second run does the fetch, the compare, the check and the client's start
as the calling user, so root holds for the namespace and the bind alone. The machine's `/etc/hosts` stays as it is, and
each program outside the start's process tree reads it as before. Where the machine's mounts are shared, as `systemd`
makes them at its start, each mount and unmount of the machine reaches the start's tree while the bind stays inside it
(`MS_SLAVE` in `mount_namespaces(7)`). So while the bind holds, the hosts file is the one thing in which the tree
differs from the machine.

The private file holds the machine's hosts file less the two feature names, and behind it one line per address family
that names both hosts on loopback:

```text
127.0.0.1 api.anthropic.com mcp-proxy.anthropic.com
::1 api.anthropic.com mcp-proxy.anthropic.com
```

A name of the machine's file that equals a feature host leaves its line, compared without case and with the line's
comment left out, the line's other names stay, and a line with no name left goes. A machine under a redirect by hosts
lines would otherwise keep the router's private address for both names beside loopback, and the client refuses live
updates for one such address among those a name resolves to. The loopback lines stand last, so a reverse lookup of
`127.0.0.1` or `::1` still reads the machine's own name. The file is the user's, at mode `0644` as the machine's file
is. It stands for `/etc/hosts` in the start's tree until the session ends, when it goes, unless the machine puts another
file in that place first.

A program of the machine that puts a new file in the place of `/etc/hosts` by a rename, as `sed -i` does, or that
removes the file while a session runs takes the private file out of the start's tree, and a write in place leaves it
(`mount_namespaces(7)`, measured in a user namespace on Debian 13 under Linux 7.0.12). From then on each new connection
of the session follows the machine's file: where that file holds no line for the two names, the connection leaves to the
upstream with the router key, and where it holds a redirect's lines, the calls reach the router and the client refuses
live updates again. The script's check runs once, at the start, and sees neither, and `SETUP=proxy` is the setup that
rests on no hosts file.

`sudo` empties the environment, so the second file, `shell-env.<pid>` at mode `0600`, carries the shell's exported
variables, its umask and the arguments for the client to the second run, which reads it and removes it before anything
else. The root shell hands that file's path over behind the marker word `phoebis-private-hosts`, and a run reads it only
where its own inputs say `SETUP=redirect` and `PRIVATE_HOSTS=yes`. The file's `set --` line puts its own path ahead of
the arguments, and the run removes the file by that word, so no inherited variable makes a run read or remove a file.

`sudo`'s own configuration decides where root finds `unshare`, `mount` and `setpriv`, through `secure_path` where it
sets one, and the script fixes no `PATH` for its root part. The call suits a user who may run each command as root:
the root shell binds a file that the user owns and can write while the session runs, and it runs a script whose path
the user names, so the call is no line to grant by itself in `sudoers`. By `sudoers(5)`, `sudo` logs each command it
runs under `log_allowed`, which is on by default. Here the command line holds the root shell's text and three paths of
the script's files, and no argument of the caller's. The client's session runs below `sudo` to its end, so the
settings of `sudo` for a session hold for it: by the same manual, `use_pty`, on by default from sudo 1.9.14, runs the
client in a pseudo-terminal of its own, `log_output` logs what the session prints, and `command_timeout` ends the
session at its bound.

Four points of this road stand as follows.

**A user namespace in place of `sudo`.** Measured on Debian 13, in a user namespace that maps the caller's id alone
(`unshare --map-current-user --keep-caps --mount`). The user and group ids read as the caller's, a file made in the
home belongs to the caller as read from outside, and a name of a hosts file bound there resolves to its address. But
`/usr/bin/sudo` reads as owned by 65534, an id the namespace does not map, and a set-user-ID program whose owner has no
mapping runs with that bit ignored (`user_namespaces(7)`). A `sudo` of the client's session would lose its rights
there, so the script asks for root through `sudo` instead.

**A resolver daemon ahead of `files`.** Not measured, since such a lookup can put a query on the network. A machine
whose `nsswitch.conf` names `resolve` ahead of `files` asks `systemd-resolved` first. The daemon reads `/etc/hosts`
itself (`ReadEtcHosts=` in `resolved.conf(5)`) and runs outside the start's mount namespace, so it answers from the
machine's hosts file and from no private one. The script's probe stops a start under which a feature name leads
elsewhere than to the router, and `SETUP=proxy` is the road for such a machine. The measured Debian 13 machine's line,
`hosts: files myhostname resolve [!UNAVAIL=return] dns`, names `files` first, so a lookup there reads the private file
for both names ahead of the daemon.

**The caller's environment.** The script's test runs the root part against stand-ins of `sudo`, `unshare`, `setpriv`
and `mount`, with a `sudo` that empties the environment and joins `0022` to the umask, as the tool does by default. A
variable of the calling shell, the caller's umask and arguments with two blanks in a row, a quote, an empty one and a
line break reach the client as the caller gave them, and no argument rides `sudo`'s command line. No start through the
real tools stands behind this road yet, so the form with `--propagation slave` has no run behind it.

**A connected watch.** Not measured: no session has held an artifact's live updates on this road yet.

### A relay on loopback

The private file leads both names to loopback, so a relay on the client's machine passes each connection to port 443
of `127.0.0.1` and of `::1` on to the feature listener, byte for byte. The client's TLS ends at the router, so the
relay reads no key and holds no certificate. It connects to the listener's own port, so the router needs no port 443
for this road. The script runs no relay of its own: it ends in the client's start, a second session would find the
port taken, and port 443 needs a right that the user lacks. The relay is a standing service, which the machine's
administrator installs once, with the right to bind a low port and no other. For a feature listener at
`192.0.2.10:8443`, two `socat` listeners as service units:

```ini title="/etc/systemd/system/phoebis-relay-ipv4.service"
[Unit]
Description=Relay from port 443 of 127.0.0.1 to the Phoebis feature listener

[Service]
ExecStart=/usr/bin/socat TCP4-LISTEN:443,bind=127.0.0.1,reuseaddr,fork TCP:192.0.2.10:8443
DynamicUser=yes
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

```ini title="/etc/systemd/system/phoebis-relay-ipv6.service"
[Unit]
Description=Relay from port 443 of ::1 to the Phoebis feature listener

[Service]
ExecStart=/usr/bin/socat TCP6-LISTEN:443,bind=[::1],ipv6only=1,reuseaddr,fork TCP:192.0.2.10:8443
DynamicUser=yes
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

The administrator installs and starts the two units. No run on this page installed them, so their bind of port 443
under `DynamicUser=yes` is unmeasured. Both `socat` lines carried a request from curl on loopback, one for each address
family, to a TLS server under a certificate for `api.anthropic.com`, with ports of the test's own in place of 443 and
8443, and `systemd-analyze verify` passed both units under systemd 257. At each start the script's probes go through
the relay, and a missing one stops the start with `nothing answers at {host}:443 as this machine resolves it`.

No relay runs where the router is on the client's machine and its feature listener answers on port 443 of both
loopback addresses. A listener that answers on one loopback family takes one relay listener for the other.

## The launch script

The script below starts the client in either feature setup. Save it to a file, set the eight inputs at its head, and
start the client through it in place of `claude`: each argument goes on to the client. The file then holds the key's
secret, so keep it readable by its user alone, at mode `0700`. At each start it fetches the CA's certificate, compares
its fingerprint, checks the road to both feature hosts, writes the env file for Bash commands, sets the client's
variables and starts the client. It is a POSIX `sh` script, and it needs `curl` and one of `sha256sum` and `shasum`. A
start under `PRIVATE_HOSTS=yes` also needs `mount`, `setpriv`, `sudo` and `unshare`.

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
PRIVATE_HOSTS='no'                 # no, or yes: a hosts file for this start alone, read under SETUP=redirect alone
PLAN='max'                         # max, or pro
BASE_CA=''                         # the private CA of the base URL's certificate, where it has one

# A start under PRIVATE_HOSTS=yes runs this file a second time inside its namespace, below a sudo that empties the
# environment. The root part hands that run a marker word and the path of a file the first run wrote: the exported
# variables, the umask and a set line with the file's own path and then the caller's arguments. The run removes the
# file by that word and by no variable, since a variable the file restores could name another file. The two inputs
# above decide whether a run reads the file, so no variable it inherited does.
entered=no
if [ "$SETUP" = redirect ] && [ "$PRIVATE_HOSTS" = yes ] && [ "${1-}" = phoebis-private-hosts ]; then
  . "$2"
  rm -f "$1"
  shift
  entered=yes
fi

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
[ "$SETUP" != redirect ] || case "$PRIVATE_HOSTS" in no|yes) ;; *) stop "PRIVATE_HOSTS is neither no nor yes" ;; esac
case "$PLAN" in max|pro) ;; *) stop "PLAN is neither max nor pro" ;; esac

state="${XDG_STATE_HOME:-$HOME/.local/state}/phoebis"
ca="$state/feature-ca.pem"
env_file="$state/claude-env.sh"
# Each name carries the process id, so two starts at one time share neither file.
new_ca="$ca.$$"
head_file="$state/check-head.$$"
# Named under PRIVATE_HOSTS=yes alone, and empty for the exit trap otherwise.
hosts=
saved=
# The script's files are the user's alone. The shell's umask returns ahead of the client's start, since under 077
# each file the session and its commands create would be made 0600.
shell_umask=$(umask)
umask 077
mkdir -p "$state"
trap 'rm -f "$new_ca" "$head_file" "$hosts" "$saved"' EXIT
# Some shells run the exit trap for a signal only where the signal has a trap of its own.
trap 'exit 1' HUP INT TERM

# The client refuses to open an artifact's live updates where a feature host resolves to a private address. This
# start gets a hosts file of its own that names both hosts on loopback, where a relay passes port 443 to the router.
# Root holds for the namespace and the bind alone: the second run is the calling user's.
if [ "$SETUP" = redirect ] && [ "$PRIVATE_HOSTS" = yes ] && [ "$entered" = no ]; then
  for tool in setpriv sudo unshare; do
    command -v "$tool" >/dev/null 2>&1 ||
      stop "PRIVATE_HOSTS=yes needs $tool, which this machine lacks. SETUP=proxy needs none of the three"
  done
  # A file of this start alone, made new, so no start writes into a file that another start has bound.
  hosts="$state/hosts.$$"
  rm -f "$hosts"
  {
    # The machine's file less the two names. A line of a hosts redirect would keep a private address for a name
    # beside loopback, and the client refuses live updates for one private address among a name's addresses. The
    # loopback lines stand last, so a reverse lookup of 127.0.0.1 or ::1 still reads the machine's own name.
    awk '
      {
        line = $0; note = ""
        at = index(line, "#")
        if (at) { note = " " substr(line, at); line = substr(line, 1, at - 1) }
        count = split(line, word, " ")
        if (count < 2) { print; next }
        kept = word[1]; names = 0
        for (i = 2; i <= count; i++) {
          name = tolower(word[i])
          if (name != "api.anthropic.com" && name != "mcp-proxy.anthropic.com") { kept = kept " " word[i]; names++ }
        }
        if (names == count - 1) print
        else if (names > 0) print kept note
      }
    ' /etc/hosts
    echo '127.0.0.1 api.anthropic.com mcp-proxy.anthropic.com'
    echo '::1 api.anthropic.com mcp-proxy.anthropic.com'
  } > "$hosts"
  chmod 644 "$hosts"
  # sudo empties the environment, joins its own umask to the caller's and logs its command line. So the second run
  # takes the caller's variables, umask and arguments from this file, and sudo's line holds none of them. The set line
  # opens with the file's own path, by which the second run removes it.
  saved="$state/shell-env.$$"
  {
    export -p
    echo "umask $shell_umask"
    printf 'set --'
    for arg in "$saved" ${1+"$@"}; do
      quoted=
      while :; do
        case $arg in
          *"'"*) quoted=$quoted${arg%%"'"*}"'\\''"; arg=${arg#*"'"} ;;
          *) break ;;
        esac
      done
      printf " '%s'" "$quoted$arg"
    done
    echo
  } > "$saved"
  rc=0
  # The caller's ids are the ones sudo names itself, so nothing this run computed says whom the client runs as.
  sudo unshare --mount --propagation slave -- sh -c '
    for id in "${SUDO_UID-}" "${SUDO_GID-}"; do
      case "$id" in ""|*[!0-9]*) echo "phoebis: sudo named no calling user" >&2; exit 1 ;; esac
    done
    mount --bind "$1" /etc/hosts || exit $?
    exec setpriv --reuid "$SUDO_UID" --regid "$SUDO_GID" --init-groups -- env -i sh "$3" phoebis-private-hosts "$2"
  ' sh "$hosts" "$saved" "$0" || rc=$?
  # The second run removes the file first, so a file that still stands means it never started.
  [ ! -e "$saved" ] || stop "the private hosts file could not be put in place (exit $rc)"
  exit "$rc"
fi

# A closing slash would make the fetch ask for //v0/features/ca.pem, and the router's 404 for that path reads as
# a router without a feature listener.
BASE_URL=${BASE_URL%/}

# The fetch goes straight to the router, with no proxy in between.
rc=0
status=$(curl -q --silent --noproxy '*' --connect-timeout 10 --max-time 30 ${BASE_CA:+--cacert "$BASE_CA"} \
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
  codes=$(curl -q --silent --output /dev/null --dump-header "$head_file" --connect-timeout 10 --max-time 30 \
    --write-out '%{http_code} %{http_connect}' --cacert "$new_ca" "$@" "$url") || rc=$?
  code=$(tr -d '\r' < "$head_file" | grep -i '^x-phoebis-error-code:' | tail -n 1 | sed 's/^[^:]*: *//')
  rm -f "$head_file"
}

unproven_ipv4=
unproven_ipv6=
# Both hosts, since a redirect that leaves out the second would pass a check of the first, and each connector call
# would then carry the router key to the upstream.
for host in api.anthropic.com mcp-proxy.anthropic.com; do
  if [ "$SETUP" = proxy ]; then
    # The proxy answers the CONNECT with its 502 once its own connect has taken ten seconds, so this probe waits
    # past that for its connection.
    probe "$host" --proxy "$PROXY_URL" --noproxy '' --connect-timeout 15
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
        6:*|7:*) [ "$PRIVATE_HOSTS" = yes ] || eval "unproven_$family=\"\$unproven_$family $host\"" ;;
        0:*|35:*|51:*|60:*) stop "$host leads to another host than the router's feature listener over $family" ;;
        *) stop "the check of the road to $host over $family failed (curl exit $rc, answer $codes)" ;;
      esac
    done
    [ "$routers" -gt 0 ] || stop "nothing answers at $host:443 as this machine resolves it"
  fi
done
# A family over which nothing answered proves nothing of its hosts line. A road over it that comes up inside the
# session would carry a new connection to the upstream with the router key where that line is missing.
for family in ipv4 ipv6; do
  eval "left=\$unproven_$family"
  [ -z "$left" ] ||
    echo "phoebis: nothing answered over $family, so its hosts line is not proved for$left. Keep both lines" >&2
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

The script reads eight inputs at its head. For each feature setup the console's client configuration card prints the
eight input lines in the script's order, with the router's values where it knows them: the base URL's scheme and port,
the fingerprint of the feature CA and the setup's word. Paste the card's block over the input lines at the script's
head, which replaces the eight lines with their comments, then replace `<key>` with the key's secret and the placeholder
host `router.example.com` with the name that reaches the router. Each input, with the variable the script sets from it:

| Input | Holds | The variable it sets |
| --- | --- | --- |
| `BASE_URL` | the URL of the router's proxy listener | `ANTHROPIC_BASE_URL` |
| `ROUTER_KEY` | the key's secret | `CLAUDE_CODE_OAUTH_TOKEN` |
| `PHOEBIS_FEATURE_CA_SHA256` | the fingerprint of the router's feature CA, from the operator | none, the script compares the fetched CA with it |
| `SETUP` | `proxy` for "Base URL and proxy", `redirect` for "Base URL and DNS redirect" | none, it picks the road and whether the script sets `HTTPS_PROXY` and `NO_PROXY` |
| `PROXY_URL` | the address of `phoebis-proxy`, read under `SETUP=proxy` alone | `HTTPS_PROXY` |
| `PRIVATE_HOSTS` | `no`, or `yes` for a hosts file of each start's own on Linux ([A hosts file for one start](#a-hosts-file-for-one-start)), read under `SETUP=redirect` alone | none |
| `PLAN` | `max` or `pro` | `CLAUDE_CODE_SUBSCRIPTION_TYPE` |
| `BASE_CA` | the file of a private CA behind the base URL's certificate, empty where it has none | `NODE_EXTRA_CA_CERTS`, through `ca-bundle.pem` |

The script sets each variable of the client by itself, with the paths of its own files ([The
variables](#the-variables)), so the shell that starts it needs none of them. A proxy or trust line set in that shell
would reach each Bash command of the session as the shell's own value.

Ahead of the fetch the script stops where `ROUTER_KEY` or `PHOEBIS_FEATURE_CA_SHA256` is empty or still reads its
placeholder, where `SETUP` is neither `proxy` nor `redirect`, where `PRIVATE_HOSTS` is neither `no` nor `yes` under
`SETUP=redirect`, and where `PLAN` is neither `max` nor `pro`. Under `PRIVATE_HOSTS=yes` it also stops there where the
machine lacks `setpriv`, `sudo` or `unshare`, and where the namespace or the bind fails. An empty key would start the
client with an empty token, and a client with a login of its own can then fall back to that login.

The script keeps its files under `${XDG_STATE_HOME:-$HOME/.local/state}/phoebis`, readable by the user alone:
`feature-ca.pem`, `claude-env.sh`, and `ca-bundle.pem` where two CAs share one file. It works under `umask 077` and puts
the shell's umask back ahead of the client's start, since under `077` each file the session and its commands create
would be made `0600`. The files of a start in progress carry the process id in their names and go at each exit, so two
starts at one time share none of them. Each start has two, and under `PRIVATE_HOSTS=yes` two more: `hosts.<pid>`, the
private hosts file at mode `0644`, and `shell-env.<pid>`, which the second run reads and removes as it starts. Each curl
call gives up after 10 seconds without a connection and after 30 seconds in all, apart from the probe through the proxy,
which waits up to 15 seconds for its connection, past the proxy's ten seconds for its own, so a name that leads to an
address which drops packets does not hold the start for the system's own timeout.
Each curl call opens with `-q`, so curl reads no configuration file of the user, and a line of a `.curlrc` leads neither
the fetch nor a probe along another road than the client's.

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
that is missing. Under `PRIVATE_HOSTS=no` the script names each such family at each start, with the hosts whose probe
over it reached nothing, in a notice behind `phoebis:` that stops nothing. Keep both lines in the hosts file whatever
families the machine reaches at the start. A resolver of the network that answers both names for both families serves
the same end. A start under `PRIVATE_HOSTS=yes` wrote both lines itself, into the start's own hosts file, and prints no
notice.

### The stops

A stop prints one line on standard error, behind `phoebis:`, and the client does not start. The messages, in the
script's order:

| Message | What it means |
| --- | --- |
| `ROUTER_KEY holds no key. Set it to the key the router's operator handed over` | The input is empty or still reads `<key>`. |
| `PHOEBIS_FEATURE_CA_SHA256 holds no fingerprint. Set it to the operator's` | The input is empty or still reads `<fingerprint>`. |
| `SETUP is neither proxy nor redirect` | The input holds another word. |
| `PRIVATE_HOSTS is neither no nor yes` | Under `SETUP=redirect`, the input holds another word. |
| `PLAN is neither max nor pro` | The input holds another word. |
| `PRIVATE_HOSTS=yes needs {tool}, which this machine lacks. SETUP=proxy needs none of the three` | Under `SETUP=redirect`, the machine lacks a tool of the private hosts file, and the line names the first it lacks of `setpriv`, `sudo` and `unshare`. |
| `sudo named no calling user` | The root part prints it ahead of the bind, where `sudo` set no `SUDO_UID` or `SUDO_GID`, or one that is no number. The next stop follows. |
| `the private hosts file could not be put in place (exit {n})` | Under `PRIVATE_HOSTS=yes`, the second run never started: `sudo` refused the call, `unshare` or the bind failed, or `setpriv` failed, and `{n}` is the exit that `sudo` passed back. A home on a share that maps root to another user can stop the bind too, a cause that no run measured. |
| `the router at {BASE_URL} does not answer (curl exit {n})` | The fetch got no HTTP answer: a wrong base URL, a router that is down, or a certificate curl does not trust, for which `BASE_CA` names the private CA. |
| `the router at {BASE_URL} has no feature listener` | The download answered `404`, as a router without the `features` block does. Ask the operator. |
| `the CA fetch from {BASE_URL} answered {status}` | The download answered another status, such as a redirect or a `502` of a front. The base URL has to reach the router's proxy listener as the client reaches it. |
| `the fetched CA does not have the operator's fingerprint. Get the fingerprint from the router's operator` | The fetched bytes are not the CA the operator named: the input is mistyped, the operator replaced the CA, or the fetch reached another server. |
| `no proxy runs at {PROXY_URL}` | Under `SETUP=proxy`, curl could not connect to the proxy's address. |
| `the proxy at {PROXY_URL} does not reach the router's feature listener` | Under `SETUP=proxy`, the proxy answered the `CONNECT` with `502`: its upstream names another address than the feature listener's, or the listener is down. A router address that drops packets can lead to this `502` too, a cause that no run measured. |
| `{host} leads to another host than the router's feature listener` | Under `SETUP=proxy`, the peer behind the proxy gave a certificate the fetched CA did not sign or another answer than the router's `401`. The program at `PROXY_URL` is no `phoebis-proxy`, or its upstream is no feature listener of this router. |
| `the check of the road to {host} failed (curl exit {n}, answer {codes})` | Under `SETUP=proxy`, the probe ended another way, a timeout among them. The line carries curl's exit and the two status codes, the answer's and the proxy's. |
| `{host} leads to another host than the router's feature listener over {family}` | Under `SETUP=redirect`, the name's address of that family, `ipv4` or `ipv6`, is not the router's: the family's hosts line is missing or wrong. Under `PRIVATE_HOSTS=yes` a resolver daemon that `nsswitch.conf` names ahead of `files` answers from the machine's hosts file and from no private one, a cause that no run measured. |
| `the check of the road to {host} over {family} failed (curl exit {n}, answer {codes})` | Under `SETUP=redirect`, the probe of that family ended another way, a timeout among them. |
| `nothing answers at {host}:443 as this machine resolves it` | Under `SETUP=redirect`, the name resolved to no address or nothing accepted the connection, over both families. The feature listener does not answer on port 443 of the address the name leads to. |

A notice prints one line behind `phoebis:` too and stops nothing: under `PRIVATE_HOSTS=no` a redirect start that passes
the check prints `nothing answered over {family}, so its hosts line is not proved for {hosts}. Keep both lines` for each
address family over which nothing answered, where `{hosts}` names the feature hosts whose probe over that family reached
nothing. A start under `PRIVATE_HOSTS=yes` wrote both lines itself and prints no notice.

### The variables

The table holds the lines the script exports, in the script's order. [The environment
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
| `NODE_EXTRA_CA_CERTS` | the path of `feature-ca.pem`, or of `ca-bundle.pem` | The client trusts the feature CA. |
| `HTTPS_PROXY` | `PROXY_URL`, under `SETUP=proxy` alone | The client's calls for the two feature hosts go through `phoebis-proxy`. |
| `NO_PROXY` | the base URL's host, `localhost`, `127.0.0.1` and `::1`, under `SETUP=proxy` alone | The model requests go to the router directly, and no entry of the shell takes a feature host off the proxy. |

`PHOEBIS_FEATURE_CA_SHA256` is the script's input and no variable of the client.

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
| An artifact's live updates, as a WebSocket | `GET` | `api.anthropic.com` | `/api/frame/sync` | admits it by the token a keyed page read handed out, without a key, and carries the socket as a tunnel of bytes |
| The Artifacts MCP server | `DELETE`, `GET`, `POST` | `api.anthropic.com` | `/api/artifacts/mcp` | forwards it under the account selected for Artifacts |
| An artifact's publish, list, read, deletion and sharing | `DELETE`, `GET`, `PATCH`, `POST` | `api.anthropic.com` | under `/api/frame/`, except `POST /api/frame/track` | forwards it under the account selected for Artifacts |
| The connector list | `GET` | `api.anthropic.com` | `/v1/mcp_servers` | forwards it under the account selected for the connectors |
| A connector's MCP session | `DELETE`, `GET`, `POST` | `mcp-proxy.anthropic.com` | under `/v1/mcp/` | forwards it under the account selected for the connectors |
| each other method, host and path | | | | refuses it |

For each call the router runs its checks in one order: three shapes that take no key, each by its exact form, the two
calls without a key and the socket request, then the key, the table, the feature's selection, the account's state, the
ledger's standing, and then the forward. The key stands ahead of the table, so a caller without a key learns nothing of
what the router forwards. The host of a call is its `Host` line, and no model request is served here: one that arrives
on the feature listener is refused, as it belongs on `ANTHROPIC_BASE_URL`. A feature call passes none of a model
request's other checks: no client class, no drift check, no guardrail, no model rule, no budget and no slow-down.

The router compares a row's path with the path as the client sent it, without the query, and forwards that path as it
came. A row with a whole path matches that path alone, and a row with a prefix matches a path that goes on behind the
prefix by one character or more, so `/api/frame` and `/api/framework/x` match no row. Some forms of a path match no row,
and a keyed call with one gets the `403` `feature_not_allowed`: a segment `.` or `..`, an empty segment and so a closing
slash, a backslash, an encoded slash, backslash or dot (`%2f`, `%5c` and `%2e` in either case), a control character that
a percent-decode gives (`%00` to `%1f` and `%7f`), and a path parameter, which a semicolon opens. The router looks for
these forms in the path as sent and in each form a percent-decode of it gives, decoded once more until no `%` with two
hex digits behind it is left, so `%252e` counts as an encoded dot and `%3b` as a semicolon. Where a decoded form holds a
`?` or a `#`, the part ahead of the first one is read for a segment `.` or `..` and an empty segment too, so
`/api/frame/x/%3Fy` and `/api/frame/x/.%3Fy` match no row. An upstream that decodes or folds such a path could otherwise
serve a refused call under the feature account's sign-in, and the client builds none of these forms. The one excepted
path, `POST /api/frame/track`, is an event report. It is matched in each case of its letters, where a path decodes to
it, where a decoded path reads it ahead of a `?` or a `#`, and where a decoded path reads it with each space and each
character outside ASCII left out, so `POST /api/frame/Track`, `POST /api/frame/%74rack`, `POST /api/frame/track%3Fx` and
`POST /api/frame/track%20` are refused too.

The two calls without a key match by their whole form. `GET` and `HEAD` on `/api/hello` answer `200` with an empty body
and send nothing upstream, which is where the first-start check of an interactive client ends, so a start needs no
stored login and no earlier session. The domain check matches a query that is `domain=<value>` with a value of 1 to 512
bytes and no `&`, and no body. The router drops each `Authorization` and `x-api-key` line of it and adds none, gives the
whole call 30 seconds, and forwards 16 checks at one time, 4 from one source address. A check above either count waits
for a place inside its 30 seconds. A request on either path in another form is no call without a key and meets the key
check.

A socket request matches by its whole form too: a `GET` on `api.anthropic.com` whose path is `/api/frame/sync`, compared
whole in each decoded form the path check reads, with an `Upgrade` line that names `websocket`, a `Connection` line that
lists `upgrade` and no body. The router reads no router key on it and admits it by its token, the one entry of the
client's token shape in its `Sec-WebSocket-Protocol` list. A keyed `GET` of `/api/frame/{id}` that the upstream answers
`200` hands out such a token in its JSON member `syncToken`, whatever the read's query, so the read ahead of a watch and
the read ahead of a presence connection each hand one out. The router reads the member beside the answer's stream, in
each content coding it decodes, and at the body's clean end it keeps the token's SHA-256 in memory with the key that
read it and the account the read went out under, until the token's own end, held between 300 and 900 seconds behind the
read. It keeps 1,024 tokens for each key and drops a key's oldest at one more, a read whose body fails behind its token
keeps nothing, and a restart starts with none.

Behind the token the router checks what a keyed call meets: the key that read the token still stands, its selection for
Artifacts still names the account, the account still holds the sign-in the read went out under, the account's state lets
it serve, and the ledger is not behind. A socket request stamps no key's last use, since it presents no key and the page
read ahead of it stamped one. The router checks the key, the selection, the sign-in and the account's state once more
when the upstream's `101` arrives, so a key deleted while the upstream held the upgrade opens no socket.

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

A socket request's head goes upstream as the client sent it, with the client's `Connection` and `Upgrade` lines and no
credential line: the router drops each `Authorization` and `x-api-key` line and adds none, since the token in the
subprotocol is the upstream's one credential. It leaves on a fresh connection of the inference connector, which no pool
takes back. The upstream's `101` goes back with its `connection` and `upgrade` lines, which the router keeps out of each
other answer, and from then on the router carries the socket's bytes both ways unread until one side ends or fails, the
socket moves no byte for the idle time, a check of its key and its account fails, or the router stops. It then closes
both sides and writes no close code of its own. Each open socket holds one of the listener's connections. An answer
other than a `101`, the upstream's own `403` for a token it refuses among them, goes back as each answer does, and a
request that gets no answer head gets the `502`.

## What the account's state does

Before each forward the router reads the selected account's state. Three states refuse the call with `403`
`feature_account_unavailable`, tested in this order: the account is deleted, it is not ready, as a signed-out account
is, or it is past its subscription's end. The message closes on `(operator)` for the first two and on
`(subscription_ended)` for the third.

While a selection of a pool or of a key names an account that is not ready or past its subscription's end, the
dashboard's attention line and the selecting pools' cards show the item `feature_account_unavailable`
([Conditions and gauges](statistics.md#conditions-and-gauges)), and the selects of the pool dialog and the key dialog
list the account disabled with its reason. The stored selection stays as it is, and its calls keep getting the `403`
until the account can serve again or the selection moves.

A paused account keeps serving the features. A pause stops the routing of model requests and nothing else, and the
router keeps a paused account's sign-in fresh. A spent window, a cooldown, a budget, overage, the utilization ceiling, a
slow-down and a session's pin do not stop a feature call either, since the router counts no feature call in a plan
window.

To stop an account's features, change the selection or disconnect the account. A deletion sets each selection that names
the account to none. A stop holds from the next call on: a keyed call in flight still runs to its own end, a connector's
long stream among them, and an open socket ends at its next check, at most 30 seconds later.

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
| `401` `auth_ambiguous`, `auth_malformed`, `auth_missing` or `auth_unknown` | The call carries no valid router key, on each call apart from the two that take none and a socket request, as on the proxy listener. A socket request that no token admits gets `auth_missing` too, and the refusal family counts it under `socket_token_unknown`. |
| `403` `feature_not_allowed` | The table holds no row for the keyed call. |
| `403` `feature_account_unset` | No account is selected for the call's feature, which the message names. |
| `403` `feature_account_unavailable` | The selected account's state refuses ([What the account's state does](#what-the-accounts-state-does)). |
| `503` `ledger_behind` | The ledger's writer is behind ([the ledger writer](statistics.md#the-ledger-writer)), on a keyed call ahead of its body's read and on a domain check ahead of its wait for a place. Nothing goes upstream, and the answer carries `Retry-After: 5`. |
| `413` `request_too_large`, `503` `body_budget_exhausted`, `429` `key_body_reads` or `408` `request_timeout` | The read of the call's body passes a bound of the `proxy` block. |
| `502` `upstream_unreachable` | The forward failed ahead of the upstream's answer head. |
| `429` `domain_checks_at_cap` | A domain check waited out its 30 seconds for a place. The answer carries `Retry-After: 1`. |
| `429` `key_sockets` | The socket request's key has 256 sockets open. The answer carries no `Retry-After`. |
| `429` `sockets_at_cap` | The listener carries 2,048 open sockets, whatever their keys. The answer carries no `Retry-After`. |

The first five rows are the checks of a keyed call in their order, and the first that fails answers. Each `403` carries
`x-should-retry: false`.

No token admits a socket request where no page read handed its token out, where it offers no token or two, where its
token has ended, where the key that read it, that key's selection or the account's sign-in moved since the read, or
where two keys of one feature account read the same token. Each such request gets the one `401` `auth_missing`, the
answer of a call without a key, so the answer tells a caller nothing of the socket's path. The client drops its token at
a `401` and dials again behind a new page read. A token that two keys read stays refused until its end, so each client
then dials with the token of its own next read. Behind its token a socket request meets the `403`
`feature_account_unavailable` and the `503` `ledger_behind` as a keyed call does, and the `403` and the `401` hold at
the upstream's `101` too.

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

**Live updates of an artifact** work through the feature listener in both setups. After a publish the client opens a
WebSocket for the page's live updates, the watch that tells a session of a new version or a comment, and a session that
edits a page opens a second one beside it, the presence connection. Each dials right behind a page read, whose token
admits it ([What the router forwards](#what-the-router-forwards)). The client resolves `api.anthropic.com` itself ahead
of the dial and refuses a carrier-NAT, link-local, private or unique-local address, so under `SETUP=redirect` the name
has to lead to a loopback address or a public one ([Base URL and DNS redirect](#base-url-and-dns-redirect)). The router
counts the open sockets of each key and of the listener and refuses a socket request above either count with a `429`
([What the router refuses and what the client shows](#what-the-router-refuses-and-what-the-client-shows)), and it ends
a socket that moved no byte in either direction for 90 seconds. No setting moves the two counts or the idle time. Each
30 seconds from a socket's open the router checks the socket's key, that key's selection, the account's sign-in and the
account's state again, as the admission checked them, and ends at once, on both sides, a socket that fails one of these
checks. A pause ends no socket, as it stops no feature call. Where the router refuses a watch for another cause than a
full count, or ends its socket after the idle time, at a failed check or at the router's stop, the session reads a lost
connection and, by the client's code, dials again behind a new page read. After a failed check, that read meets the
router's refusal for the changed state, or goes out under the account the selection names now. At the `429` of a full
count the session waits 30 seconds or more, counts no failure and dials again, by the client's code. The client's event
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

A session that watches a page leaves two rows for each socket it opens: the page read with the status 200 and, right
behind it, `GET /api/frame/sync` with the status 101 and the outcome `forwarded`, written at the `101` while the socket
stays open. Its `duration_ms` runs to the `101`, and no row tells how long the socket ran. A presence connection leaves
the same two rows, and each redial adds two more. A socket request the router refuses leaves no row, one it refused at
the upstream's `101` among them though that request went upstream, and neither does the refused event report.

The console lists the rows in the Requests view's section [Feature calls](console.md#feature-calls), with its filters
and its empty state. `GET /admin/feature-calls` answers them newest first as `feature_calls` with `next_cursor`, under
the query members `accounts`, `cursor`, `features`, `from`, `keys`, `limit` (1 through 500, and 50 by default),
`outcomes` and `to` ([API reference](api-reference.md)).

The metrics listener counts the calls in two families ([Metrics](statistics.md#metrics)):

| Family | Labels | Counts |
| --- | --- | --- |
| `phoebis_feature_calls_total` | `feature`, `key`, `outcome` | each call the router sent upstream, the domain check among them under the feature `domain_check` |
| `phoebis_feature_refusals_total` | `code`, `feature`, `key` | each call the router refused with a `401`, a `403`, the `429` `domain_checks_at_cap` of a domain check, the `429` `key_sockets` or `sockets_at_cap` of a socket request or the `503` `ledger_behind`, under the feature `none` for a call that matched no row, for each `401` and for a refused domain check, and the `401` of a socket request that no token admits under the code `socket_token_unknown` |

Neither family carries a path, a status or an account. A refused call and a domain check each write one line at `debug`
too, with the code or the outcome, the method, the path without its query and the key's label. A socket's tunnel writes
one at its end, with the cause (`check_failed`, `client_ended`, `client_failed`, `idle`, `stopped`, `upstream_ended` or
`upstream_failed`), under `check` the failed check (`account`, `key`, `selection` or `sign_in`) where the cause is
`check_failed`, the method, the path, the key's label and the socket's life in milliseconds. The router writes one where
a page read under one key returns a token that another key read, with the path and the two keys' labels, and one where
it cannot scan a page read's answer, with the cause and the path. None of these lines holds a token or a byte of a
body. Under `RUST_LOG=info,phoebis_relay::features=debug` the router writes the feature module's `debug` lines, these
among them, and keeps each other module at `info` ([Environment variables](installation.md#environment-variables)).

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
- Remote Control: it stays unavailable behind the router in each setup ([Headers and
  transport](client.md#headers-and-transport)).
- The DesignSync tool: its calls under `/v1/design/` are on no row, so one that carries a router key gets the `403`
  `feature_not_allowed`, and one that carries the token of the tool's own login gets the `401` of a call without a valid
  router key.
- The features in the "Base URL" setup: without a proxy or a redirect the client sends their calls to the upstream with
  the router key, and the router sees none of them.
