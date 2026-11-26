# API reference

The admin API is documented by the router itself: an OpenAPI document and an interactive page over it, both served by
the admin listener out of the binary. This page says where they are and how to use them. The access rules, the shared
shapes and every operation's request, answer and refusals live in the document and nowhere else.

## Where it is

Open `http://<admin listener>/api-reference/` in a browser, where `<admin listener>` is the address the `admin.listen`
key names (see [Installation](installation.md)), `https://` where that listener carries a certificate pair. No login:
the page loads without a session, and every call it makes carries the admin secret as a bearer instead of a cookie.

The document alone is at `http://<admin listener>/api-reference/openapi.json`, OpenAPI 3.1 as JSON, for a script or a
client generator:

```sh
curl -s http://127.0.0.1:8081/api-reference/openapi.json | jq '.info.version, (.paths | length)'
```

## The bearer rule

Type the admin secret into the page's Authorization input. Every request the page sends then carries
`Authorization: Bearer <admin secret>`, which the gate admits without a cookie and without the `Origin` check. A wrong
secret answers `401 secret does not match` and counts toward the login throttle, so five wrong tries in a minute answer
`429 too many failed logins, try again in a minute` to the page and to the console's login alike for the rest of that
minute. The page keeps the secret in memory and never stores it, so it is gone with the tab.

## What the document covers

The operations of the admin listener under `/admin/`, grouped by area in the page's sidebar, each with its parameters,
its answer's shape with an example captured from the route, and its refusals with their status and message. Not in it:
the proxy listener's routes, which [The client](client.md) describes, the events route, which [The
relay](relay.md#the-companion-protocol) describes, and the metrics listener, which [Statistics](statistics.md)
describes.
