# The companion mod

The companion mod is a Claude Code plugin that runs inside the Claude Code process and reports session liveness and
agent state to the router. The router's session engine keeps the record of each session and agent, and the router's
cache warming reads those records to decide which sessions' prompt caches to keep warm ([Keepalive](keepalive.md)).
Installing the mod, its settings, the start check, the `allowance` tool and what the mod reports are in the companion's
part of this site, [Phoebis Companion](../companion/index.md), which starts with [Install and
settings](../companion/install.md). This page holds the router's side: the permissions a key gives its companion, what
the router does with a request of another instance, and where the console shows a session the mod reports.

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

## An instance mismatch

On the router's side, a request whose instance header names another instance than the one that holds the session is
foreign traffic while the holder's lease is live, and a session saved attended reads as live for one lease length after
a router restart, until its registration arrives. The router leaves the session's recipes as they were and, at the
session's first mismatch, writes one WARN line, `a request named another instance than the session's registration`,
naming the session id and never an instance id. Once the lease has lapsed, such a request meets no check and arms its
recipe as the holder's does. The new instance's start still meets the silent holder and takes over as [The instance
header](../companion/reports.md#the-owner-header) states, which replaces the agents those requests armed.

## The Sessions view

On the router's side, a session the mod reports shows `reporting` under Companion in the console's Sessions view
([The mod](keepalive.md#the-mod)). Where the mod also reports facts of the session, the view leads the session's row
with its custom title or its project's folder name, shows its project, its repository and its branch in the open row and
in two columns that are hidden by default, and, where the mod reports a subagent's facts, leads that subagent's line
with its name, else its description ([Sessions](console.md#sessions)). Which facts a session reports is set in the mod
alone, by settings that are off by default ([Configure](../companion/install.md#configure)), and the router has no
setting for them. The router cleans each reported value before it keeps or shows it ([The companion
protocol](relay.md#the-companion-protocol)).
