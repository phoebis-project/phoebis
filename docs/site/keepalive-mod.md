# The keepalive mod

This page is for the operator installing and configuring the keepalive mod on a client machine. The mod is a Claude
Code plugin that reports session liveness and agent state to the router. It runs inside the Claude Code process, and
the router's keepalive engine reads its reports to decide which sessions' prompt caches to keep warm. The mod reports
facts and holds no policy: every ceiling and switch belongs to the router ([Keepalive](keepalive.md)). The page covers
installing the mod, its two options and the variables they fall back to, what it reports, where it runs and how to
verify a checkout.

## Install

Inside Claude Code:

```text
/plugin marketplace add daystrom-labs/phoebis-marketplace
/plugin install phoebis-keepalive@phoebis
```

The install line resolves once the marketplace is added. From a shell, `claude plugin marketplace add` and
`claude plugin install` take the same arguments. For development, `claude --plugin-dir <checkout>` loads a checkout of
[the companion repository](https://github.com/daystrom-labs/phoebis-keepalive) for one session.

The mod needs Claude Code 2.1.287 or later. On an older client it does nothing.

## Configure

The mod needs a URL to report to and the router key to report under. It takes each from the first source that has one:

| Source | Value | Fallback |
| --- | --- | --- |
| `management_url` option | where to report | `ANTHROPIC_BASE_URL`, unless the key comes from `token_file` |
| `token_file` option | a private file holding the router key | `ANTHROPIC_AUTH_TOKEN`, then `ANTHROPIC_API_KEY`, then `CLAUDE_CODE_OAUTH_TOKEN`, unless the key file it names cannot be read |

An empty or blank value counts as unset. Without a URL or without a key from any source, the mod reports nothing. The
three variables rank as Claude Code ranks them when it picks its own credential, so the key the mod reports under is
the key the client sends. When Claude Code already reaches the router through `ANTHROPIC_BASE_URL` with a router key in
`ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN`, both options can stay empty.

With `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1` set, Claude Code checks the organization's policy for plugin network
access before a mod's first fetch, against `api.anthropic.com` with the credential it holds. A router key fails that
check, and every report is then refused inside the process. `CLAUDE_CODE_SUBSCRIPTION_TYPE=max` beside a router key in
`CLAUDE_CODE_OAUTH_TOKEN` marks the session a consumer subscription, which no organization policy governs, so the check
is skipped and the reports go out. Claude Code documents that variable nowhere. [The environment
block](client.md#the-environment-block) on The client carries it, and
[`CLAUDE_CODE_SUBSCRIPTION_TYPE`](client.md#claude_code_subscription_type) there says what the client does with it.

Claude Code asks for both options when it installs the mod, and the `/plugin` menu or `claude plugin configure` changes
them later. They are user-level settings: Claude Code reads a plugin's configuration from the user's settings file, a
`--settings` file or managed settings, and never from a repository's project or local settings file. A repository
therefore cannot change either option, but a project's settings `env` block can set `ANTHROPIC_BASE_URL`, so that URL
serves the environment's own token alone. A key from `token_file` needs `management_url`. With `management_url` empty,
the mod reports nothing and writes one line to Claude Code's debug log asking for it, so the key file's key never
reaches a host a repository picked.

The URL may carry a path prefix, such as `https://gateway.example/router`. The mod drops one trailing slash and appends
the route to what is left. The mod accepts plain HTTP on any host, as the model traffic to the same base URL does. Its
requests go through Claude Code's fetch, so the plugin network access policy described above applies to them and Claude
Code handles their TLS.

`token_file` takes the absolute path of a file whose trimmed text is the key, written without `~`. A file the mod cannot
read makes this process report nothing, with one line in Claude Code's debug log naming the path, until the next session
start reads the file: no variable stands in for it. Reading the key file fails for a misspelled or moved file, for a
path starting with `~/`, which the mod does not expand, and for a relative path in any session whose working directory
does not hold the file, since Claude Code resolves a relative path against that directory. The mod reads the file's text
and cannot read its mode, so keeping that file private (mode `0600`) falls to you.

The backoff, the heartbeat interval and the presence throttle are constants, and the mod has no other setting. A report
that did not get through, for a transport failure, a 5xx or a limit, goes out again along a fixed backoff. A heartbeat
goes out at a share of the remaining lease, within fixed bounds. Both figures are on
[The relay](relay.md#the-keepalive-protocol). A presence report goes out at most once a minute ([Presence](#presence)).

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
| `classic.Stop` | main's turn ended, with the client's list of its background work | a command the list no longer runs leaves its owner's open set |
| `classic.SubagentStop` | a subagent's turn ended, with the same list | the same |
| `turn.complete` of a subagent | the subagent's turn ended | `state parked` on its open set, or `state released` |
| `turn.complete` of main | main's turn ended | `state idle`, or `state parked` with its park |
| `session.end` | the session ends or changes id | `session_end`, one request |
| the heartbeat timer | time passed | the session id check, the reconcile, then `heartbeat` |

A prompt is the person's own when its origin is `bridge` (sent through Remote Control), `composer` (typed at the
prompt), `sdk` (handed over by an SDK host) or `slack-ping` (sent from Slack). The router marks main busy on that prompt
itself, and a prompt of any other origin reports no activity.

A stop event, `classic.Stop` at the end of main's turn and `classic.SubagentStop` at the end of a subagent's, carries
the client's list of its background work: every background command and subagent, with its id, type and status. A
command in an open set that the list reads as ended, whatever ended it, or no longer holds, leaves its owner's open set
there. The mod reads no notification's text: a subagent's end is the agent list's to tell, and a notification that
names a subagent changes nothing, so a subagent that stopped with a background command of its own stays parked on that
command until a stop event's list no longer runs it.

A Bash call counts as a background command when its result carries `backgroundTaskId`. A tool call that another plugin
raises marks no agent busy, although a background command or a cron it starts still counts for the session. A loop the
client's agent list never names, such as a workflow's agent or one of Claude Code's own forks, counts for the agent that
started it, or for main, and is never reported as an agent.

The mod reads `$.session.id()` at session start, at every prompt and at every heartbeat. After a `/clear` or a
`/resume`, `session.end` ends the registered session at once and no `session.start` follows, so the next prompt or
heartbeat that reads the new id registers it under the same instance. Nothing the old session had open or scheduled
carries over. At each heartbeat the mod also releases the subagents the client's agent list reads `completed`, `failed`
or `killed` or no longer lists, and closes the background commands past their deadline. The list describes the agents
the mod held when it asked for it, so a subagent spawned while the answer was out is judged at the next heartbeat, and
an answer that arrives after a `/clear` or a `/resume` replaced the session is dropped. A tool result or a spawn answer
that lands after such a replacement is dropped the same way: the work belongs to the session that started it.

The mod writes its tracked work, each agent's open set, the session's crons and the subagents it released, to the
plugin's host state whenever it changes. A hot reload of the module in the same session reads it back before the
registration's answer is reconciled, so a park the router holds is kept rather than replaced with a fresh busy main,
the commands and crons the earlier load tracked still close and fire, and a released subagent stays released. A new
session id starts from nothing.

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

- a stop event of main or of a subagent whose list of background work no longer runs it
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
module. A reload reuses the pair while the two variables still name it, so the owner line and the instance stay. A child
process inherits the two variables without that state, so it mints its own pair and registers as its own instance. A
child process that never runs the fence keeps the parent's owner line: one on a client older than the floor in
[Install](#install), one with the mod disabled, or one without a URL or a key. That line matters only for a child that
sends the parent's session id.

### What the mod leaves alone

The mod runs no shell command. It hooks two settings-hook events, `classic.Stop` and `classic.SubagentStop`, for the
list of background work they carry, and reads no transcript, no background command's output and no conversation text.
The one file it reads is the key file that `token_file` names.

## Where it runs

The mod runs wherever Claude Code loads the plugin: the Agent SDK, `claude -p`, a cloud session that carries the plugin,
the Desktop app, the terminal and the VS Code extension's chat panel. It behaves the same everywhere: with a URL and a
key it reports unless Claude Code refuses its fetch (see [Configure](#configure)), and without them it does nothing. A
`claude -p` run registers, reports its turns, its background commands and its subagents, and ends its session when it
exits. The mod draws nothing. Its only messages are the conflict line in the transcript and five kinds of line in Claude
Code's debug log: a dropped report, a key file that could not be read, a key file without `management_url`, a refused
key and a report whose fetch threw.

## Verify

From a checkout of the companion repository:

```sh
claude plugin validate --strict .
claude plugin test .
```

The validation passes with no warning. Its listing of the hooks module, with the long lines wrapped here, is the
statement of what the mod does:

```text
❯ ./register.ts hooks: session.start, prompt.submit, prompt.edit, turn.step, tool.call, agent.spawn,
    turn.complete, classic.Stop, classic.SubagentStop, session.end
❯ ./register.ts gating hook with .catch: prompt.submit
❯ ./register.ts gating hook with .catch: tool.call
❯ ./register.ts gating hook with .catch: agent.spawn
❯ ./register.ts gating hook with .catch: classic.Stop
❯ ./register.ts gating hook with .catch: classic.SubagentStop
❯ ./register.ts calls: $.agent.list (via agentList, reconcile), $.clock.after (via acknowledged, deferAgent, delay),
    $.clock.now, $.env.get, $.env.set (via fence), $.fs.read, $.http.fetch, $.session.id (via ensureSession),
    $.session.version, $.state.get (via ensureSession, fence), $.state.set (via fence, persistModel), $.ui.log
❯ ./register.ts env writes: ANTHROPIC_CUSTOM_HEADERS, PHOEBIS_KEEPALIVE_INSTANCE
❯ ./register.ts env reads: ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN, ANTHROPIC_BASE_URL, ANTHROPIC_CUSTOM_HEADERS,
    BASH_DEFAULT_TIMEOUT_MS, BASH_MAX_TIMEOUT_MS, CLAUDE_CODE_OAUTH_TOKEN, PHOEBIS_KEEPALIVE_INSTANCE
❯ ./register.ts state writes: phoebis-keepalive.identity, phoebis-keepalive.model
❯ ./register.ts state reads: phoebis-keepalive.identity, phoebis-keepalive.model
```

The five gating hooks sit where a hook could refuse what was asked. Each passes its event on unchanged, and its
`.catch` passes the event on when the mod fails, so the mod never refuses a prompt, a tool call, a spawn or a stop. The
validation also lists the two state values the mod reads and writes, its identity and its tracked work, which
`types/index.d.ts` declares.

`claude plugin test` runs the cases in `hooks/*.test.ts`. They replay the event logs under `hooks/fixtures/`, recorded
from Claude Code sessions, and hand-built events where no log shows a row, against a stubbed router whose answers each
case scripts, on a clock the case advances. No test opens a socket.

On the router's side, a session the mod reports shows `reporting` under Companion in the console's Sessions view
([The mod](keepalive.md#the-mod)).
