# Pairing an account

You are adding an account to the router, in the console or by `curl`. The router holds two kinds. An OAuth account is
paired: the router hands you a login link, you sign in, and you paste back the address the browser lands on. An API-key
account is keyed at creation and verified against the upstream before the row is kept. This page walks the pairing
dialog in the console, the same flow with `curl`, the five states an account reads in and the actions the Accounts view
offers on it.

## Two kinds of account

An OAuth account is a Claude.ai or Console login, paired through the dialog or the API below. The router keeps the
refresh token the pairing hands it and renews the access token on its own. An API-key account holds an Anthropic API
key toward `api.anthropic.com` and is created with its key in one call, `POST /admin/accounts` with `"kind": "api_key"`
and the key as `secret`. The router sends a models request with the staged key before the row exists, so a rejected key
answers `400 credential rejected by the upstream`, and a models endpoint that answers a status other than `200` or
`401`, is unreachable or answers a malformed body answers `502 the key check failed: {reason}` with the reason after the
colon. Neither creates the account. A new key for an existing account goes through `PATCH /admin/accounts/{id}` as
`secret` and is verified the same way, and a check that does not succeed refuses the edit whole.

A usage-based Enterprise seat is an OAuth account the router places as a paid source beside the API-key accounts. The
router classes it from the stored profile: the rate-limit tier `default_claude_zero`, or the seat tier
`enterprise_usage_based`. Such a seat has a spend window against its monthly cap in place of the plan windows and is
billed in dollars at the API rates. A Team or seat-based Enterprise seat keeps its plan windows. The account's plan is
read from the profile's organization type first and its rate-limit tier second, so a free organization reads `free` and
prices no capacity, and `GET /admin/accounts/{id}` answers the word as `subscription.plan`.

An id matches `[a-z0-9][a-z0-9-]{0,63}`, a label is at most 256 bytes, and an id in use answers
`409 an account with this id exists`. The create route takes `"kind": "oauth"` too, with no secret, for a script that
makes the account ahead of its pairing: the account reads `unpaired` until a pairing confirms, and an `oauth` create
that carries a secret answers `400 an oauth account is paired, not keyed`.

## Through the console

