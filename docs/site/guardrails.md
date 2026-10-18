# Guardrails

A guardrail is the operator's filter on the requests a key may send: a named list of conditions over a request, attached
to pools and keys, that records the requests it matches or refuses them before any account is touched. This page is for
the operator blocking a request shape, the client's subagent summary fork first. It covers what a guardrail is, the fork
and the three presets the build ships, every condition the editor offers, the path from observing to enforcing, the test
against captured bodies, what a blocked client receives and what this phase leaves out. The view that edits a guardrail
is on [The console](console.md#guardrails), the admin routes are in the [API reference](api-reference.md), and the
refusal a blocked client meets is on [Refusal codes](client.md#refusal-codes).

## What a guardrail is

A guardrail is a named object of the registry: an id the console derives from the name, a description, an enabled
switch, a mode of `observe` or `enforce`, and one to sixteen conditions in the order the operator wrote them, every one
of which must hold for the guardrail to match. A condition reads one thing about a request: a header, a fact the router
derives from the request (its kind, its request class, its agent type, its client, its model or its user agent), a
member of the parsed body, or the text of a selected message ([The conditions](#the-conditions)). The name and the
description are at most 256 bytes each.

A guardrail applies where it is attached. A pool's guardrails cover every key of the pool and run first, in the pool's
order, and the key's own run after them in the key's order. A key cannot drop a guardrail its pool attached, no
guardrail applies router-wide, and a guardrail attached to both a key and its pool is evaluated once, at its pool
position. A disabled guardrail stays attached and runs nowhere. A new pool and a new key start with no guardrail: the
guardrail dialog's "Applies to" group attaches a saved guardrail to pools and keys, and the pool and key dialogs attach
saved guardrails from their side and arrange the order ([Pools](console.md#pools), [Keys](console.md#keys)).

In observe mode a match is recorded on the request row and the request goes on as if nothing had matched. In enforce
mode a match refuses the request with a 400 before any account is selected ([What a blocked client
receives](#what-a-blocked-client-receives)). Every attached guardrail is evaluated on every request it applies to,
whatever an earlier one answered, so an observed match lands on the row beside an enforced one and a candidate can be
compared with the guardrail in force. The first enforcing guardrail that matches decides the refusal, and the row
records every match in evaluation order with the one that blocked marked.

The evaluation is one step of the relay's admission ([Admission](relay.md#admission)): after the key is authenticated,
the body is read and the conversation is climbed, and before the request's facts are published and before any candidate
account is resolved. It runs for an inference request and a token count and never for a keepalive probe. A blocked
request touches no account, no pin, no hold, no window and no burn, so the slow-down never holds it and it refreshes
nothing ([Slow-down](routing.md#slow-down)). The router reads its guardrails with the registry snapshot, so a save in
the console takes effect on the next request without a restart. Deleting a guardrail removes its attachments and leaves
the request rows that name it, which the Requests view marks `{id} (deleted)`.

## The subagent summary fork

While a subagent runs, Claude Code's Agent tool shows a status line under it. The client writes that line by forking the
subagent's whole conversation every 30 seconds, with one user block appended that asks for a description of the most
recent action in three to five words, under the request class `auxiliary`. The fork reads the subagent's full cached
context for about 24 output tokens, and no client setting turns it off.

The cost is in the cache reads. On one router over three days, 2026-10-13 to 2026-10-16, 9,459 such forks read 2.94
billion cached tokens and 26.3 million input tokens, 22 percent of every cached token the router read in those days and
39 percent of the cached tokens its subagent traffic read. The figures are that router's over those days, since a
session with no running subagent sends no fork and a session with several sends one per subagent every 30 seconds. The
first preset below catches the fork, and the Requests view shows what it costs on your router before any request is
refused ([Observe, then enforce](#observe-then-enforce)).

## The presets

The build ships three presets, each a complete definition the binary carries: an id, a version, a name, a description,
the client versions it was measured on, three conditions and a suggested mode, observe for all three. "Create guardrail"
on the Guardrails view opens a menu with "Blank guardrail" and, under "From a preset", one row per preset in the order
of their ids: Away recap, Prompt suggestion, Subagent status summary. Choosing a row opens the dialog with the preset's
name, mode and conditions filled in and the Description field empty for the operator's own words, and the preset's
explanation reads muted under the line `From the preset {name} v{n}`. The saved guardrail is stamped with the preset's
id and version whatever was changed before saving, so the row and the dialog always say where it came from.

The stamp is read against the shipped table whenever the guardrail is read. Where the conditions differ from the stamped
version's, the row and the dialog show "customized". Where the build ships a newer version of the preset, they show
"update available", and the dialog's Identity group names the version with the button "Update to the current preset",
which replaces the conditions with the current version's and makes the save take that version ("Saving takes version
{m}."). The build never changes a saved guardrail on its own, since the conditions may have been changed on purpose: a
newer preset is an offer the operator takes. Every shipped version stays in the table, so a stamp always resolves, and a
guardrail created without a preset carries no stamp.

`Subagent status summary`, version 1, measured on Claude Code 2.1.296 to 2.1.302, catches the fork of the section above
with three conditions: `request_class` equals `auxiliary`, the body member `tools` is present, and the last user message
matches `^Describe your most recent action in 3-5 words`. Its description reads: "Claude Code forks each running
subagent's whole conversation every 30 seconds to ask for a status line of three to five words. Each fork reads the
subagent's full cached context for about 24 output tokens, and no client setting turns it off. On one router the forks
were 22 percent of every cached token read over three days. Blocking them leaves the Agent tool's status line static and
changes nothing else."

`Prompt suggestion`, version 1, measured on Claude Code 2.1.296 to 2.1.301, catches a second fork of the same shape.
After a turn in an interactive session the client forks the main thread's whole conversation to predict what the user
types next and shows the answer in the input. The appended user block starts `[SUGGESTION MODE: Suggest what the user
might naturally type next into Claude Code.]`, so the preset's third condition matches the last user message against
`^\[SUGGESTION MODE: Suggest what the user might naturally type next`, after the same `request_class` and `tools`
conditions. On the same router over three days, 2026-10-14 to 2026-10-17, 154 such forks read 48.2 million cached tokens
for about seven output tokens each. The client setting `promptSuggestionEnabled` turns the feature off on one machine, a
switch the summary fork has no equivalent of.

`Away recap`, version 1, measured on the same client versions, catches the third. When the user steps away and comes
back, the client forks the main thread's whole conversation to ask for a recap of the session in under 40 words. The
appended block starts `The user stepped away and is coming back. Recap in under 40 words`, and the pattern is `^The user
stepped away and is coming back\. Recap in under 40 words`. Over the same three days 59 recaps read 20.6 million cached
tokens for about 90 output tokens each. The setting `awaySummaryEnabled` turns the feature off on one machine.

## The conditions

A condition is one row of the dialog's Conditions group: a subject, the subject's own cell where it takes one (a header
name, a body path or a message selector), an operator from the subject's row below and, for `equals`, `gt`, `lt` and
`matches`, a value. Every member is text, so the API carries no typed value. A guardrail holds 1 to 16 conditions, and
the admin API refuses a condition it cannot read with a 400 reading `conditions[{i}]: {reason}`, `{i}` counting from
zero, which the dialog shows under row `i + 1`.

| Subject | What it reads | Operators |
| --- | --- | --- |
| `agent_type` | The `x-claude-code-agent-type` hint, absent where the request carries none or its value is not 1 to 256 bytes of ASCII letters, digits and `-./:_` | `absent`, `equals`, `matches` |
| `body` | The value at a path into the parsed body, the grammar below | `absent`, `equals`, `gt`, `lt`, `matches`, `present` |
| `client` | The client class read off the user agent, `claude-code/<entrypoint>` or `other` ([Clients](routing.md#clients)) | `equals`, `matches` |
| `header` | The header `name` names, its first value where that value is visible ASCII, absent where the request carries none | `absent`, `equals`, `matches`, `present` |
| `kind` | `inference` or `count_tokens` | `equals` |
| `message` | The text blocks of the entries a selector picks out of `messages`, the selectors below | `matches` |
| `model` | The canonical model the request is routed under, an undated id resolved to its dated snapshot ([Catalogs](routing.md#catalogs)) | `equals`, `matches` |
| `request_class` | The `x-claude-code-request-class` hint, read as the agent type is | `absent`, `equals`, `matches` |
| `user_agent` | The `User-Agent` header cut to 256 bytes at a character boundary, the cut the request row keeps, absent where the request carries none | `absent`, `equals`, `matches` |

`equals` compares bytes, case-sensitive, so a case-insensitive match is `matches` with `(?i)`. `absent` holds where the
subject reads nothing and `present` where it reads anything. `matches` runs the pattern as an unanchored search, so a
pattern that means the start of the text anchors itself with `^`. A `header` condition's `name` is lowercased when it is
saved and must be a header token, 1 to 128 bytes of letters, digits and the punctuation ``!#$%&'*+.^_`|~-``, since a
name outside that set can never arrive. A `kind` condition compares against one of its two words, which the dialog
offers as Inference and Count tokens.

A body path is a member name followed by any number of segments, each a `.` and a member name or an index in brackets:
`tools`, `max_tokens`, `metadata.user_id`, `system[0].text`, `messages[-1].role`, `tools[*].name`,
`messages[*].content[*].type`. A member name is one or more characters other than `.`, `[` and `]`, so a key with a
space or a hyphen needs no quoting. An index is `[n]` counted from zero, `[-n]` counted from the end with `n` at least
1, or `[*]` for any element. A path is at most 256 characters and 32 segments. The walk reads the parsed body, never the
raw bytes, so an escaped spelling and a raw one read the same. A member segment on a value that is not an object, an
index on a value that is not an array and an index past either end make the path absent. `[*]` fans out over the array's
elements: the condition holds when the rest of the path holds on any element, `absent` holds when no element has it, and
nested `[*]` segments fan out in turn.

`present` and `absent` test whether the path resolves to any value, `null` included. `equals` holds where the value is a
scalar whose JSON text equals the condition's value: a string without its quotes (`fast`), a number as the parser
renders it (`64`, `0.5`), `true`, `false` or `null`. An object or an array never equals. `gt` and `lt` hold where the
value is a number greater than or less than the condition's value, which must itself parse as a number, and fail on
anything else. `matches` holds where the value is a string the pattern finds a match in, and fails on anything else.

A `message` condition names a selector over the `messages` array and a pattern over the text of the selected entries.
The selectors are `any` (every entry, whatever its role), `any_assistant`, `any_user`, `first_user`, `last` (the last
entry, whatever its role), `last_assistant` and `last_user`. Claude Code appends a per-turn control entry of role
`system` after the user's turn, so `last_user` skips it and reads the user's own last entry, while `last` reads the
control entry. The pattern runs on each text block of each selected entry, a string content as one block and an array's
`text` blocks one by one, and the condition holds when one block of one selected entry matches. A body without a
`messages` array, or with no entry the selector picks, fails the condition. Tool results, tool inputs, images and
documents are not read ([What this phase does not do](#what-this-phase-does-not-do)).

A pattern is in Rust's `regex` dialect: linear time in the text it searches, no lookaround, no backreferences, Unicode
on, case-insensitive through an inline `(?i)`. A pattern is at most 2,048 characters and compiles under a 1 MiB size
limit, since a key holds up to 64 guardrails of 16 patterns and a pattern of the presets' shape compiles under a few
kilobytes. The admin API refuses a pattern that does not compile with `conditions[{i}]: pattern does not compile:
{message}` and one past the bound with `conditions[{i}]: pattern is longer than 2048 characters`. The patterns compile
once, when the registry snapshot loads, and the dialog leaves the compile to the router, so a bad pattern is refused on
save.

Inside one guardrail the conditions run in a fixed cost order whatever order they were written in, since the cheap facts
decide most requests before any tree walk: `kind`, `request_class`, `agent_type`, `client`, `model`, `user_agent`,
`header`, then `body`, then `message`. Conditions of one subject keep their written order, and the walk stops at the
first condition that fails. A `body` or `message` condition on a request whose body did not parse fails, and `absent` on
such a body holds, since the tree holds nothing. The written order is the one the dialog shows and the one every refusal
and the test name a condition by.

## Observe, then enforce

A guardrail starts in observe mode, the suggested mode of every preset. Create it, attach it to a pool or a key in its
"Applies to" group, and let an hour of traffic run. Each request the guardrail matched then carries it: on the Requests
view the Guardrail filter in the Outcome group lists every guardrail of the registry and `none` for a request no
guardrail matched, and the drawer's Guardrails section names each match in evaluation order with an `observed` or
`blocked` badge ([Requests](console.md#requests)). The Analytics view's Traffic section carries a Guardrails card with
the observed and blocked counts and the blocked bytes per guardrail over the range ([Analytics](console.md#analytics)),
and the Guardrails view's "Matches, 24h" column reads the same counts for the last day.

When the matches are the requests you meant, run the test of the next section against the captured bodies of the pool or
the key, which answers for each sampled request whether the definition matches and which condition decided. Then switch
the mode: the row's menu offers "Switch to enforce", and the dialog's Mode field does the same. From the next request
on, a match is refused before any account is selected.

A block is a refusal the operator asked for, so the Dashboard's "Error rate, 24h" tile leaves the blocked requests out
of its rate and `Most common` skips the code, while "Requests, 24h" keeps counting them, since the router received them.
The Analytics view's "Errors by code" card and its Traffic table keep the `guardrail_blocked` rows as inventories of
what the router answered, beside the Guardrails card that counts the same requests by guardrail.

## Testing a guardrail

The dialog's Test group runs the form's current conditions, saved or not, against the bodies the ledger captured.
"Sample from" picks a pool or a key and Sample the size, 50, 200 or 500, and "Run test" asks the router for the most
recent requests of that pool or key that have a captured body, newest first, no further back than the longest capture
window (`stats.body_capture_max_hours`), since no older body exists. A keepalive probe is never sampled. The router
evaluates the definition over the bodies in pages of 50 and keeps the verdicts alone, so no body leaves the ledger and a
console read waits for at most one page. The route is `POST /admin/guardrails/test`, which a script calls with a saved
guardrail's id or an unsaved list of conditions ([API reference](api-reference.md)).

The verdicts read `{matched} matched, {unmatched} unmatched, {indeterminate} indeterminate of {sampled} sampled` over a
table of Time, Request and Verdict, the matched rows first and each request linked to its row on the
[Requests](console.md#requests) view. The body conditions, the message conditions and the facts the row records (the
kind, the model, the client, the request class, the agent type and the user agent) evaluate as they would live. A
`header` condition evaluates from the row too where the header is one of the five the row keeps:
`x-claude-code-request-class`, `x-claude-code-agent-type`, `user-agent`, `x-claude-code-compaction` and
`x-claude-code-context-compacted`. Any other header is unknown to the test: the condition is set aside, the rest run,
and a request whose other conditions all held reads `condition {n} needs the live header`, while a request a later
condition fails reads `failed at condition {n}` whatever the header would have said. The hint columns hold the filtered
read the router makes of a hint, so a hint the client sent malformed reads absent in the test and present under the raw
`header` subject live.

An empty sample reads `No captured bodies for {name} in the last {h} hours.` where capture is on, and where the key's
effective capture hours or the pool's read zero, "Body capture is off, so there is nothing to test against. Turn it on
in the key's Capture group." with the link "The pool's keys" to the Keys view filtered to the pool. Capture is a key's
setting, inherited from its pool ([Keys](console.md#keys)), and what the bodies hold and how long they stay is on [Body
capture](statistics.md#body-capture).

## What a blocked client receives

An enforcing match answers HTTP 400 in Anthropic's error shape with the type `invalid_request_error`, the message
`blocked by guardrail "{name}"`, the header `x-phoebis-error-code: guardrail_blocked` and the header `x-should-retry:
false`, so a client that retries its 4xx answers does not send the request again unchanged. The answer names the
guardrail and nothing else about it: the request row records its id, and the drawer shows it. The message never reads
"prompt is too long", the phrase the client reads in a 400 as its compaction trigger, and a guardrail's name may not
contain it. The status and the type are fixed: Claude Code treats a 400 as final and shows it as an API error on a
visible turn, where a 429 or a 5xx would be retried. The row is on [Refusal codes](client.md#refusal-codes) beside the
router's other refusals.

A blocked summary fork is no visible turn. In Claude Code 2.1.296 to 2.1.302 the Agent tool's status line stays at its
last value, the client's debug log carries `[AgentSummary]` skip lines, and no error reaches the transcript. What the
client shows for a blocked prompt suggestion or away recap is not measured, so run those two presets in observe until
the Requests view has shown their matches.

## What this phase does not do

A guardrail blocks, and the edit actions wait for the next phase: a regex replace within a selected text, a dropped
message or member, a set member or a set header each change the bytes toward the upstream, which the billing hash and
the drift checks guard ([Client drift](relay.md#client-drift)). An empty success answer in place of the 400, a shape
that would let a fork fail silently, is deferred with them, and the action word stays `block`.

A condition reads text blocks alone. Tool results, tool inputs, images and documents are not read, since the text blocks
decide the first target and every fork's marker is a text block. A guardrail has no logical OR, since two guardrails
give one and the editor's one row per condition stays flat. No guardrail reads a response, since the block runs before
any account is touched and a response-side rule would act after the window was spent. No guardrail applies router-wide
and no key opts out of its pool's, so a guardrail runs where it is attached and nowhere else.

The record is the count and the bytes. The Analytics card reports the blocked requests' bytes and no token estimate of
what a block saved, since a token figure would be a guess over a body the upstream never read. A block writes no
routing-log record, since the request row and its matches are the record, and no route lists a guardrail's matches,
since the Requests view with the Guardrail filter lists them. The dry run has no rate limit, since its cap of 500
requests, its pages of 50 and the ledger's one reader connection bound the work. No preset covers the client's `short`
summary mode, since no router the presets were measured on has seen it.
