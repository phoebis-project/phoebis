# The companion mod

This page is for the operator installing and configuring the companion mod on a client machine. The mod is a Claude Code
plugin that runs inside the Claude Code process. It reports session liveness and agent state to the router, which the
router's keepalive engine reads to decide which sessions' prompt caches to keep warm, checks the router at session start
and names a misconfiguration in the session, and gives the model one tool, `allowance`, which reads what the key's pool
has left so the model can pace its own work. The mod reports facts and holds no policy: every ceiling and switch belongs
to the router ([Keepalive](keepalive.md)), and what a key's companion may do is a permission set on the key ([The key's
permissions](#the-keys-permissions)). The page covers installing the mod, its four settings and the variables they fall
back to, the start check, the tool, what the mod reports, where it runs and how to verify a checkout.

## Install

Inside Claude Code:

```text
/plugin marketplace add phoebis-project/phoebis-marketplace
/plugin install phoebis-companion@phoebis
```

The install line resolves once the marketplace is added. From a shell, `claude plugin marketplace add` and
`claude plugin install` take the same arguments. For development, `claude --plugin-dir <checkout>` loads a checkout of
[the companion repository](https://github.com/phoebis-project/phoebis-companion) for one session.

The mod needs Claude Code 2.1.287 or later. On an older client it writes one transcript line, `phoebis-companion: Claude
Code <version> is older than 2.1.287, so the mod does nothing`, raises a toast with the same text when Start check
toasts is on, and does nothing more. The `allowance` tool needs 2.1.293 or later: on a client between the two versions
the mod reports and runs its start check, and writes one line to Claude Code's debug log in place of registering the
tool.

## Configure

Claude Code asks for the mod's settings when it installs the mod, and the `/plugin` menu changes them later. The mod
needs a URL to report to and read from, and the router key to do so under. It takes each from the first source that has
one:

| Setting | Default | Fallback when empty |
| --- | --- | --- |
| Management URL (`management_url`): where to report and to read | empty | `ANTHROPIC_BASE_URL` |
| Router key (`token`): the key, kept in Claude Code's secure storage | empty | `ANTHROPIC_AUTH_TOKEN`, then `ANTHROPIC_API_KEY`, then `CLAUDE_CODE_OAUTH_TOKEN` |
| Cache warming reports (`keepalive`): whether the lifecycle reports go out | on | none |
| Start check toasts (`notifications`): whether a start check's transcript line also raises a toast | on | none |

An empty or blank value counts as unset. The three variables rank as Claude Code ranks them when it picks its own
credential, so the key the mod reports under is the key the client sends. When Claude Code already reaches the router
through `ANTHROPIC_BASE_URL` with a router key in `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY` or
`CLAUDE_CODE_OAUTH_TOKEN`, both fields can stay empty. Without a URL or without a key from any source the mod writes one
transcript line, `phoebis-companion: no management URL from the option or ANTHROPIC_BASE_URL` or `phoebis-companion: no
router key from the option or the environment`, and does nothing else. A URL from either source that fails the check
below writes `phoebis-companion: the management URL from the option or ANTHROPIC_BASE_URL is not usable (<reason>)`
instead, and the mod does nothing else either.

A key set in Router key goes to the Management URL alone and never to the host `ANTHROPIC_BASE_URL` names. The settings
are user-level: Claude Code reads a plugin's configuration from the user's settings file, a `--settings` file or managed
settings, and never from a repository's project or local settings file. A repository therefore cannot change a setting,
but a project's settings `env` block can set `ANTHROPIC_BASE_URL`, so that URL serves the environment's own token alone.
With Router key set and Management URL empty, the mod sends nothing and writes `phoebis-companion: the token setting
needs the Management URL, so this session reports nothing` to the transcript.

The URL may carry a path prefix, such as `https://gateway.example/router`. The mod drops one trailing slash and appends
the route to what is left. It accepts plain HTTP on any host, as the model traffic to the same base URL does. A save of
the Management URL is refused with `an http or https URL with a host` for a value that is not one (an empty value is
accepted, since empty means the environment), with `a URL with a username or password` for a credential inside the URL,
with `a URL with a query` for a `?`, and with `a URL with a fragment` for a `#`. The session start runs the same check
on the URL it resolves, from the option or from `ANTHROPIC_BASE_URL`, so a value edited by hand into a settings file or
a base URL with a query or a fragment reaches no request, and the start line names the reason. The Router key is trimmed
when it is saved. The mod's requests go through Claude Code's fetch, so the plugin network access policy described below
applies to them and Claude Code handles their TLS.

With `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1` set, Claude Code checks the organization's policy for plugin network
access before a mod's first fetch, against `api.anthropic.com` with the credential it holds. A router key fails that
check, and every report and read is then refused inside the process. `CLAUDE_CODE_SUBSCRIPTION_TYPE=max` beside a router
key in `CLAUDE_CODE_OAUTH_TOKEN` marks the session a consumer subscription, which no organization policy governs, so the
check is skipped and the requests go out. Claude Code documents that variable nowhere. [The environment
block](client.md#the-environment-block) on The client carries it, and
[`CLAUDE_CODE_SUBSCRIPTION_TYPE`](client.md#claude_code_subscription_type) there says what the client does with it.

With Cache warming reports off, the mod sends no keepalive event, and the start check and the tool still run. The
backoff, the heartbeat interval, the presence throttle, the start check's wait and the tool's cache are constants, and
the mod has no other setting. A report that did not get through, for a transport failure, a 5xx or a limit, goes out
again along a fixed backoff. A heartbeat goes out at a share of the remaining lease, within fixed bounds, and both
figures are on [The relay](relay.md#the-keepalive-protocol). A presence report goes out at most once a minute
([Presence](#presence)).

## The start check

At session start, once the settings resolve to a URL and a key, the mod sends one read of the allowance route,
`GET /v0/companion/allowance?session_id=<id>` ([The companion routes](relay.md#the-companion-routes)), before and apart
from the keepalive registration, and waits up to three seconds for the answer. The answer settles three things at once:
whether a router answers at the URL, whether the key is valid, and whether the key may read the allowance.

| Answer | What the mod does |
| --- | --- |
| `200` with the allowance body | Registers the tool and keeps the body as the first entry of the tool's cache. No line. |
| `403` with the code `companion_not_permitted` | Registers no tool, since the operator withheld the permission, and writes `phoebis-companion: this key may not read the allowance` to Claude Code's debug log alone. The lifecycle reports still go out. |
| `401` | Writes `phoebis-companion: the router at <url> refused the key (401), so this session reports nothing` to the transcript, registers no tool, and empties the keepalive queue, as a `401` on the keepalive route does. |
| anything else | Writes `phoebis-companion: no answer from the router at <url> (<status, failure or "not the allowance">), the allowance tool answers once it is back` to the transcript and registers the tool anyway. |

The last row covers a transport failure, a `5xx`, another `4xx`, the three-second limit, and a `200` whose body is not
the allowance, such as the console page the admin listener serves for an unknown path when the Management URL names the
admin listener in place of the proxy listener. The tool is registered anyway because the tool list sits at the front of
the prompt's cached prefix: a tool that appears later in the session rewrites the whole prefix once, and a resumed
session pays for its whole conversation. While the router stays unreachable, a call of the tool answers the failure's
text. The keepalive part retries along its backoff as [The relay](relay.md#the-keepalive-protocol) states.

With Start check toasts on, each transcript line the mod writes at session start, the three lines
[Configure](#configure) names for an unset setting included, also raises a toast with the same text. A failure text, the
URL included, has its secrets redacted before it reaches a transcript line, a toast or a tool result, since a fetch
error can carry the bearer.

A reload of the plugin runs the start check again, with the tool's cache cleared first, so a reload under a changed URL
or key never answers the earlier configuration's body, and registers the same tool under the same name, which replaces
the registration without a second entry in the tool list. A reload or a later start whose check does not refuse the key
clears the refusal an earlier `401` left and registers the session again, so a key fixed in the `/plugin` menu needs no
restart. A `401` on the keepalive route also unconfigures the tool: until such a start, a call answers that the session
has no usable router configuration, and sends no request. A `403` on the keepalive route empties the queue as a `401`
does but leaves the tool reading: it comes from something in front of the router, so it refuses the request and not the
key. It says nothing about whether the key may read the allowance, which the tool's own route answers.

## The allowance tool

The mod declares one tool, `allowance`, which takes no input and sits in the prompt's tool list, so the model sees it
from its first turn and calls it as `mcp__phoebis-companion__allowance`. A call answers the router's JSON body as text,
from a cache of ten seconds or a fresh read of the allowance route, so a model that calls twice inside ten seconds reads
one answer and the router sees one request. A read that fails answers a short text naming the failure, marked as an
error for the model and never thrown, and only a body that is the allowance is cached. The tool's description tells the
model what the answer holds and when a call pays: before a long autonomous run, before a fan-out of subagents, and after
a turn that felt slow. It proposes no model or effort change and names no account by anything but its pseudonym.

The answer is the key's pool as the router reads it at that instant. The protocol page lists every field with its type
([The companion routes](relay.md#the-companion-routes)). This section says what the model learns from each part.

`pool` carries the pool's `pinning` mode and its `strategy`. `accounts` holds one entry per active account of the pool
under its pseudonym `id`, with its `funding` (`plan`, `usage_based` or `api_key`), its rate-limit `tier`, the
`obstacle`, a hold the selector keeps on the whole account at that instant (`budget`, `budget_unknown`, `cooldown`,
`overage` or `spent`, the last for a usage-based seat at its spend cap, and `null` with no such hold, where each
window's `fit` says whether new work lands), the `slow_down` in force on it or `null`, its `windows`, and its `spend`,
one row per spend budget the ledger has reported for the account, with the period, the dollars spent, the budget
and the period's end.

`windows` holds one entry per plan window the account has a reading of, keyed by the window id (`five_hour`,
`seven_day`, or a model's own week, as [The windows the router works from](routing.md#the-windows-the-router-works-from)
names them), and is empty for an API-key account. `utilization` is the share used now, from 0 to 1 of the window.
`burn_per_hour` is the share the account's sessions burn in an hour, and `demand_per_hour` what they would burn with no
slow-down holding them. `horizon_at` is the window's reset, or the account's subscription end when that comes first, and
`null` where neither is known. `projected_at_horizon` is the predicted share at the earlier of `horizon_at` and five
hours from now. `ceiling` is the share of the window the pool fills an account to, the pool's 5-hour ceiling on
`five_hour` and its weekly ceiling on the other windows. `fit` is the word a fresh placement reads on the window for a
request that prices no rewrite: `fits` where a fresh session at the pool's usual burn stays under the ceiling until the
earlier of `horizon_at` and five hours from now, `rewrite_only` where a request still lands under the ceiling and the
usual burn after it reaches the ceiling before that instant, `least_bad` at or over the ceiling, `ending` where the
window has allowance left and the account is inside its wind-down, and `spent` where the share stands at 1.0 or more, so
the window holds no allowance until `horizon_at`. `runway_minutes` is the whole minutes until the share reaches the
pool's ceiling at the current burn, with the band's hours weighed as the projection weighs them ([The windows the router
works from](routing.md#the-windows-the-router-works-from)): `0` at or above the ceiling, a number while the burn would
reach the ceiling before the window resets and before an end of the account's subscription that lies ahead, and `null`
where nothing bounds the runway, because nothing burns or because the window resets or the subscription ends first.

`pins` holds one entry per pin of this session under the key: the `agent` (`main` or the subagent's id), the `model`,
the `account` by its pseudonym, when the pin was placed and last used, `warm` and `warm_until` for its cache entry, its
`runway_minutes` on the window that binds it (`binding_window`), read by the same rule as a window's with the account's
subscription end counted only while it lies ahead, and its `slow_down` figures.

A model reads the answer for pace alone. A window's `fit` says whether new work has room on it, and `fits` leaves room
for a long run or a fan-out. A `null` runway means that nothing burns, or that the window resets or the account's
subscription ends before the share reaches the ceiling. A short runway on the account a pin names means the pool is
close to its ceiling on that window at the current burn, and the model can finish its work before then, wait for the
reset where `horizon_at` names one, or spread its work out. Where `horizon_at` names the account's subscription end, the
quota left on the window is lost there. The router keeps the final say on placement: the tool steers no placement, no
pin and no shedding round.

The accounts are pseudonymous. An account's `id` is eight lowercase hexadecimal characters, a keyed digest of the
account's id under a key the router derives from its storage key, so the pseudonym is the same across sessions and
router restarts on one installation, differs between two installations, and reverses to nothing. The answer names no
label, email, account id, key id or notes field. The console shows each account's pseudonym as its Companion id on the
account's details, so an operator can tell which account the model named ([Accounts](console.md#accounts)).

## What the mod reports

The mod reports on main, the session's own agent, and on each subagent the client lists. Each agent has an open set: the
background commands and the subagents it started that have not ended yet. The session's crons and wakeups belong to
main. Every report is one request to the router's keepalive route, and the route, the six events with their fields, the
acknowledgment, the refusal codes and how the mod queues, sends and retries are on
[The relay](relay.md#the-keepalive-protocol).

| Mod event | Fact | Report |
| --- | --- | --- |
| `session.start` | the process is up | the owner fence, then `session_start` |
| `prompt.submit` from `bridge`, `composer`, `sdk` or `slack-ping` | a real prompt | `activity` kind `prompt` for main |
| `prompt.submit` from `scheduled-trigger` | a cron or a wakeup fired | it leaves the crons or moves to its next fire |
| `prompt.edit` | the person is typing | `activity` kind `presence` for main, at most once a minute |
| `turn.step` | a model request of main or a subagent | `state busy` when the agent was not busy |
| `tool.call` before the tool runs | a tool runs | `state busy` when the agent was not busy |
| `tool.call` after Bash | a command started in the background | it joins the agent's open set with its deadline |
| `tool.call` after CronCreate or ScheduleWakeup | a wakeup is scheduled | it joins the crons with its next fire |
| `tool.call` after CronDelete | a cron is gone | it leaves the crons |
| `agent.spawn` | a subagent started | `agent_start`, and it joins its parent's open set |
| `classic.Stop` | main's turn ended, with the client's background list | a command the background list no longer runs leaves its owner's open set |
| `classic.SubagentStop` | a subagent's turn ended, with the background list | the same |
| `turn.complete` of a subagent | the subagent's turn ended | `state parked` on its open set, or `state released` |
| `turn.complete` of main | main's turn ended | `state idle`, or `state parked` with its park |
| `session.end` | the session ends or changes id | `session_end`, one request |
| the heartbeat timer | time passed | the session id check, the reconcile, then `heartbeat` |

A prompt is the person's own when its origin is `bridge` (sent through Remote Control), `composer` (typed at the
prompt), `sdk` (handed over by an SDK host) or `slack-ping` (sent from Slack). The router marks main busy on that prompt
itself, and a prompt of any other origin reports no activity.

A stop event, `classic.Stop` at the end of main's turn and `classic.SubagentStop` at the end of a subagent's, carries
the client's background list: every background command and subagent, with its id, type and status. A command in an open
set that the background list reads as ended, whatever ended it, or no longer holds, leaves its owner's open set there.
The mod reads no notification's text: a subagent's end is the agent list's to tell, and a notification that names a
subagent changes nothing, so a subagent that stopped with a background command of its own stays parked on that command
until a stop event's background list no longer runs it.

A Bash call counts as a background command when its result carries `backgroundTaskId`. A tool call that another plugin
raises marks no agent busy, although a background command or a cron it starts still counts for the session. A loop the
client's agent list never names, such as a workflow's agent or one of Claude Code's own forks, counts for the agent that
started it, or for main, and is never reported as an agent.

The mod reads `$.session.id()` at session start, at every prompt and at every heartbeat. After a `/clear` or a
`/resume`, `session.end` ends the registered session at once and no `session.start` follows, so the next prompt that
reads the new id registers it under the same instance. Nothing the old session had open or scheduled carries over. At
each heartbeat the mod also releases the subagents the client's agent list reads `completed`, `failed` or `killed` or no
longer lists, and closes the background commands past their deadline. The agent list describes the agents the mod held
when it asked for it, so a subagent spawned while the answer was out is judged at the next heartbeat, and an answer that
arrives after a `/clear` or a `/resume` replaced the session is dropped. A tool result or a spawn answer that lands
after such a replacement is dropped the same way: the work belongs to the session that started it.

The mod writes its tracked work, each agent's open set, the session's crons, the subagents it released and those the
router last named released, to the plugin's host state whenever it changes. A hot reload of the module in the same
session reads it back before the registration's answer is reconciled, so a park the router holds is kept rather than
replaced with a fresh busy main, the commands and crons the earlier load tracked still close and fire, and a released
subagent stays released. A new session id starts from nothing.

### Parks

When main's turn ends, main reports `idle`, or `parked` with a `park_kind`:

- `background` while its open set holds anything
- `scheduled` while a cron or a wakeup is pending
- `both` while both hold

`wake_at` is the earliest time the client fires one of the session's crons or wakeups, in epoch milliseconds. The mod
computes it as the client's scheduler does, from the cron expression, the time of creation or of the last fire, and the
cron's id. A wakeup's `wake_at` is the `scheduledFor` time its ScheduleWakeup call answered. `deadline` is the latest
deadline among the open background commands, in epoch milliseconds, and the report carries it only while every open
entry is a command, because a subagent has no deadline.

A background command's deadline is its start plus the call's `timeout`, or plus 30 minutes when the call names none, and
at most 2 hours after its start. These are the limits at which the client stops a background command, and the two
variables only raise them: `BASH_DEFAULT_TIMEOUT_MS` above 30 minutes becomes the default, and the larger of
`BASH_MAX_TIMEOUT_MS` and `BASH_DEFAULT_TIMEOUT_MS`, when it is above 2 hours, becomes the cap. The mod reads both at
session start. The router reads whole milliseconds, so the mod rounds the deadline of a fractional `timeout` up to the
next whole millisecond.

A subagent parks the same way on its own open set when its turn ends. A subagent whose turn ends with nothing open
reports `released`, since its work went back to its parent, and its next turn under the same id starts it again with
`agent_start`.

An entry leaves its open set on the first of these:

- a stop event of main or of a subagent whose background list no longer runs it
- the end of a subagent: its release at its turn end, or the client's agent list reading it ended or no longer naming it
- the first heartbeat after a background command's deadline
- main's next real prompt, for every command an earlier turn of main started, still running or not

A long-running command, such as a dev server, therefore parks main only until main's next real prompt. A prompt typed
into main's running turn keeps the commands that turn started. A cron leaves the crons when CronDelete removes it or
when it fires as a one-shot, however far away that is, since a one-shot has no age limit. A recurring cron moves to its
next fire each time it fires, until the fire at which it is seven days old: the client removes it after that fire, and
so does the mod. A new ScheduleWakeup replaces the pending wakeup, and one called with `stop` cancels it.

Two wakeups never reach the mod: the fallback wakeup Claude Code arms on its own when the model does not reschedule a
`/loop` turn, and the durable crons a previous process left in `.claude/scheduled_tasks.json`. While one of them is
pending, main reads `idle`, and the router's idle ceiling ([The ceilings](keepalive.md#the-ceilings)) governs how long
the cache stays warm for it.

### Presence

`prompt.edit` fires while the person types at the prompt. The mod reports it as `activity` kind `presence` for main, at
most once a minute. The router resets main's idle clock on a presence and does nothing else with it, so no agent turns
busy and no state changes.

### The owner header

At session start the mod fences the process: it mints two v4 uuids, the owner id and the instance id, and writes one
`X-Keepalive-Owner-Id: <owner id>` line into `ANTHROPIC_CUSTOM_HEADERS`. That line replaces any owner line already
there, whatever the case of the header's name, and the variable keeps its other lines. Claude Code sends the header on
every model request of the process, so the router can tell this process's requests from another process's on the same
session id. The mod keeps the instance id in `PHOEBIS_KEEPALIVE_INSTANCE`. Every report names the instance, and every
`session_start` names the owner as `owner_id`, which no other report carries. A `/clear` or a `/resume` keeps both ids,
since the owner is the process.

Claude Code holds the pair in the plugin's own state for the session, and that state survives a hot reload of the
module. The mod also keeps the pair in the plugin's store under the key `identities`, beside the identity of the client
process it was minted in, for the newest eight processes. The store outlives the session, so a reload after a `/clear`
or a `/resume`, under either id, finds the pair there when the session's state holds none. A reload reuses the pair
while the two variables still name it, so the owner line and the instance stay, and the router's session keeps its
owner. A child process inherits the two variables but runs as another process, for which the store holds no pair, so it
mints its own pair and registers as its own instance. Where Claude Code cannot run the command that reads the process's
identity, a reload after a `/clear` mints a new pair. A child process that never runs the fence keeps the parent's owner
line: one on a client older than the floor, where the mod writes only the line [Install](#install) names, one with the
mod disabled, or one without a URL or a key. That line matters only for a child that sends the parent's session id.

A `session_start` the router refuses with a bare `instance_conflict`, since another instance holds the session under a
live lease, leaves the session live. The mod writes one line to the transcript, `phoebis-companion: another instance
reports session <id>, so the start is retried once its lease lapses`, and sends the start again under its backoff, from
one second up to thirty and every thirty seconds from then on. Once the holder's 90-second lease lapses, the router
names the silent holder, and the mod's next start takes the session over with `previous_instance_id`: the router
replaces the session's agents, the next requests arm recipes again, and the mod writes `phoebis-companion: took over
session <id> from a silent instance`.

On the router's side, a request whose owner header names another owner than the session's registration is foreign
traffic while the holder's lease is live, and a session saved attended reads as live for one lease length after a router
restart, until its registration arrives. The router leaves the session's recipes as they were, counts the mismatch, and
at the session's first mismatch writes one WARN line, `a request named another owner than the session's registration`,
naming the session id and never an owner id. Once the lease has lapsed, such a request takes the session over: the
router records the request's owner as the session's, keeps it across a restart and arms the recipe as it does for the
registered owner. The new instance's start still meets the silent holder and takes over as above, which replaces the
agents those requests armed.

### What the mod leaves alone

The mod runs one shell command at load, `echo $PPID`, to read the client process's identity, and no other. It hooks two
settings-hook events, `classic.Stop` and `classic.SubagentStop`, for the background list they carry, and reads no
transcript, no background command's output and no conversation text. It reads no file. It keeps the key `identities` in
its own store, which Claude Code holds as a file under the user's configuration directory, and its four settings live
where Claude Code keeps a plugin's configuration, the Router key in its secure storage.

## The key's permissions

What a key's companion may do is a permission set on the key, two words in this version: `allowance`, whether the
companion may read the allowance route, and `keepalive`, whether the router warms the key's sessions' caches from the
companion's reports. A new key starts with both. In the console the key dialog's Sessions group holds the two as
toggles, "Allowance tool" ("The model may read the pool's allowance through the companion.") and "Cache warming" ("The
router warms this key's sessions' caches."), and the Keys table notes `allowance off` or `cache warming off` under a
key's name for each word that is off ([Keys](console.md#keys)). In the admin API the key resource carries
`companion_permissions`, the list of the words that are on ([Keys](routing.md#keys)).

With `allowance` off, the route answers the companion `403` with the code `companion_not_permitted`, and the mod
registers no tool, with one line in Claude Code's debug log. With `keepalive` off, the router accepts the key's reports
and probes nothing for its sessions: no recipe is retained, and the Sessions view names the cause as
`key <name> permits no cache warming` ([Turning it on](keepalive.md#turning-it-on)). The mod itself sends its reports
either way, since the key's settings live on the router alone.

## Where it runs

The mod runs wherever Claude Code loads the plugin: the Agent SDK, `claude -p`, a cloud session that carries the plugin,
the Desktop app, the terminal and the VS Code extension's chat panel. It behaves the same everywhere: with a URL and a
key it reports, checks and answers the tool unless Claude Code refuses its fetch (see [Configure](#configure)), and
without them it writes its one transcript line and does nothing else. A `claude -p` run registers, reports its turns,
its background commands and its subagents, and ends its session when it exits. The mod draws nothing beyond the toasts
of the start check. Its messages are the version line of [Install](#install), the transcript lines of
[Configure](#configure) and [The start check](#the-start-check), the conflict line and the takeover line of [The owner
header](#the-owner-header), and the lines in Claude Code's debug log: a dropped report, a refused key, a report whose
fetch threw, a start hook that threw, the allowance permission the operator withheld, the client process's identity read
at load, and the tool left unregistered on a client older than 2.1.293.

## Verify

From a checkout of the companion repository:

```sh
claude plugin validate --strict .
claude plugin test .
```

The validation passes with no warning. Its listing of the hooks module is the statement of what the mod does: the events
it hooks, the engine calls it makes, the variables it reads and writes (it writes `ANTHROPIC_CUSTOM_HEADERS` and
`PHOEBIS_KEEPALIVE_INSTANCE` and no other), and the state values `types/index.d.ts` declares. The gating hooks sit where
a hook could refuse what was asked, on `agent.spawn`, `classic.Stop`, `classic.SubagentStop`, `prompt.submit` and
`tool.call`. Each passes its event on unchanged, and its `.catch` passes the event on when the mod fails, so the mod
refuses no prompt, tool call, spawn or stop. The settings hooks on `config.set` refuse a Management URL as
[Configure](#configure) states and nothing else.

`claude plugin test` runs the cases in `hooks/*.test.ts`. They replay the event logs under `hooks/fixtures/`, recorded
from Claude Code sessions, and hand-built events where no log shows a row, against a stubbed router whose answers each
case scripts, on a clock the case advances. No test opens a socket.

On the router's side, a session the mod reports shows `reporting` under Companion in the console's Sessions view
([The mod](keepalive.md#the-mod)).