Open Accounts ([The console](console.md#accounts)) and choose Add account. Pick "OAuth login" or "API key" and give the
account a label. The id the router will use is derived from the label and shown under it, and "customize" opens it for
editing. Choose Start pairing, or paste the key under API key and choose Add API key. An API-key account is in the list
once the router has verified its key. An OAuth account appears once its pairing is confirmed, and a pairing you cancel
leaves no account.

The pairing dialog runs two steps.

### Log in and paste the address

Open the authorization link, sign in to Claude.ai and approve the router. The browser lands on
`http://localhost:{port}/callback?code=...&state=...`, the loopback redirect Claude Code registers with the provider,
on a port from 49152 to 65535. Nothing listens there, so the page fails to load, which keeps the browser off the
provider's own code page, and the address bar still holds the whole URL. Paste the URL, or the code alone, and choose
Submit. The console sends a value starting with `http://` or `https://` as the URL and anything else as the code with
the session's state.

### Confirm

The dialog shows the email and organization the upstream answered. Check that they are the ones you meant to pair, then
choose Confirm to store the refresh token for this account. The Accounts view shows the account's first reading once
the poll lands, a few seconds after the confirm, and its Usage card reads "The first quota reading is in flight." until
then.

Both of the console's starts, for a new account and for an existing one, are `claude_ai` logins, which authorize at
`claude.com`. A `console` login, which authorizes at `platform.claude.com`, is available through the API. A pairing
session expires ten minutes after it starts, the window Claude Code gives a pasted callback before treating it as stale,
and the dialog counts the time down. A new start from the same console session cancels the pairing left standing, for an
existing account and for a new one alike, and the old link stops working. A start while another session's pairing
stands, or while one is being confirmed, answers `409 a pairing session is already in progress for this account`, and
the standing pairing has to finish or expire first.

## With curl

The `curl` lines below are the same flow by hand, with the admin secret as a bearer. `Authorization: Bearer` with the
secret is admitted without a cookie and without the `Origin` check, and every bearer call shares one session, so a
pairing one bearer call starts the next confirms. A mutation without a bearer that carries an `Origin` must name the
request's own `Host` with it, `https://` where the admin listener terminates TLS or a front says the browser's leg does
([admin](installation.md#admin)). A request with a body carries `Content-Type: application/json`, or the router answers
`415 content type must be application/json`. The lines name the loopback admin listener of the Quick start,
`http://127.0.0.1:8081`, and a listener with a certificate pair takes `https://`. Every operation's request, answer and
refusals are in the document [API reference](api-reference.md) points at.

Start a pairing session for a new account. `login` is `claude_ai` by default or `console`:

```sh
curl -sS -X POST http://127.0.0.1:8081/admin/oauth/sessions \
  -H "authorization: Bearer $PHOEBIS_ADMIN_SECRET" -H 'content-type: application/json' \
  -d '{"new":{"id":"primary","label":"Primary"},"login":"claude_ai"}'
```

The answer is `201` with the session's `id`, the `authorization_url` to open, the `redirect_uri` the session registered
and `expires_at`. No row is written until the confirm, so a session started with `new` that is canceled, expires or
fails leaves no account, and an id in use answers `409 an account with this id exists`. To pair an existing OAuth
account, its first pairing or a re-pair, name it with `account_id` instead of `new`:

```sh
curl -sS -X POST http://127.0.0.1:8081/admin/oauth/sessions \
  -H "authorization: Bearer $PHOEBIS_ADMIN_SECRET" -H 'content-type: application/json' \
  -d '{"account_id":"primary","login":"claude_ai"}'
```

Open the `authorization_url` in a browser and sign in. The browser lands on the loopback redirect,
`http://localhost:{port}/callback?code=...&state=...`, which fails to load. Paste it back as `url`, or send the code
alone as `code` with the `state` parameter of the `authorization_url` as `state`:

```sh
curl -sS -X POST http://127.0.0.1:8081/admin/oauth/sessions/$ID/callback \
  -H "authorization: Bearer $PHOEBIS_ADMIN_SECRET" -H 'content-type: application/json' \
  -d '{"url":"http://localhost:{port}/callback?code=...&state=..."}'
```

The provider appends the state after a `#` in the code it issues, and that suffix may stay. The router parses the
pasted URL and does not fetch it: a URL whose scheme, host, port or path differs from the session's redirect answers
`400 callback does not match the registered redirect`, and a state that differs from the session's
`400 state does not match the pairing session`. The router exchanges the code at the token endpoint and answers `200`
with `status` `exchanged` and the `pending` identity: `account_uuid`, `email`, `organization_name` and
`organization_uuid`. A rejected exchange answers `502` with the upstream's detail and ends the session, since the code
is spent.

Check that the email and organization are the ones you meant to pair, then confirm:

```sh
curl -sS -X POST http://127.0.0.1:8081/admin/oauth/sessions/$ID/confirm \
  -H "authorization: Bearer $PHOEBIS_ADMIN_SECRET"
```

The answer is `200` with the `account_id`, `status` `confirmed` and `first_poll_in_flight` `true`. The credential is
committed and the account's state set to `ready`, and a session started with `new` creates the row `ready` with its
identity, credential and profile in one write. The confirm reads the usage endpoint right after answering, so the
windows show within seconds. An account stays bound to the upstream account it was first paired to, so moving it
elsewhere means deleting it: confirming another upstream account answers
`409 this slot is already paired to a different upstream account`, and an upstream account already paired to another of
the router's accounts `409 that upstream account is already paired to another slot`. A confirm on an account whose
stored credential does not open under the router's storage key, because another key sealed it or it is damaged, answers
`409 {label} holds a credential sealed under another storage key, so disconnect it before pairing again` with the
account's label. The console's pairing dialog offers Disconnect for it, which deletes the sealed credential so a fresh
pairing proceeds.

`GET /admin/accounts` lists the accounts with their `kind`, `state` and the rest of each row, and
`GET /admin/accounts/{id}/usage` answers the poll's latest snapshot of the account's usage windows and the poll's
standing, without a read of the endpoint:

```sh
curl -sS -H "authorization: Bearer $PHOEBIS_ADMIN_SECRET" http://127.0.0.1:8081/admin/accounts
curl -sS -H "authorization: Bearer $PHOEBIS_ADMIN_SECRET" http://127.0.0.1:8081/admin/accounts/primary/usage
```

## Account states

`GET /admin/accounts` reads `state` as one of five values, which [The console](console.md#accounts) labels.

| State | What puts an account there | What moves it on |
| --- | --- | --- |
| `disconnected` | Disconnect, on an OAuth account | A pairing confirmed on the same account, back to `ready` |
| `funding_required` | The upstream refused an API-key account's request over its credit balance | A retry that succeeds after five minutes (or the [model rejection interval](routing.md#cooldowns-and-failover) when that is shorter), or Clear funding, back to `ready` |
| `ready` | An API-key account created or re-keyed with a key the upstream accepted, a pairing confirmed, a funding retry that succeeds, or Clear funding | A funding refusal, a definitive credential failure, or Disconnect |
| `reauth_required` | A definitive credential failure, listed below | A pairing confirmed, or a new key stored through `PATCH /admin/accounts/{id}`, back to `ready`, or Disconnect |
| `unpaired` | An OAuth account created ahead of its pairing, or Disconnect on an API-key account | A pairing confirmed, or a key stored through `PATCH /admin/accounts/{id}`, back to `ready` |

A pairing session on an existing account leaves its state unchanged, `unpaired`, `disconnected` or `reauth_required`,
until the confirm writes `ready`. A session started with `new` has no row until the confirm writes one, and a cancel, an
expiry or a failure before that leaves nothing. Renewing an access token leaves the state at `ready` for the whole
attempt: a refresh the token endpoint refuses with any status but `401` or `400` with `invalid_grant`, or one that fails
in transport, arms a backoff and tries again, and only a definitive failure, listed below, moves the state.

`reauth_required` follows one definitive failure, never a retry count. An API-key account is marked on a `401`, or a
`403` whose error type is `authentication_error`, from the upstream. An OAuth account is marked when a refresh finds no
refresh token in the stored bundle, when the token endpoint answers `401` or `400` with `invalid_grant`, when the usage
poll's read of the usage endpoint answers `401` on the account's current credential, when the refreshed token names an
identity other than the account's, when a request replayed with a freshly refreshed token is refused again, or when a
refresh was in flight as the process stopped, which the next start cannot settle and so quarantines. The usage poll
stops reading a marked account until a pairing is confirmed on it. Any other failed answer of the usage or the profile
endpoint, or a read that fails in transport, leaves the state as it is: the poll logs one warning and writes one
`poll_failed` event per poll that failed, while a `429` backs the poll off, and a usage `403` whose error code is
`oauth_not_allowed_for_organization` writes `plan_ended` in its place where the profile it makes due reports no plan, as
the next paragraph describes.

An OAuth account whose subscription ended keeps its credential and its state, and its profile reports the organization
holding no plan. A profile read that finds no plan, the hourly one or the start's, flags the account plan ended, and a
usage `403` whose error code is `oauth_not_allowed_for_organization` makes that read due at once. While the flag stands
the state stays as it was, the usage poll sends the account no usage request and reads its profile hourly, the stored
tier stays the last plan's, the router places nothing on the account, reading it as past its end, the account view
answers no quota window, since the last reading would draw as live, and Refresh reads the profile before any usage
request. A profile read that finds a plan again, or a pairing confirmed on the account, clears the flag, and the usage
poll reads the account from its next slot. A disconnect leaves the flag standing until the pairing after it.
`GET /admin/accounts` answers the flag as `plan_ended_at`, and the routing log records `plan_ended` and `plan_restored`.

The Accounts view offers these actions, each through a control that [The console](console.md#accounts) describes, and
each calls its own route:

| Action | Route | What it does |
| --- | --- | --- |
| Catalog | `GET /admin/accounts/{id}/catalog` | Answers the account's model list as last read, and `?refresh=1` reads the upstream again first. A failed read is held for sixty seconds. |
| Clear cooldown | `DELETE /admin/accounts/{id}/cooldowns/{scope}` | Clears one scope of the cooling-down block: `account`, `fast` (the fast lane's rate limit), `fast_credits` (the fast lane's usage-credits refusal) or `model:{canonical}`. A scope with no row answers `404 no such cooldown`. |
| Clear funding | `POST /admin/accounts/{id}/clear-funding` | Moves a `funding_required` account back to `ready` at once, ahead of the router's own retry after five minutes, or the [model rejection interval](routing.md#cooldowns-and-failover) when that is shorter. Any other state answers `409 the account is not waiting on funding`. The console's button reads "Clear funding flag". |
| Delete account | `DELETE /admin/accounts/{id}` | Removes the account from every pool and deletes it. Request history stays in the ledger. |
| Disconnect | `DELETE /admin/accounts/{id}/credentials` | Deletes the credential and lands the account on `disconnected`, or on `unpaired` for an API-key account. Pool memberships and request history stay, and a pairing in flight for the account is canceled. |
| Pair or Re-pair | `POST /admin/oauth/sessions` | Starts the pairing session described above, with `account_id` naming the account. |
| Pause and Resume | `POST /admin/accounts/{id}/pause`, `POST /admin/accounts/{id}/resume` | Flips `enabled`. Selection skips a paused account, and its state stays as it was. |
| Verify | `POST /admin/accounts/{id}/verify` | Sends a models request to the upstream with the stored key and answers `ok` with the upstream's `status`, `200` on success and `401` for a rejected key. It writes no state. An OAuth account answers `400 only an API-key account is verified this way`, and an API-key account without a key `409 account holds no API key`. |

The console warns before Clear cooldown runs that the account is eligible at once, and that an account still rate
limited re-enters cooldown on its next request ([Accounts](console.md#accounts)).
