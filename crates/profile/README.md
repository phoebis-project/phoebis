# phoebis-profile

Pinned wire constants of Claude Code 2.1.310 on Linux x64, and the fixture files measured from it.

## Fixtures

The 23 files under `fixtures/` belong to one capture of 2026-10-27 of the pinned 2.1.310 binary running against
loopback receivers with dummy credentials: the inference plane and the count path from print-mode runs, and the control
plane from a scripted `/login`, `/status` and `/usage` session in a pseudo-terminal and a print-mode run started on an
expired token, every control call tunneled through `HTTPS_PROXY` to a receiver that answered with canned responses.
`scripts/build-fixtures.py` wrote the 19 captured fixtures from that capture, and nothing else writes one. The four
files under `responses/` are the canned answers the control receiver read during the capture, kept by hand.
[The pinned client build](../../docs/site/development.md#the-pinned-client-build) records the binary, the capture and
the manifest.

- `body_apikey.json`, `body_fp.json`, `body_fp_again.json`, `body_oauth.json`: the four captured `/v1/messages`
  request bodies, one per inference run (an API key, an OAuth token with the first-party override, the override a
  second time as a second hash vector, and an OAuth token without it), the client's bytes after the redaction below.
- `control_profile.http`, `control_roles.http`, `control_token.http`, `control_token_code.http`,
  `control_usage.http`: the request heads of the profile read, the roles read, the refresh grant, the code grant and
  the usage read, with their captured bodies.
- `count.json`, `count_apikey.http`, `count_oauth.http`, `count_oauth_fp.http`: the one `/v1/messages/count_tokens`
  body of the Read tool's pre-turn count, which the three credential modes send byte for byte, and the three heads.
- `hello.http`: the `HEAD /api/hello` request head.
- `hello_control.hex`, `hello_inference.hex`: the raw ClientHello bytes of the control and inference TLS connections,
  the 5-byte TLS record header included.
- `inference_apikey.http`, `inference_oauth.http`, `inference_oauth_fp.http`: the request heads of the three
  `/v1/messages` credential modes (an API key, an OAuth token, and an OAuth token with the first-party override).
- `responses/`: the canned bodies the capture's control receiver returned to the token, profile, roles and usage
  requests.

## Re-pinning

A new Claude Code build requires a re-capture and a diff against these fixtures before the profile is re-pinned.
`scripts/capture-baseline.sh <binary> <output-dir>` reproduces the inference-plane half of the capture against
loopback receivers, the hello probe run included. `scripts/capture-count-tokens.sh <binary> <output-dir>` reproduces
the count half past a receiver that answers with a canned tool call, so the Read tool's pre-turn count reaches the
wire. `scripts/capture-control.sh <binary> <output-dir>` records the control plane in two variants, with and without
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`, the login driven by `scripts/capture/drive_login.py` in a pseudo-terminal.
`scripts/build-fixtures.py --check <capture-dir>` builds fixtures from a capture directory into a temporary location,
normalizes the per-run randomness out of both trees, and diffs the result against this directory, printing
`fixtures match` or the differing files. `--control <variant-dir>` beside it grades the control fixtures from that
variant's recordings, and without one the check lists them under `not captured (control plane):`.
`--count <count-dir> --check` builds and grades the four count fixtures the same way. The write commands, the profile
value and the manifest a re-pin changes are on [Re-pinning](../../docs/site/development.md#re-pinning).

## Redaction

The 19 captured fixtures are what `scripts/build-fixtures.py` wrote from the capture and are never edited by hand. The
builder puts a placeholder in for the device id, the session id (one value in the session header and in
`metadata.user_id`), the prompt id (one value in the prompt id header and in the billing line's `cc_prompt_id`), the
request id header, the tokens and the code grant's `code`, `code_verifier` and `state`, empties `account_uuid`,
rewrites the receiver's loopback `Host` to `api.anthropic.com`, and re-signs the `cch` of each body that carries one
over the redacted bytes. Everything else is the client's bytes.
