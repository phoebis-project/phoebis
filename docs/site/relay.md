# The relay

What the router changes on the wire between Claude Code and Anthropic, what it sends on its own, its contract with the
keepalive mod and how long it waits on an upstream. The client's environment block, the refusal table and the models
listing are on [The client](client.md), the cooldown tables on [Pools, keys and
routing](routing.md#cooldowns-and-failover), and the mod's install and settings on [The keepalive
mod](keepalive-mod.md).

## The frame

The request the upstream receives is the one Claude Code sent, header order and spelling included ([Headers and
transport](#headers-and-transport)). The router edits the body and the head where the account requires it (the
credential, the account's identity, the OAuth claim), where the operator switched an edit on (a pool field or a
bootstrap key), and where it owns the value (the framing headers, the folded model, the restored billing fields). Three
request kinds pass the same handler: an inference request (`POST /v1/messages`), a token count (a path ending in
`/count_tokens`) and a probe, the keepalive engine's replay of a kept request ([How a probe
works](keepalive.md#how-a-probe-works)).

A request body arrives as `gzip` or identity, and the router inflates a gzip body before any read or edit, so every
check and every edit sees the plain bytes. A token count gets the `safeguards` cut, the model fold, the credential, the
OAuth claim and the auto mode beta drop, and none of the uuid splice, the cache TTL rewrite, the `diagnostics` member,
the billing restoration or `x-client-request-id`: its public schema has no `metadata` member for the identity to go in,
and it is not billed. Fast mode changes no byte: `speed` travels as sent, and a fast request differs in its cache lane
key and in the fast lane's own cooldown. The models listing is not relayed at all: the router builds it from the
accounts' catalogs ([The models listing](client.md#the-models-listing)).

## Admission

Before a request is relayed, the handler admits it in a fixed order, and each step refuses with the code its row of
[Refusal codes](client.md#refusal-codes) names, so a refused request's row in the ledger says which refusal turned it
away. An inference request and a token count take every step. A keepalive probe's key is read by its id, its body is the
router's own, and the guardrails never evaluate it.

The key comes first: the credential header is read (`auth_missing`, `auth_malformed`, `auth_ambiguous`) and the key it
names is looked up, with a key that is unknown, revoked, retired, disabled or expired, or whose pool is disabled or
gone, refused as `auth_unknown`. The class gate follows, off the user agent alone: any client outside Claude Code is
refused (`client_not_allowed`) before a byte of the body is taken off the wire, under every key. The content coding must
be gzip or identity (`unsupported_content_encoding`).

The body is read under its bounds: the key's slot for arriving bodies (`key_body_reads`), the declared length against
`proxy.max_request_bytes` (`request_too_large`), the process's byte budget (`body_budget_exhausted`), the read deadline
of `proxy.body_read_seconds` (`request_timeout`), a connection that closes before the body ends (`request_incomplete`)
and a body that runs past the bound (`request_too_large`). A gzip body is then inflated under the same bound
(`body_budget_exhausted` where the budget cannot hold the inflated bound, `invalid_body` where the bytes are not valid
gzip, and `request_too_large`), so every later read and edit sees the plain bytes.

The body's facts are read once, the parsed tree beside them, and the ledger's standing is checked before the first fact
is published, so no request is accepted that the writer cannot record (`ledger_behind`). The repeated-member check
refuses a body that carries `metadata`, `model`, `stream` or `system`, the first system block's `text` or
`metadata.user_id` more than once (`body_shape`), and a body that nests deeper than the relay reads or is not a Messages
request at all (`invalid_body`), since the relay edits the first spelling of a member and a last-wins parser upstream
would act on the second ([The body edits](#the-body-edits)).

The model and the identity are read from the body and the headers (`missing_model` for a body without a model,
`invalid_model` for one that is not a usable model name), the catalog resolves the canonical model the request is routed
under, and the conversation climb reads the boundary chains to place the request in its conversation ([The boundary
chains](#the-boundary-chains)).

Then every guardrail attached to the pool and then the key is evaluated, in their order, before the facts are published,
so the facts carry the matches, and an enforcing match then refuses the request with `guardrail_blocked` before any
account is read ([Guardrails](guardrails.md)). A key with no guardrail pays one lookup here and nothing else.

The candidates are resolved from the key's model rules (`model_not_allowed`).
A Claude Code inference request that lacks an item of [the required
set](client.md#the-required-set) is then refused (`client_contract`), before any account is read. The drift checks judge
the billing stamp and the version suffix once, before any account is selected (`client_drift`, [Client
drift](#client-drift)), and the selection takes over: the session's pin or the pool's strategy, whose own refusals are
on [When nothing can serve](routing.md#when-nothing-can-serve).

## The body edits

The edits run in this order on every leg, on the plain bytes, and each fails closed: a body whose member an edit cannot
find or walk is refused rather than sent on under the account's credential ([refusal codes](client.md#refusal-codes)).

| Edit | Purpose | Request kinds | Account kinds | Switch and its home |
| --- | --- | --- | --- | --- |
| Cut the top-level `safeguards` member | Keep the upstream's per-tool review off when the operator turned auto mode off | All three | Both | `auto_mode_server`, the key's field, else the pool's, on when the key has no pool |
| Splice the account's uuid into `metadata.user_id` on an OAuth leg, or blank it on an API-key leg (the empty uuid the client itself writes on an API key) | Bill the request to the selected account, and send an API-key leg the shape the native client sends | Inference and probe (the `count_tokens` schema has no `metadata`) | Both | None |
| Replace `model` with the folded spelling: the `[1m]` suffix removed, ASCII lowercased | The long-context marker is a client-side routing hint | All three | Both | None |
| Restore the absent fields of the billing line at `system[0].text`: `cch=00000` on every line, `cc_prev_req={id}` on a conversation class and on a conversation-shaped fork, and `cc_prompt_id={id}` from the client's `x-claude-code-prompt-id` header, each at its place in the client's own field order | Give a reduced-shape request the first-party fields, never rewriting a present one | Inference from a `claude-cli/` agent whose version the client itself would spell, when the line is there and `system` is an array | Both | None, but a field an account refused once is latched off for that account |
| Insert the top-level `diagnostics` member before `stream` (else last), naming the previous message id on this account or `null` | Ask the upstream for cache diagnostics, naming `null` on the first request after a move and on a probe | Inference and probe, where the client sent cache markers and at least one tool and no `diagnostics` of its own | Both | The pool's `cache_diagnostics` |
| Set a present `diagnostics.previous_message_id` to `null` | Name no message the account cannot find | The first request a session sends to an account other than the one its previous request landed on, whatever model the two requests named (a move, a failover's leg or its landing, a pin released or passed over, a session that switched model between its turns), an auxiliary request reading its own model's landing alone and a pin only auxiliary requests have used counting as no other model's landing, and every probe | Both | None |
| Rewrite every ephemeral `cache_control` marker to the policy's TTL (`1h` writes `"ttl":"1h"` after `"type":"ephemeral"`, `5m` removes `ttl`), and under `inject_when_absent` add a top-level marker to a body with none, never on an auxiliary request | Apply the pool's cache TTL policy | Inference and probe | Both: the `metered` rules for an API-key account or an account in overage, else the `plan` rules, per model and per main or subagent branch | The pool's cache TTL policy ([Cache TTL](routing.md#cache-ttl)), off by default |
| Re-sign the `cch=` hash over the edited bytes | Keep a client's own hash valid after the edits moved the bytes it covered | All three, where the client stamped a real hash, and a restored placeholder only under the switch | Both | `proxy.sign_cch_placeholder` in the bootstrap file, default `false`, for the restored placeholder alone |
| Gzip the edited body, or send it as identity | The outbound coding | All three | Both | `proxy.upstream_gzip` in the bootstrap file: `auto` mirrors the client, `on` always gzip, `off` never |

The restored placeholder travels unsigned because the client itself sends `cch=00000` wherever its native patch does not
run, and the upstream accepts it. `proxy.sign_cch_placeholder` signs it with this build's formula instead, for an
upstream that rejects the placeholder, and is off until such a rejection shows. A client's own hash is re-signed
whatever the switch says.

A latched refusal: an upstream `400` naming a field or a beta this leg injected strips it for one more try on the same
account and latches it off for that account ([How a head is classified](#how-a-head-is-classified)). The latch is keyed
on the account row, so an edit of the account in the console clears it, and a restart forgets it. The session memory
behind `cc_prev_req` holds what the router last saw for a key, a session and an agent, in process and written behind to
the registry, and is kept for `sessions.memory_retention_days` ([sessions](installation.md#sessions)).

## The header edits

| Edit | Request kinds | Account kinds | Switch |
| --- | --- | --- | --- |
| The client's `authorization` or `x-api-key` slot carries the account's credential: `Authorization: Bearer {token}` for an OAuth account, `x-api-key: {secret}` for an API-key account. A request with no slot gets it right after `anthropic-version`, where the pinned client puts its key | All | Both | None |
| Never copied: `cookie`, `expect`, `forwarded`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `proxy-connection`, `te`, `trailer`, `transfer-encoding`, `upgrade`, `via`, `x-forwarded-for`, `x-forwarded-host`, `x-forwarded-port`, `x-forwarded-proto`, `x-forwarded-server`, `x-keepalive-owner-id`, `x-real-ip` | All | Both | None |
| `connection` becomes `keep-alive` | All | Both | None |
| `content-encoding` names the router's own coding, or is dropped when the body leaves as identity, and is appended when the client sent none | All | Both | `proxy.upstream_gzip` |
| `content-length` is the length of the body the router sends | All | Both | None |
| `host` is `api.anthropic.com` | All | Both | None |
| `x-client-request-id`, a v4 uuid, when the client sent none: after the client's `x-app` line, else right before the credential | Inference with a restoration context | Both | None, latched off for an account that refused it |
| Several `anthropic-beta` lines merge into one, each token once in the order sent, the betas below applied to the whole list once, and a line left empty is not sent | All | Both | None |
| Everything else travels as sent, in the client's order and spelling | All | Both | None |

The never-copied set has its reasons: the hop-by-hop headers describe the client's connection rather than the payload,
cookies and proxy credentials are the client's own secrets, the forwarding headers would disclose the client's network,
and `x-keepalive-owner-id` is the router's own ownership marker, which the keepalive mod writes and the router reads. A
client's `Expect: 100-continue` goes for the reason [Headers and transport](#headers-and-transport) gives. `connection`
is the router's own because its upstream connection is pooled and independent of the client's, which ends at the
router.

## The betas

| Token | Action | Request kinds | Setting |
| --- | --- | --- | --- |
| `oauth-2025-04-20` | Injected first in the list for an OAuth account when the client's line lacks it (the whole line when the line is empty), dropped for an API-key account, and never added when the client sent no line at all | All three | None, the account's kind |
| `extended-cache-ttl-2025-04-11` | Injected when the leg's TTL is one hour, dropped when it is five minutes, else passed through. Stripped and latched after a `400` naming it, after which the leg sends five minutes | Inference and probe (a count carries no TTL, so its token passes) | The pool's cache TTL policy |
| `cache-diagnosis-2026-04-07` | Injected with the `diagnostics` member, else passed through. A client-sent one is never reported as injected | Inference and probe | The pool's `cache_diagnostics` |
| `dangerous-tool-use-2026-09-03` | Dropped when auto mode is off for the key, else passed through | All three | `auto_mode_server` on the key, else the pool |
| `per-turn-control-2026-07-01` | Read for the cache lane key, passed through | All three | None |
| `auto-mode-classifier-2026-07-16` | Read with the `auxiliary` class as the marker of a Claude Code client's own classifier call, passed through | All three | None |
| Any other token | Passed through | All three | None |

The OAuth claim is a claim about the credential rather than about the client's feature set, so it leads the list and the
client's own entries keep their order behind it, and an API key that sends it is rejected upstream, so the router strips
it there. A beta the router injects is reported as its own, so an upstream `400` naming it strips it for one retry and
latches it off for the account, while a `400` naming a beta the client sent is the client's to handle and reaches it as
sent. The one-hour body and its beta stand or fall together: an upstream that refused the beta cannot serve the hour, so
a leg that stripped it, or an account latched against it, sends five minutes and reports that.

## The restoration

Under `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL` Claude Code sends the full first-party shape, the attribution line with
every field and `x-client-request-id` among them, and the router restores nothing ([The client](client.md)). Without it
the client sends the reduced shape, and the router restores toward `api.anthropic.com` the fields that line lacks: the
`cch` billing hash as the placeholder `cch=00000`, `cc_prev_req`, `cc_prompt_id` and the `x-client-request-id` header,
each only when absent and under the rules below, at the place the client's own field order gives it. A present field is
never rewritten. A Claude Code request whose `system` is an array and carries no line, as under
`CLAUDE_CODE_ATTRIBUTION_HEADER=0`, is refused `400` `client_contract` before the restoration reads it ([The required
set](client.md#the-required-set)), and the router still writes no line the client did not, so every restoration carries
the client's own stamp.

The restoration applies to an inference request whose `User-Agent` is `claude-cli/` followed by a version the client
itself would spell, digits and dots with at most one `-` and a prerelease suffix of letters, digits, dots and hyphens,
as in `claude-cli/2.1.269 (external, cli)`. The version lands in a line this build signs, so a version spelled any
other way restores nothing rather than a guessed one. A request without the class header is refused before the
restoration reads it ([The required set](client.md#the-required-set)), so the restoration reads the class from every
request and falls back on nothing. A token count has no line to fill,
and a probe replays a request as the client sent it.

The previous request id comes from the router's session memory, keyed on the client's own session id
([Clients](routing.md#clients)), and the memory survives a
restart ([sessions](installation.md#sessions)). It is filled on the conversation classes (`compaction`, `main`,
`subagent`, `workflow`) and on a class `auxiliary` request that carries an agent id or a prompt id, the
conversation-shaped forks, never on a side query, since the client's own builder writes the field on its conversation
sites alone. The prompt id comes from the `x-claude-code-prompt-id` header, which the client sets from the same id it
writes into the line, so a request without the header gets no `cc_prompt_id`, as the native client writes none. The auto
mode classifier requests are side queries, so the restoration writes no line field on them beyond the `cch` placeholder
where the line lacks it, and the router records the ones that carry the classifier marker as the client's classifier
calls, which take no share of a plan window ([Auto mode behind the router](#auto-mode-behind-the-router)). Three things
are not restored, because the router cannot: the `advanced-tool-use-2025-11-20` beta, since the client has to run its
tool search (`ENABLE_TOOL_SEARCH=true` selects it), the global cache scope on the static system block, and
`cc_turn_origin` with the five fields after it in the client's order (the turn position, the gateway and the project
fields), which the router never writes and whose turn origin it records when the line carries it.

## Client drift

Two checks guard the restoration against a client build the router was not pinned to. The hash check refuses a stamped
`cch=` hash this build's formula does not reproduce over the body, and the suffix check refuses, on a Claude Code
inference request, a `cc_version` suffix that does not reproduce from the first user message and the version the user
agent names. On a resumed conversation, whose first user message is the client's resume placeholder, the suffix also
reproduces from the empty prompt, since the client stamps the request over a message of tool results alone and writes
the placeholder afterwards. The hash goes first. A body carrying only the placeholder `cch=00000` passes, and a line
with no `cc_version`, a bare version with no suffix, or a version other than the agent's is not the suffix drift: the
first two carry nothing to check, and the third is the hash check's to judge. Both run once per request, after the key's
client and model checks and before any account is chosen, so the refusal names no account and no leg ever meets a drift.
The contract check runs before both, so a body without the line is a contract refusal and never a drift, and a request
that fails the contract and a drift check is refused for the contract ([The required set](client.md#the-required-set)).

Either refuses the request as `400` `invalid_request_error` with the code `client_drift`, the header
`x-phoebis-error-code: client_drift` and the message of its kind ([refusal codes](client.md#refusal-codes)):

| Drift kind | Message |
| --- | --- |
| `hash` | `the client's billing hash does not match the pinned profile. Re-pin the router to this client version` |
| `version_suffix` | `the client's version suffix does not reproduce from the prompt and the user agent. Re-pin the router to this client version` |
| `version_suffix`, resumed conversation | `the conversation was resumed, and the client's version suffix does not reproduce from the resume placeholder, the empty prompt or the user agent. Re-pin the router to this client version` |

The refusal logs `a caller stamped a billing line this build cannot reproduce. Re-pin the router to that client` at
warn level with the drift kind, the user agent and the profile name, and records itself as the request's one decision,
of kind `client` with the code `drift` and the check as its reason, so the request log names which check refused.

The warning also carries `resume`, `true` where the first user message is the client's resume placeholder, and the
suffix check's refusal of such a body names the resume in its message while it stays a real non-reproduction, since the
router reproduces a resumed conversation's stamp over the empty prompt.

The router fails closed because a stamp it cannot verify never travels under an account's credential, edited or not: a
wrong hash is indistinguishable from tampering, and the cost would fall on the account rather than on the request, which
is why neither relaying the body unedited under the client's stamp nor stripping the stamp is an answer. No setting
clears the refusal. The formula comes from the profile compiled into the build, `claude-code-2.1.269-linux-x64`, and
every reader takes it from there, so the cure is a build re-pinned to the client's version
([Re-pinning the client build](operations.md#re-pinning-the-client-build)). The user agent's version itself is recorded
for the ledger and never judged: every version is relayed the same way.

## The boundary chains

The router reads two cumulative boundary-hash chains off every request body: a static chain over the tools array and
the system blocks, and a message chain with one boundary per entry of `messages`. Both are computed on the client's own
bytes as canonical JSON with every `cache_control` member removed at every level, so a marker that moves changes no
boundary, and the two chains start from different seeds so they never collide: an MCP server joining mid-session changes
every static boundary and leaves every message boundary equal. Neither chain gates the request. Both ride with the
observation and the selection, so a warm pin's continuation and a keepalive's guard read the same hashes: the session's
pin compares its own hashes against a later request's to tell a continuation from a restart, and the keepalive engine
reads them as its guard ([The guard field](keepalive.md#the-guard-field)).

## Auto mode behind the router

Claude Code in auto mode reviews each tool use before it runs. Behind the router, with or without
`_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL` and in `-p` mode alike, the client asks the upstream to do that review on the
model's own turn: every session post carries a `safeguards` member near the end of its body (the machine's permission
rules with their literal text, the working and home directories, the trusted directories, the git state and the user
identity) and the `dangerous-tool-use-2026-09-03` beta, and the answer's `message_delta` carries one verdict per tool
use.

What the router does with that request is the pool's **Support server-side auto mode** setting, on by default, which a
key can override. Both sit in the console's expert view, the pool's in its dialog's Auto mode group and the key's in its
dialog's Access group ([Pools](routing.md#pools)).

- On, the router passes the member, the beta and the verdict through unchanged, and the request log and the drawer
  record the request as a server-side check, `server`.
- Off, the router drops the member and the beta from every request of the key and restamps a first-party body's billing
  hash. The upstream then evaluates nothing, and one completed answer without a verdict latches the client for the rest
  of its process, subagents included, across `/clear` and `/compact`: from then on it sends one classifier request of
  its own per reviewed tool use, on its Sonnet default rather than the session's model, which, where it carries the
  classifier marker (the `auxiliary` class with the beta `auto-mode-classifier-2026-07-16` the client sends under the
  first-party assumption with experimental betas allowed), the router records with the standing `client` under
  Classifier and prices at no share of a plan window on a subscription account, as the client's documentation says a
  Pro, Max or Team plan is never charged for them, and at its model's price in the account's overage and on an API-key
  account ([Requests](console.md#requests), [Statistics](statistics.md#plan-meter)), while the
  `dangerous-tool-use-2026-09-03` beta stays in its header and the router keeps dropping it. The first session post
  under the off state still asks, so a session that starts there costs one asking round. The request log records each
  request whose member the router dropped as `stripped`, and the client's marked classifier requests under Classifier
  `client` ([The console](console.md)).

`CLAUDE_CODE_AUTO_MODE_SERVER=0` in the client's environment is the client-side equivalent of the off state, with no
first asking round, and `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1` does the same in first-party mode.

The client's own classifier runs in two stages: a stage-1 request that stops at `</block>` and answers `<block>no` or
`<block>yes`, and, after a `yes` or an unreadable answer, a stage-2 request with no stop sequence that reasons in
`<thinking>` before its `<block>` answer, with an optional `<reason>` and `<category>`. A stage-1 block followed by a
refused stage 2 stands as a block. On Sonnet 5 the first stage answers `<severity>N` under the stop sequence
`</severity>`, where Sonnet 5.5 answers the older `<block>` shape, so a first-stage call ends after a few output tokens
either way. Behind a plain base URL without the hint gate the classifier requests carry neither the beta nor the class
header, and the router refuses them for the missing header. Under the first-party assumption with experimental betas
allowed (`CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` unset) they carry `auto-mode-classifier-2026-07-16` and the
`auxiliary` request class on both stages, the pair the router reads as the marker. A call that carries the class without
the beta, as under the hint gate alone or under `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`, is an ordinary auxiliary
request to the router and takes its model's share. The classifier bodies carry no `stream` and no `tools`, a
security-monitor system block, and `stop_sequences` at stage 1. They carry the session's own id and pin like any
request, on the Sonnet default, so a session on another model takes a second pin beside its own. The client also sends
them under the on setting for a tool input a PreToolUse hook rewrote, since the server's verdict is bound to the input
the model wrote.

**The notice.** The client can show a one-line notice that its classifier requests are billed because the gateway does
not support the server-side check. A Pro, Max or Team login never sees it. A key or token login sees it once per process
after the latch, naming the router's `host:port`, and acknowledges it. The text, from the client's own strings: "We're
changing auto mode to no longer charge for classifier requests in Claude Code. However, this session isn't eligible
because your requests go through `<host>`, which isn't compatible with this update. Nothing breaks: auto mode keeps
working, and its classifier requests are billed as before. To fix it and access the new version of auto mode, ask your
gateway to implement: `<url>`".

The router never adds the member or the beta to a request that did not carry them, and never answers a verdict itself. A
body it cannot cut under an off setting, because the member is spelled twice or in a way the raw walk never finds, or
whose billing stamp it cannot restamp after the cut, is refused rather than sent on
([What the proxy accepts](client.md#what-the-proxy-accepts)).

## The per-turn effort form

On a model the client marks for it, Claude Code sends `per-turn-control-2026-07-01` on `anthropic-beta` and carries the
effort of each turn in a `role: "system"` entry appended to `messages`, beside the top-level `output_config.effort`. The
router relays both as sent and never rewrites one form into the other. It reads the newest such entry's level as the
request's effort, since the upstream applies the newest control entry over the top-level member, and judges the user's
turn on the last entry the user or the assistant wrote, so the control entry after the user's turn is the client's and
not a turn of its own. To the message chain the entry is one more entry of `messages`. Under the beta the lane's effort
part is `*`, since the upstream keeps the cache across a per-turn change, so an effort change under this form keeps the
session's cache lane. On a model the client sends no such entry for, the effort is part of the cache lane and a change
of it reads the session's prefix as cold ([Selection](routing.md#selection)).

## How a head is classified

Every upstream response head is classified before a byte reaches the client, by account kind. A streamed `200` is held
until its `message_start` or its first `error` event, at most 64 KiB and 30 seconds, so a rate-limit error delivered as
a `200` stream can still fail over without a byte having reached the client. A stream that has not reached its first
committing event by then is forwarded as it stands, because the client is waiting and a longer hold buys no better
verdict. Past the commit, the router's own reading of a stream, the copy the ledger and the other bus readers take their
phases and usage from, holds at most 512 KiB of one event while that event is still arriving. An event that outgrows
the bound before its end is skipped in that copy and the events after it are still read, while one that arrives whole
in one decoded piece is parsed whatever its size. The relay logs one warning at the end of a stream that had an event
skipped, and the client receives every byte as sent either way. A stream the upstream cuts, stalls or sends undecodable
after the head, and one the account's cancel ends, for an account disconnected, unpaired, deleted or re-created under
its id, logs one warning with the account, the request id, the upstream request id, the bytes the client received, the
cut's code and the error's words, and lands in the ledger as a `stream_interrupted` event the request's drawer shows.
Every cooldown deadline carries a second of margin, so a request sent at the reset is not refused by a clock a hair
behind.

| Head | Verdict | What follows |
| --- | --- | --- |
| `400` `invalid_request_error` naming a field or a beta this leg injected, on a leg that has not stripped yet | `StripInjected` | One more try on the same account without the named items, which are latched off for the account |
| `404` `not_found_error` naming the model sent | `FailOver` | Another account, the model cooled on this one for `routing.model_rejection_seconds` under the window `model_rejection` |
| A fast request rate limited, with the credits refused or no plan window rejected | `FailOver` | The fast lane cools apart from the account: refused credits until the overage reset the head names, else an hour (a spend cap until the month turns), and a rate limit until the fast bucket's reset, else `Retry-After`, else ten minutes |
| OAuth `429`, or a `200` whose first event is `rate_limit_error` | `FailOver` | Another account, this one or the model cooled until the rejected window's reset, else `Retry-After`, else `routing.cooldown_fallback_seconds` |
| API-key `429`, or a `200` whose first event is `rate_limit_error`, naming the spend limit | `FailOver` | Another account, this one cooled until 00:00 UTC on the first day of the next month, reason `spend_cap` |
| API-key `429`, or a `200` whose first event is `rate_limit_error` | `FailOver` | Another account, the model cooled from `Retry-After`, or the account for `routing.cooldown_fallback_seconds` without one |
| API-key `400` naming the credit balance | `FailOver` | Another account, this one marked `funding_required` |
| `401`, or `403` `authentication_error` | `Refresh` for OAuth, `FailOver` for an API key | OAuth: the token is refreshed and the request tried again on the same account. API key: another account, this one marked `reauth_required` |
| `403` `permission_error` | `FailOver` | Another account, the model cooled on this one for an hour, reason `permission` |
| Anything else (a `2xx`, any other `4xx` or `5xx`) | `Commit` | The upstream's answer reaches the client as sent |

The cooldown tables, the windows and the reasons are on [Cooldowns and failover](routing.md#cooldowns-and-failover).
The rate-limit families the router reads off a head are `anthropic-ratelimit-unified-*`, `anthropic-ratelimit-*` and
`anthropic-fast-*`. A connection that fails before the whole request was written is retried once on a fresh connection,
after about a second when no connection could be opened and at once when a reused connection died, and one that may
have reached the upstream is never sent again: an upstream that read the request and closed without answering is
indistinguishable from one that never saw it, and sending it again could bill it twice.

## The edits toward the client

The response head the client receives drops `anthropic-organization-id`, `connection`, `content-length`, `keep-alive`,
`proxy-authenticate`, `proxy-authorization`, `set-cookie`, `te`, `trailer`, `transfer-encoding` and `upgrade`: the
connection and framing headers describe the upstream hop, and the framing does not match the body the client is sent,
while the upstream's own identity and cookies name the account behind the key, never the key holder. It hides the
`anthropic-ratelimit-` and `anthropic-fast-` families on every answer that is not a `429` or a `200` whose first event
is a `rate_limit_error`, so an organization's standing never reads as the key holder's own, and a rate limit keeps them
so the client's continue arms on the reset the upstream named. The router decodes every coded body (gzip, deflate,
brotli and zstd), forwards it as identity and drops `content-encoding`, whatever the client's `Accept-Encoding`, and
answers any other coding with a `502` `upstream_undecodable` before any byte reaches the client. Everything else in the
head reaches the client as the upstream wrote it. A refusal the router answers itself is in Anthropic's error shape
with the header `x-phoebis-error-code` naming the code ([refusal codes](client.md#refusal-codes)).

## Headers and transport

Request headers travel as the client sent them, in the client's own order and spelling: the router records the head off
the wire before its HTTP parser reads it, since the parser keeps no spelling a server can read back. The router drops
the hop-by-hop and forwarding headers, puts the account's credential where the client put the key, and edits
`anthropic-beta` as [The betas](#the-betas) lists. It owns `Host` and `Content-Length`, and adds nothing for a header
the client left out beyond what [The header edits](#the-header-edits) lists: the credential slot a request without one
gets, a `content-encoding` line when the body leaves gzip and the client sent none, the restored `x-client-request-id`
and an `anthropic-beta` line for a beta it injects, so any Claude Code version relays on its own shape rather than a
guessed one. A client's `Expect: 100-continue` is dropped: the router writes the body right behind the head, so the
upstream would otherwise wait for a continue the router never sends.

The proxy listener speaks HTTP/1.1 and no HTTP/2. An HTTP/2 client's head would reach the router through its HTTP
library's own map, in that library's own spelling, which is the silently different outbound shape the pass-through
exists to prevent, so the connection is refused instead.

## What the router sends on its own

The polls, the fetches and the probes leave the router with no client behind them. A poll and a fetch have no switch,
and the credential refresh that a poll or a fetch draws ahead of the token's expiry has none either. A probe has the
pool's.

**The usage poll.** Every 300 seconds the router reads the usage endpoint of every OAuth account that holds a
credential, a paused account included, because a paused account's quota is still the operator's to watch. The call
carries `anthropic-beta: oauth-2025-04-20`. The interval is often enough that a routing decision lags use of the account
outside the router by about that long, longer while the endpoint rate-limits the poll, and rare enough that the poll
alone never reaches the endpoint's per-account limit. No key or field turns the poll off.

**The profile.** On every twelfth poll of an account, an hour apart, the router reads the account's profile again: a
tier moves rarely, and the profile endpoint shares the usage endpoint's per-account limit. It rides on the poll and has
no switch of its own.

**The catalog fetch.** The router reads each enabled account's model listing with `GET /v1/models?limit=1000` as the
pinned client sends it, every `routing.catalog_refresh_seconds` (default 21600, 60 to 604800, the bootstrap file refused
at start with `routing.catalog_refresh_seconds must be between 60 and 604800, not {value}`), on demand when the account
has none, and holds a failed read for sixty seconds before the account is read again
([routing](installation.md#routing)). No switch: the key sets the interval alone.

**The startup fetch.** Before the proxy listener binds, the router fetches every enabled account's catalog and samples
every polled account's usage, each bounded by `routing.startup_fetch_seconds` (default 30, 1 to 60, the bootstrap file
refused at start with `routing.startup_fetch_seconds must be between 1 and 60, not {value}`), so the first request finds
the dated ids and the samples in place. What is still running at the bound finishes in the background, and the
scheduled poll's first slots lie one interval from there. The key bounds the wait and nothing turns the fetch off.

**The keepalive probes.** Under the pool's `keepalive.enabled` the router replays a kept request through the same
handler as the client's own, as a probe, to keep its prompt cache warm ([Turning it on](keepalive.md#turning-it-on)). A
probe costs a cache read ([How a probe works](keepalive.md#how-a-probe-works)) and is the one request of this list with
a switch.

## The keepalive protocol

The keepalive mod reports to one route, `POST /v0/keepalive/events` on the proxy listener, with
`Authorization: Bearer <router key>`, `Content-Type: application/json` and a JSON body of at most 16 KiB. The key is
resolved exactly as `/v1/messages` resolves it, with the generation it was created under, so a key id created again
reaches none of the previous holder's sessions. Every body carries five fields, `protocol` (the integer `1`), `event`,
`session_id`, `instance_id` and `sequence`, then the fields of its event, and a field an event does not take is refused
rather than ignored, so a report the mod got wrong never reads as one it got right. `sequence` rises with every report
of a session: the mod sends the clock in epoch milliseconds, or one more than the previous report's when that is
higher, so a session the mod registers again after a reload or a return to an earlier id never falls behind the
sequence the router took last. An agent's reports carry `request_generation` once the router has named a generation
for that agent. Main's agent id is `main`.

| Event | Fields | What the router does |
| --- | --- | --- |
| `session_start` | `owner_id`, and `previous_instance_id` when it takes over from another instance | Registers the session under the key, or takes it back after a lapse or an end, and answers with the agents it holds |
| `heartbeat` | none | Renews the lease, the one event besides a `session_start` that does |
| `agent_start` | `agent_id`, and `request_generation` when known | Starts the agent `busy`, or restarts a released one under the generation the start names |
| `activity` | `agent_id`, `kind` (`prompt` or `presence`), and `request_generation` when known | A prompt sets the agent `busy` and resets its idle clock. A presence moves the idle clock alone |
| `state` | `agent_id`, `state`, `request_generation` when known, and the park fields of a `parked` state | Moves the agent to the state |
| `session_end` | none | Releases every agent of the session and ends it |

The router accepts five states. `busy` is an agent at work. `idle` is an agent between requests: a prompt that sent no
request, such as a local command, lets the idle window go on, and a park that ended restarts it. `parked` is an agent
waiting on a background command or a scheduled wakeup, and carries `park_kind` (`background`, `scheduled` or `both`),
`wake_at`, the earliest scheduled wakeup, required unless the kind is `background`, and `deadline`, the latest deadline
across the background commands the park waits on. `released` ends the agent's run and discards its recipe, and a
released main ends its session's idle window. `unknown` discards the recipe too, under its own reason. The mod sends the
first four. The lease a `session_start` or a `heartbeat` renews runs 90 seconds, and the pool's idle limits take over
when it lapses.

A `200` carries an acknowledgment with `session_id`, `instance_id`, `sequence`, `lease_expires_ms` and
`request_generation`: for an agent event the named agent's generation beside its `agent_id`, and for a session-level
event, a `session_start`, a `heartbeat` or a `session_end`, main's generation, which the mod adopts for main. The
acknowledgment of a `session_start` adds `agents`, a list of `{agent_id, request_generation, state}` for every agent the
router holds for the session, main included. The mod adopts every generation the answer names and reconciles that list
with its own agents. An agent only the router holds is adopted when the client's own agent list reads it live, and
released otherwise, and without that list the router's entry stays as it is. An agent the mod released in this session,
which the client has not spawned or given a turn since, is not adopted again whatever the list reads, since that release
ends the run the router holds, and the mod sends the release once more when none is still queued, so a router restored
from a save older than the release learns of it. A state the router holds wrongly is sent again. An agent only the mod
holds is announced with `agent_start` and its state. An entry the router holds as `released` is skipped both ways, so a
subagent the mod holds as parked stays released on the router until its next report starts it again with `agent_start`.
An agent the answer names while the mod's release of it still waits in the queue stays unannounced, so the agent's next
run starts it again with `agent_start`.

A refusal takes one of two shapes. A missing, malformed, ambiguous or refused key is answered the relay's way, the same
`401` in Anthropic's error shape that `/v1/messages` answers, with `x-phoebis-error-code` naming an `auth_*` code
([refusal codes](client.md#refusal-codes)), so a mod pointed at the wrong port sees the answer the client would, and
never a `403`. Every other refusal is the route's own JSON, `{"code": "<code>"}`, without the `x-phoebis-error-code`
header, under `Cache-Control: no-store`, which every answer of the route carries. An `instance_conflict` adds
`instance_id` when the instance that holds the session has ended or gone silent, so the mod can take it over, and
`stale_request_generation` and `agent_released` add the agent's current `request_generation`, so the mod adopts it
without a registration. The eighteen codes, by status:

| Code | Status | When |
| --- | --- | --- |
| `invalid_event` | 400 | The body is not typed `application/json`, is not this protocol's JSON, names no `protocol`, carries a field its event does not take or lacks one it requires, carries a park field on a state other than `parked`, or reports `parked` without `park_kind` or, outside `background`, without `wake_at` |
| `invalid_identity` | 400 | `session_id`, `instance_id`, `agent_id`, `owner_id` or `previous_instance_id` is not a valid id, or `sequence` is 0 |
| `invalid_state` | 400 | `kind` is neither `prompt` nor `presence`, `state` is none of the five, or `park_kind` is none of the three |
| `unknown_event` | 400 | `event` is none of the six |
| `unsupported_protocol` | 400 | `protocol` is not `1` |
| `agent_exists` | 409 | An `agent_start` for an agent the router holds that is not released under the generation the start names |
| `agent_released` | 409 | An `activity` or a `state` for an agent the router holds as released |
| `instance_conflict` | 409 | A `session_start` for a session another instance holds, without the holder's id as `previous_instance_id` |
| `instance_ended` | 409 | A report for a session that ended or whose lease lapsed, which only a `session_start` takes back |
| `stale_request_generation` | 409 | An `activity` or a `state` whose `request_generation` is not the agent's current one |
| `stale_sequence` | 409 | A `sequence` at or below the session's last |
| `unknown_agent` | 409 | An `activity` or a `state` for an agent the session does not hold |
| `unknown_instance` | 409 | A report for a session the router does not hold under this key and its generation, or from an instance other than the holder |
| `unknown_previous_instance` | 409 | A `session_start` naming a `previous_instance_id` for a session the router does not hold |
| `event_too_large` | 413 | The body exceeds 16 KiB |
| `agent_limit` | 429 | The engine's agent cap is reached, on an `agent_start` or on a `session_start` that would add main |
| `session_limit` | 429 | The engine's live session cap is reached on a `session_start` that would bring a session to life |
| `quiesced` | 503 | The engine is closing, so a late report is counted and not applied |

The mod sends one request at a time. Reports queue in order, the next goes out when the answer to the one in flight
lands, and no hook waits for a request, so a stalled router never holds up a tool call or a prompt. A report that
repeats what the queue already holds collapses into it: the queue keeps one activity per agent (a prompt over a
presence), one heartbeat and the newest state per agent. The answer to a `session_start` or a `heartbeat` arms the next
heartbeat at a third of the remaining lease, between 1 and 25 seconds. No other answer moves it, and no heartbeat goes
out before a registration has been acknowledged. What each answer makes the mod do:

- A `2xx`: the report is done and the backoff starts over.
- A transport failure, a `5xx`, a `session_limit`, or an `agent_limit` on a `session_start`: the report goes back to the
  head of the queue and the whole queue is retried after 1, 2, 4, 8 and 16 seconds, then every 30 seconds.
- An `agent_limit` on an `agent_start`: the start goes back and that agent's reports wait behind it, retried along the
  same backoff under the agent's own retry time, while the heartbeats and the other agents' reports go on.
- A `401` or a `403`: the mod empties its queue and reports nothing more until the module loads again.
- `agent_exists`: the mod drops the report, counts the agent as announced and registers again.
- `agent_released` with a generation: the mod drops the report and adopts the generation, and the agent's next report
  starts it again with `agent_start`. Without a generation the mod drops the report and registers again.
- `instance_conflict` naming the holding instance: the mod retries the registration with `previous_instance_id`, and
  again with each newly named holder. One that names no instance keeps the session: the start goes back to the head of
  the queue and is retried along the same backoff until the router names the holder.
- `instance_ended`, `stale_sequence` and `unknown_instance`: the report goes back behind one `session_start` with the
  same instance. A registration refused again with the same code waits along the same backoff before it goes out
  again, rather than going out at once round after round.
- `stale_request_generation` with a generation: the mod adopts it and sends the report once more, and drops it on a
  second refusal. Without a generation the mod drops the report and registers again.
- `unknown_agent`: the report goes back with an `agent_start` right before it.
- `unknown_previous_instance`: the mod registers again without `previous_instance_id`.
- Any other answer, a `400` or a `413`: the mod drops the report, since the router never takes it.

At the end of a session the mod sends one `session_end` at once, ahead of the queue and past a request in flight, and
drops what is still queued. Every `session.end` hook shares one short budget, so the end is a single request that is
never retried. The router's lease covers whatever it misses, a crash or a killed process included.

## The relay's waits

| Wait | Bound | Why |
| --- | --- | --- |
| The upstream connect and TLS handshake | 10 seconds | A first handshake on a cold host runs well under it, and a stalled one costs at most twice it plus the one-second wait between the two tries before the `502` |
| The upstream response head | 600 seconds | The relay's own ceiling on a silent upstream, above the client's wait with the client's default timeouts: Claude Code gives the head 180 seconds plus 1 second per 32 KiB of body under its own 600-second cap, so the client leaves no later than this guard |
| The gap between two stream events | 600 seconds | The same ceiling: Claude Code aborts a stream after 180 seconds without a byte while its watchdog is on, so the client leaves no later than this guard |
| The pre-commit hold | 30 seconds and 64 KiB | The client is waiting, and a longer hold buys no better verdict |
| A retry in place | The pool's `retry_in_place_seconds`, 0 to 300, default 60 | The operator's bound on waiting out a throttle on the same account |
| The slow-down's hold | The pool's `slow_down_hold_seconds`, 1 to 300, default 120 | Before the head, so the client's first-byte window covers it |
| The wait for a moved pin's first head | The pool's `slow_down_hold_seconds` from the first request's dispatch, never past the request's deadline | Before the head, so the client's first-byte window covers it |

A connect or handshake that times out counts as unsent, so the request is written once more on a fresh connection after
a wait of about a second. A retry in place sleeps the throttle's delay on the same account, with the same credential,
before any byte of the answer's head reaches the client ([Retry in place](routing.md#retry-in-place)). The slow-down's
hold sleeps to the pin's deadline, which a retry or a sibling request of the pin rejoins, before the request is selected
again ([Slow-down](routing.md#slow-down)). A reset wait and the slow-down and rewrite holds end inside the head guard
whatever its length: at the request's deadline where the guard leaves one, else at the guard itself.

Beneath these sit Claude Code's own waits. The client cuts a stream that has gone silent for 180 seconds with its
watchdog on, and gives the first byte of an answer a window of at least 180 seconds ([The client](client.md)). The four
relay waits that meet them differ. The head and idle guards sit above the client's window, so a dead upstream stream is
the client's cut at 180 seconds, and the router's 600 seconds stand behind the client's window while the watchdog is on.
A retry in place holds the head back for the throttle's delay, up to the pool's bound, so a bound above the client's
first-byte window makes the client end the request and send it again. The slow-down's hold holds the head back too,
served before any slot or reset wait and under its own bound, the pin's cycle times the slow-down up to
`slow_down_hold_seconds`, whose default of 120 seconds leaves 60 of the window for the upstream's head and the
pre-commit hold. The wait for a moved pin's first head holds the head back too, under the same bound counted from the
first request's dispatch and never past the request's own deadline, with the pool's `slow_down` on or off. The
operator's choice is to keep each bound at or under the window, or to switch the watchdog off for a client whose
requests may wait that long ([The client](client.md)).
