# phoebis-profile

Pinned wire constants of Claude Code 2.1.269 on Linux x64, and the fixture files measured from it.

## Fixtures

The 18 files captured on 2026-09-13 come from the pinned 2.1.269 binary running against loopback receivers with
dummy credentials; the six `count_apikey`, `count_oauth`, and `count_oauth_fp` files (`.http` and `.json` each)
come from a 2026-09-21 capture of the 2.1.278 build against the same loopback receivers (`docs/site/development.md`
records both captures).

- `body_apikey.json`, `body_fp_9b244.json`, `body_fp_e142e.json`, `body_oauth.json`: the four captured
  `/v1/messages` request bodies (API-key, first-party OAuth, non-first-party OAuth, and a second first-party
  sample), byte for byte.
- `control_profile.http`, `control_roles.http`, `control_token.http`, `control_usage.http`: request heads for the
  control-plane login and usage endpoints, with their captured bodies.
- `count_apikey.http`, `count_apikey.json`, `count_oauth.http`, `count_oauth.json`, `count_oauth_fp.http`,
  `count_oauth_fp.json`: the three `/v1/messages/count_tokens` request heads and bodies of the Read tool's
  pre-turn count, one per credential mode.
- `hello.http`: the `HEAD /api/hello` request head.
- `hello_control.hex`, `hello_inference.hex`: raw ClientHello bytes captured on the control and inference TLS
  connections, including the 5-byte TLS record header.
- `inference_apikey.http`, `inference_oauth.http`, `inference_oauth_fp.http`: request heads for the three
  `/v1/messages` credential modes (API-key, non-first-party OAuth, first-party OAuth).
- `responses/`: the synthetic bodies the capture harness's fake upstream returned to the login and usage requests.

## Re-pinning

A new Claude Code build requires a re-capture and a diff against these fixtures before the profile is re-pinned.
`scripts/capture-baseline.sh <binary> <output-dir>` reproduces the inference-plane half of the capture recipe
against loopback receivers; its header comment covers the control-plane half, which needs a live login session and
cannot run unattended. `scripts/capture-count-tokens.sh <binary> <output-dir>` reproduces the count half beside it,
past a receiver that answers with a canned tool call so the Read tool's pre-turn count reaches the wire.
`scripts/build-fixtures.py --check <capture-dir>` builds fixtures from a capture directory into a temporary
location, normalizes the per-run randomness out of both trees, and diffs the result against this directory,
printing `fixtures match` or the differing files; `--count` alongside `--check` builds and grades the six count
fixtures the same way.

## Redaction

The only redacted values are the credentials (`Authorization: Bearer`, `x-api-key`, and the refresh token in the
token-endpoint body) and the `Host` line of each request head. Every request body is native, and
`X-Claude-Code-Session-Id`, `x-client-request-id`, and the `metadata.user_id` identifiers keep their captured
values because the CCH digits in the heads are the hash of those exact bytes.
