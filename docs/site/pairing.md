# Pairing an account

An `api_key` account needs no pairing; its key is stored when the account is created. An `oauth` account is
paired: the router hands you an authorization URL, you sign in, and you paste back the address the browser lands
on. This page walks the pairing dialog in the console, the same flow with `curl`, and what an account looks like
afterwards.

## Two kinds of account

An `oauth` account is a Claude.ai or Console login, paired through the dialog or the API below. An `api_key`
account, shown as "API key" in the console, holds an Anthropic API key toward `api.anthropic.com`.
Creating an `api_key` account verifies the key against its host at once: a rejected key answers
`400 credential rejected by the upstream`, and a models endpoint that fails, is unreachable or answers a malformed body
answers `502 the key check failed: ...` with the reason after the colon. Neither creates the account.

## Through the console

Open Accounts and choose Add account. The dialog's subtitle names the choice: subscription logins share your Claude.ai
quota, while API keys bill per token against a Console workspace. Pick OAuth login and give the account a label. The id
the router will use is shown under it, derived from the label, and customize opens it for editing. Choose Start
pairing. The account appears in the list once the pairing is confirmed. A pairing you cancel leaves no account.

The pairing dialog runs two steps.

### Log in and paste the address

Open the authorization link, sign in to Claude.ai, and approve the router. The browser then lands on a callback
address that will not load, since nothing listens on that port. Paste the URL from the address bar, or the code alone.

### Confirm

Confirm the identity shown, the account's email and organization, to store the refresh token for this account.

The console starts every pairing as a `claude_ai` login, authorizing against `claude.com`; a `console` login,
authorizing against `platform.claude.com`, is available through the API. A pairing session expires ten minutes
after it starts, and the dialog shows the remaining time. Starting again from the same console session replaces the
pairing left standing, for an existing account and for a new one alike. A pairing another session started, or one being
confirmed, has to finish or expire first.

![The Accounts view: the list on the left, the selected account's actions, identity, usage, cooling-down block,
and credential on the right](assets/accounts-light.png#only-light)
![The Accounts view: the list on the left, the selected account's actions, identity, usage, cooling-down block,
and credential on the right](assets/accounts-dark.png#only-dark)

## With `curl`

The `curl` lines below are the same flow by hand.

Every admin mutation that carries an `Origin` header must name the request's own `Host` with it, with the `https://`
scheme where the admin listener terminates TLS or a front sends `X-Forwarded-Proto: https` ([access](api.md#access)),
and the router refuses the request otherwise; a request that carries no `Origin`, as the curl lines below do on
loopback, is admitted. Every request that carries a body also needs `Content-Type: application/json` (the curl lines
below already carry it). Log in first and keep the cookie:

```sh
curl -sS -c jar.txt -X POST https://phoebis.example/admin/session \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"secret":"'"$PHOEBIS_ADMIN_SECRET"'"}'
```

Start a pairing session for a new account: the row is written when the pairing is confirmed. The answer carries an
`id` and an `authorization_url`, and `login` selects `claude_ai` (the default) or `console`:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"new":{"id":"primary","label":"Primary"},"login":"claude_ai"}'
```

To pair an existing slot again, name it with `account_id`:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"account_id":"primary","login":"claude_ai"}'
```

Open the `authorization_url` in a browser and sign in. The provider sends the browser to the loopback redirect
the session registered, `http://localhost:<port>/callback?code=...&state=...`, which is the same redirect the
desktop client uses. Nothing listens on that port, so the page fails to load; the URL in the address bar is
still complete. Paste it back (the answer to the start request names the exact `redirect_uri`):

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions/$ID/callback \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"url":"http://localhost:<port>/callback?code=...&state=..."}'
```

The code alone works as well, with the session state in its own field; the provider appends the state after a
`#` in the code it issues, and that suffix may stay:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions/$ID/callback \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"code":"<code>#<state>","state":"<state>"}'
```

The router parses the pasted URL and never fetches it; a URL whose scheme, host, port, or path differs from the
session's redirect is refused, as is a state that differs from the session's.

The router exchanges the code and answers with the upstream account and organization it reached. Check that they
are the ones you meant to pair, then commit the credentials:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions/$ID/confirm \
  -H 'origin: https://phoebis.example'
```

`GET /admin/accounts` lists the slots and their states, and `GET /admin/accounts/{id}/usage` answers the poll's
latest snapshot of the account's usage windows and the poll's standing. The confirm reads the endpoint at once, so the
windows show within seconds of it.

A loopback deployment behind an SSH tunnel uses `http://127.0.0.1:8081` in place of `https://phoebis.example` in
every example above, and sends no `Origin`.

## Account states

| State | What puts an account there | What moves it on |
| --- | --- | --- |
| `disconnected` | Disconnect, on an `oauth` account | Pair or Re-pair |
| `funding_required` | The upstream named an `api_key` account's credit balance | Clear funding, back to `ready` |
| `ready` | Pairing confirms, an `api_key` verifies, or a hold clears | Reauth, funding, or Disconnect |
| `reauth_required` | A definitive credential failure; see note below | Re-pair, or a new `api_key` secret |
| `unpaired` | A create ahead of its pairing, or an `api_key` Disconnect | Pairing, or a stored `api_key` secret |

A pairing session on an existing row leaves its state unchanged, `unpaired` or `reauth_required` for a re-pair, for
the whole flow. The console tracks progress through the session's own status. A session started with `new` has no row
until the confirm writes it `ready`, and a cancel, an expiry or a failure before that leaves nothing. Renewing an
access token leaves the account's state at `ready` for the whole attempt, win or lose; only a definitive failure moves
it.

`reauth_required` follows a single definitive failure, never a retry count: a 401 on an `api_key` account, an
`oauth` refresh that finds no stored token, gets a 401 or `invalid_grant` from the token endpoint, or resolves to
a mismatched identity, or a request retried once with a freshly refreshed token that still comes back 401.

The Accounts view offers these actions, each calling its own route:

| Action | What it does |
| --- | --- |
| Catalog | Opens the account's live model list, as of the last fetch, with a refresh. |
| Clear cooldown | Clears one scope of the cooling-down block: the whole account, its fast lane, or one model. |
| Clear funding | Clears a `funding_required` account back to `ready`. |
| Delete account | Removes the account from every pool and deletes it; request history stays in the ledger. |
| Disconnect | Removes the credential; request history stays in the ledger. |
| Pair or Re-pair | Opens the pairing dialog described above. |
| Pause and Resume | A paused account takes no traffic. |
| Verify | `api_key` only; asks the account's host for its model list. |

The console warns before Clear cooldown runs: "The account becomes eligible immediately. If the account is still
rate limited, the next request re-enters cooldown."
