# Pairing an account

An `api_key` account needs no pairing; its key is stored when the account is created. An `oauth` account is
paired: the router hands you an authorization URL, you sign in, and you paste back the address the browser lands
on. This page walks the pairing dialog in the console, the same flow with `curl`, and what an account looks like
afterwards.

## Two kinds of account

An `oauth` account is a Claude.ai or Console login, paired through the dialog or the API below. An `api_key`
account holds a key toward `api.anthropic.com`. Creating an `api_key` account verifies the key against its host
at once: a rejected key answers `400 credential rejected by the upstream`, while a host that serves no model
list leaves the account ready with the error recorded and no models.

## Through the console

Open Upstreams and choose Add upstream. The dialog's subtitle names the choice: subscription logins share your Claude.ai
quota, while API keys bill per token against a Console workspace. Pick OAuth login, give the upstream an id (lowercase
letters, digits, and dashes; it names the upstream in every log and URL) and a label, then choose Start pairing.

The pairing dialog runs three steps.

### Log in

Open the authorization link, sign in to Claude.ai, and approve the router. The browser then lands on a callback
address; keep that tab open for the next step.

### Paste the address

Paste the URL your browser landed on, or the code alone. The page will not load, since nothing listens on that
port; the URL in the address bar is complete, and the code alone works too.

### Confirm

Confirm the identity shown, the account's email and organization, to store the refresh token for this upstream.

The console starts every pairing as a `claude_ai` login, authorizing against `claude.com`; a `console` login,
authorizing against `platform.claude.com`, is available through the API. A pairing session expires ten minutes
after it starts, and the dialog shows the remaining time.

![The Upstreams view: the list on the left, the selected upstream's actions, identity, usage, served models, and
credential on the right](assets/upstreams-light.png#only-light)
![The Upstreams view: the list on the left, the selected upstream's actions, identity, usage, served models, and
credential on the right](assets/upstreams-dark.png#only-dark)

## With `curl`

Whenever `admin.origin` is set, every admin mutation has to carry a matching `Origin` header, and the router
refuses the request otherwise. With `origin` unset, a request that carries no `Origin` is admitted, and one
naming another host is refused the same way. Every request that carries a body also needs `Content-Type:
application/json` (the curl lines below already carry it). Log in first and keep the cookie:

```sh
curl -sS -c jar.txt -X POST https://phoebis.example/admin/session \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"secret":"'"$ROUTER_ADMIN_SECRET"'"}'
```

Start a pairing session for the account slot. The answer carries an `id` and an `authorization_url`; `login`
selects `claude_ai` (the default) or `console`:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"account_id":"business","login":"claude_ai"}'
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

`GET /admin/accounts` lists the slots and their states, and `GET /admin/accounts/{id}/usage` reports the
account's usage windows.

A loopback deployment behind an SSH tunnel uses `http://127.0.0.1:8081` in place of `https://phoebis.example` in
every example above, and sends no `Origin`.

## Account states

| State | What puts an account there | What moves it on |
| --- | --- | --- |
| `disconnected` | Disconnect, on an `oauth` account | Pair or Re-pair |
| `funding_required` | An `api_key` upstream named its credit balance | Clear funding, back to `ready` |
| `ready` | Pairing confirms, an `api_key` verifies, or a hold clears | Reauth, funding, or Disconnect |
| `reauth_required` | A definitive credential failure; see note below | Re-pair, or a new `api_key` secret |
| `unpaired` | No credential yet, or Disconnect on an `api_key` account | Pairing, or a stored `api_key` secret |

Starting or running a pairing session leaves the account's state unchanged, `unpaired` or `reauth_required` for a
re-pair, for the whole flow; the console tracks progress through the pairing session's own status instead.
Renewing an access token leaves the account's state at `ready` for the whole attempt, win or lose; only a
definitive failure moves it.

`reauth_required` follows a single definitive failure, never a retry count: a 401 from an `api_key` upstream, an
`oauth` refresh that finds no stored token, gets a 401 or `invalid_grant` from the token endpoint, or resolves to
a mismatched identity, or a request retried once with a freshly refreshed token that still comes back 401.

The Upstreams view offers these actions, each calling its own route:

| Action | What it does |
| --- | --- |
| Clear cooldown | Clears the account's cooldown immediately. |
| Clear funding | Clears a `funding_required` account back to `ready`. |
| Disconnect | Removes the credential and the account's extra headers; request history stays in the ledger. |
| Pair or Re-pair | Opens the pairing dialog described above. |
| Pause and Resume | A paused account takes no traffic. |
| Re-sync | Runs discovery again. |
| Verify | `api_key` only; asks the account's host for its model list. |

The console warns before Clear cooldown runs: "The upstream becomes eligible immediately. If the upstream is
still rate limited, the next request re-enters cooldown."

A viewer sees every one of these pages and none of the buttons, and may refresh usage.
