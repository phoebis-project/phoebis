# phoebis-profile

Pinned wire constants of Claude Code 2.1.269 on Linux x64, and the fixture files measured from it.

## Fixtures

All files under `fixtures/` were captured on 2026-09-13 from the pinned 2.1.269 binary running against loopback
receivers with dummy credentials.

- `body_apikey.json`, `body_fp_9b244.json`, `body_fp_e142e.json`, `body_oauth.json`: the four captured
  `/v1/messages` request bodies (API-key, first-party OAuth, non-first-party OAuth, and a second first-party
  sample), byte for byte.
- `control_profile.http`, `control_roles.http`, `control_token.http`, `control_usage.http`: request heads for the
  control-plane login and usage endpoints, with their captured bodies.
- `hello.http`: the `HEAD /api/hello` request head.
- `hello_control.hex`, `hello_inference.hex`: raw ClientHello bytes captured on the control and inference TLS
  connections, including the 5-byte TLS record header.
- `inference_apikey.http`, `inference_oauth.http`, `inference_oauth_fp.http`: request heads for the three
  `/v1/messages` credential modes (API-key, non-first-party OAuth, first-party OAuth).
- `responses/`: the synthetic bodies the capture harness's fake upstream returned to the login and usage requests.

A new Claude Code build requires a re-capture and diff against these fixtures before the profile is re-pinned.

## Redaction

The only redacted values are the credentials (`Authorization: Bearer`, `x-api-key`, and the refresh token in the
token-endpoint body) and the `Host` line of each request head. Every request body is native, and
`X-Claude-Code-Session-Id`, `x-client-request-id`, and the `metadata.user_id` identifiers keep their captured
values because the CCH digits in the heads are the hash of those exact bytes.
