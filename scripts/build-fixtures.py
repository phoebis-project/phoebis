#!/usr/bin/env python3
"""Build crates/profile/fixtures from a capture directory, or --check a capture against them."""
import argparse
import contextlib
import difflib
import hashlib
import io
import json
import os
import pathlib
import re
import shutil
import struct
import subprocess
import sys
import tempfile

DEFAULT_CAPTURE = pathlib.Path.home() / "workspace/cc-capture-2026-09-13/cc-capture"
FIXTURES = pathlib.Path("crates/profile/fixtures")

# Bound on the unified diff --check prints per body. A whole Messages body runs to thousands of pretty-printed
# lines; the operator needs enough to tell a date or a kernel string in the system prompt from a shape change.
DIFF_LINES = 40

# Both ClientHellos carry the same server name, so their byte length is the only thing that tells the inference
# path from the control path in a connect.json.
CONTROL_HELLO_LEN = 1473
INFERENCE_HELLO_LEN = 1499

# The fixtures only the control-plane half of the capture produces. scripts/capture-baseline.sh cannot reach it
# without a live login session, so --check reports these as not captured rather than as drift.
CONTROL_PLANE = {
    "control_profile.http",
    "control_roles.http",
    "control_token.http",
    "control_usage.http",
    "hello_control.hex",
    "responses/profile.json",
    "responses/roles.json",
    "responses/token.json",
    "responses/usage.json",
}

# The relay answers HEAD /api/hello itself, so a router capture never carries one; and although tls_raw.py
# peeks and records the router's own ClientHello on every connection, that shape is graded by
# crates/transport/tests/client_hello.rs, not by this comparison. --check reports all of these as not captured
# rather than as drift when --through-router is given, on top of everything the control plane never reaches.
UNCAPTURED_THROUGH_ROUTER = CONTROL_PLANE | {"hello.http", "hello_inference.hex", "hello_control.hex"}

# The header names a live capture must never let reach disk unredacted; scripts/capture/tls_raw.py blanks their
# values at write time, and this mask keeps that redaction and a fixture's dummy credential comparing equal.
CREDENTIAL_HEADERS = {"authorization", "cookie", "proxy-authorization", "x-api-key"}

# Set from --through-router: a capture taken at the router's own upstream needs different grading than one
# taken at the measured client, since the relay it passed through rewrote the wire shape on the way.
THROUGH_ROUTER = False

# The inference runs the capture drives, in the order it drives them, and the fixture each one's body becomes.
INFERENCE_BODIES = ["body_oauth.json", "body_fp_9b244.json", "body_apikey.json", "body_fp_e142e.json"]

# The three of those runs whose request head is pinned. The fourth repeats the first-party override and is kept
# for its body alone, as a second hash vector.
INFERENCE_HEADS = ["inference_oauth.http", "inference_oauth_fp.http", "inference_apikey.http"]

# The credential modes the count capture drives, each its own directory, and the fixture pair each one's count
# request becomes.
COUNT_MODES = ["count_oauth", "count_oauth_fp", "count_apikey"]

# The fixtures only the count capture produces. The baseline and the router capture never send a count, so
# --check without --count reports these as not captured rather than as drift, and --count --check requires them.
COUNT_FIXTURES = {f"{mode}{suffix}" for mode in COUNT_MODES for suffix in (".json", ".http")}

# The two header values a session mints fresh on every run; masked so --check reports profile
# drift, not the identifiers themselves.
HEADER_PLACEHOLDERS = {
    "x-claude-code-session-id": "SESSION-ID-PLACEHOLDER",
    "x-client-request-id": "REQUEST-ID-PLACEHOLDER",
}

# The billing block, and the two values inside it a session mints fresh on every run. A new cc_prompt_id changes
# the cch digits with it, so masking the id alone would still leave --check reporting drift with no protocol change.
BILLING_BLOCK = re.compile(rb'"x-anthropic-billing-header:[^"]*"')
BILLING_PROMPT_ID = re.compile(rb"cc_prompt_id=[^;\"]*;")
BILLING_STAMP = re.compile(rb"cch=[0-9a-f]{5};")

# The billing fields the relay restores on a reduced-shape request; under THROUGH_ROUTER these are dropped
# outright rather than masked, since a baseline that never carried one has no value for a placeholder to replace.
RESTORED_BILLING_FIELDS = (b"cch", b"cc_prev_req", b"cc_prompt_id")

# The relay puts the serving account's identity into the escaped metadata string; a capture taken at the router's
# upstream carries it where the native capture, made with a dummy token, carried nothing.
ACCOUNT_UUID = re.compile(rb'(\\"account_uuid\\":\\")[^\\]*(\\")')

# The values a written fixture carries in place of the run's own ids. Each keeps its field's shape, 64 hex digits or
# a version 4 uuid, so every parser that reads the fields still parses a written fixture. The session header and
# the body's session_id share one placeholder, as the client sends one uuid in both, so the relay's reading of
# header and body stays consistent.
DEVICE_ID_PLACEHOLDER = "0123456789abcdef" * 4
PROMPT_ID_PLACEHOLDER = "33333333-3333-4333-8333-333333333333"
REQUEST_ID_PLACEHOLDER = "22222222-2222-4222-8222-222222222222"
SESSION_ID_PLACEHOLDER = "11111111-1111-4111-8111-111111111111"


def redact(line: str) -> str:
    line = re.sub(r"Bearer sk-ant-oat01-\S+", "Bearer sk-ant-oat01-PLACEHOLDER", line)
    line = re.sub(r"x-api-key: sk-ant-api03-\S+", "x-api-key: sk-ant-api03-PLACEHOLDER", line)
    line = re.sub(r"^Host: 127\.0\.0\.1:\d+$", "Host: api.anthropic.com", line)
    # The client spells these two names in different cases, so they match in any case and keep their spelling.
    line = re.sub(r"(?i)^(x-claude-code-session-id:\s*)\S+$", rf"\g<1>{SESSION_ID_PLACEHOLDER}", line)
    line = re.sub(r"(?i)^(x-client-request-id:\s*)\S+$", rf"\g<1>{REQUEST_ID_PLACEHOLDER}", line)
    return line


def redact_body(raw: bytes) -> bytes:
    """Replace metadata.user_id's device_id and session_id with their placeholders, empty its account_uuid, and
    replace every billing block's cc_prompt_id.

    The members are matched in their escaped form, the form they take inside the user_id string, across the whole
    body as normalize_body masks them, so a written file holds no id the comparison would hide. account_uuid is
    emptied in every mode: the native capture's dummy token leaves it empty, a capture at the router's upstream
    carries the serving account there, and the committed shape is the empty value.
    """
    device = DEVICE_ID_PLACEHOLDER.encode()
    session = SESSION_ID_PLACEHOLDER.encode()
    prompt = b"cc_prompt_id=" + PROMPT_ID_PLACEHOLDER.encode() + b";"
    raw = re.sub(rb'(\\"device_id\\":\\")[^\\]*(\\")', lambda m: m.group(1) + device + m.group(2), raw)
    raw = re.sub(rb'(\\"session_id\\":\\")[^\\]*(\\")', lambda m: m.group(1) + session + m.group(2), raw)
    raw = ACCOUNT_UUID.sub(lambda m: m.group(1) + m.group(2), raw)
    return BILLING_BLOCK.sub(lambda m: BILLING_PROMPT_ID.sub(lambda _: prompt, m.group(0)), raw)


def declared_length(entry):
    """The Content-Length the capture recorded on this entry's head, or None when it carried none."""
    for line in entry["headers"]:
        name, colon, value = line.partition(":")
        if colon and name.strip().lower() == "content-length":
            return int(value.strip())
    return None


def head(entry, content_length=None):
    lines = [entry["request_line"]] + [redact(h) for h in entry["headers"]]
    if content_length is not None:
        lines = [l if not l.startswith("Content-Length:") else f"Content-Length: {content_length}" for l in lines]
    return ("\r\n".join(lines) + "\r\n\r\n").encode()


def control(entry, name, out):
    body = entry.get("body", "")
    body = re.sub(r"sk-ant-ort01-\S+?\"", 'sk-ant-ort01-PLACEHOLDER"', body)
    lines = [entry["request_line"]] + [redact(h) for h in entry["headers"]]
    lines = [l if not l.startswith("Content-Length:") else f"Content-Length: {len(body)}" for l in lines]
    (out / name).write_bytes(("\r\n".join(lines) + "\r\n\r\n").encode() + body.encode())


def inference_entries(cm: list) -> list:
    """The capture's inference posts, matched on the path alone: a count request's line starts with the
    inference path as well, and zipping one onto an inference fixture name would mis-pin that fixture."""
    return [
        (i, e)
        for i, e in enumerate(cm)
        if e["request_line"].split(" ")[1].split("?")[0] == "/v1/messages" and e["request_line"].startswith("POST ")
    ]


def count_entries(cm: list) -> list:
    """The capture's token-count posts, the one request the count script's receiver provokes."""
    return [(i, e) for i, e in enumerate(cm) if e["request_line"].startswith("POST /v1/messages/count_tokens")]


def build(src: pathlib.Path, out: pathlib.Path, resign: bool = True) -> None:
    """Write the fixture tree at `out`, sourced from the capture directory `src`.

    `src/mitm/` is optional: a capture without it holds the inference plane only, and the control fixtures and
    canned responses are left unwritten for `check()` to report as not captured.

    The bodies are written redacted, and with `resign` their cch digits are re-signed over the redacted bytes.
    `check()` builds with `resign` off, since its comparison masks the digits and `check_hashes` grades the raw
    bodies' own.
    """
    (out / "responses").mkdir(parents=True, exist_ok=True)
    cm = json.load(open(src / "clean_modes.json"))
    # Selected by request line rather than by index: a capture that interleaves its HEAD /api/hello probes
    # differently still lands the same four bodies on the same four fixtures.
    posts = inference_entries(cm)
    if len(posts) != len(INFERENCE_BODIES):
        raise SystemExit(f"{src}/clean_modes.json holds {len(posts)} POST /v1/messages entries, want 4")
    captured = [(src / f"clean_body_{idx}.json").read_bytes() for idx, _ in posts]
    if THROUGH_ROUTER:
        # head() below regenerates Content-Length from the redacted body, and the router grading masks the field,
        # so a body the declared length does not describe would normalize to the same bytes as one it does.
        # Checked against the captured bytes, before the redaction changes their length.
        for (idx, entry), body in zip(posts, captured):
            declared = declared_length(entry)
            if declared != len(body):
                raise SystemExit(
                    f"{src}/clean_modes.json entry {idx} ({entry['request_line']}) declares Content-Length "
                    f"{declared}, but the body captured beside it is {len(body)} bytes"
                )
    for body, name in zip(captured, INFERENCE_BODIES):
        (out / name).write_bytes(redact_body(body))
        if resign:
            resign_written(out / name)
    for (_, entry), name, body_name in zip(posts, INFERENCE_HEADS, INFERENCE_BODIES):
        (out / name).write_bytes(head(entry, len((out / body_name).read_bytes())))
    # The relay answers HEAD /api/hello itself, so a router capture never carries one; both it and the hex
    # files below are optional in this mode.
    hello = next((e for e in cm if e["request_line"].startswith("HEAD /api/hello")), None)
    if hello is None:
        if not THROUGH_ROUTER:
            raise SystemExit(f"{src}/clean_modes.json holds no HEAD /api/hello entry")
    else:
        (out / "hello.http").write_bytes(head(hello))
    # connect.json, which tls_raw.py always writes, holds the ClientHello the router itself sent toward its
    # upstream; that shape is graded by crates/transport/tests/client_hello.rs, not here, so it is never opened
    # in this mode even though it exists.
    if not THROUGH_ROUTER:
        conn = json.load(open(src / "connect.json"))
        inf = next((h for h in conn if h.get("hello_len") == INFERENCE_HELLO_LEN), None)
        if inf is None:
            raise SystemExit(f"{src}/connect.json holds no {INFERENCE_HELLO_LEN}-byte ClientHello")
        (out / "hello_inference.hex").write_text(inf["raw_hex"] + "\n")
        ctl = next((h for h in conn if h.get("hello_len") == CONTROL_HELLO_LEN), None)
        if ctl is not None:
            (out / "hello_control.hex").write_text(ctl["raw_hex"] + "\n")
    if not (src / "mitm").is_dir():
        return
    login = json.load(open(src / "mitm/login.json"))
    status = json.load(open(src / "mitm/status.json"))
    control(login[0], "control_token.http", out)
    control(login[1], "control_profile.http", out)
    control(login[3], "control_roles.http", out)
    control(status[15], "control_usage.http", out)
    # The tunnel harness answered these synthetic bodies; they are the fake upstream's canned responses.
    harness = (src / "mitm/mitm5.py").read_text()
    for name, var in [("token", "TOKEN"), ("profile", "PROFILE"), ("usage", "USAGE")]:
        m = re.search(rf"^{var}=json\.dumps\((.*)\)\.encode\(\)$", harness, re.M)
        assert m, f"mitm5.py no longer defines {var} as a single json.dumps(...).encode() literal"
        value = eval(m.group(1), {"False": False, "None": None, "True": True})
        if name == "token":
            # The canned token response's access/refresh tokens are credential-shaped; redact them like every other.
            value["access_token"] = "sk-ant-oat01-PLACEHOLDER"
            value["refresh_token"] = "sk-ant-ort01-PLACEHOLDER"
        (out / "responses" / f"{name}.json").write_text(json.dumps(value, separators=(",", ":")) + "\n")
    (out / "responses/roles.json").write_text('{"organization_role":"admin","workspace_role":"workspace_admin"}\n')


def build_count(src: pathlib.Path, out: pathlib.Path) -> None:
    """Write count_<mode>.json and count_<mode>.http for each mode directory under `src`, from the one
    count_tokens request each run recorded; the inference entries beside it are the client's turn and are not
    extracted, since the inference fixtures stay pinned to the build that measured them."""
    for mode in COUNT_MODES:
        cm = json.load(open(src / mode / "clean_modes.json"))
        counts = count_entries(cm)
        if len(counts) != 1:
            raise SystemExit(f"{src}/{mode}/clean_modes.json holds {len(counts)} count_tokens entries, want 1")
        idx, entry = counts[0]
        body = (src / mode / f"clean_body_{idx}.json").read_bytes()
        declared = declared_length(entry)
        if declared != len(body):
            raise SystemExit(
                f"{src}/{mode}/clean_modes.json declares Content-Length {declared} on its count_tokens entry, "
                f"but the body captured beside it is {len(body)} bytes"
            )
        (out / f"{mode}.json").write_bytes(body)
        (out / f"{mode}.http").write_bytes(head(entry, len(body)))


def mask_hello(raw: bytes) -> bytes:
    """Python port of transport::hello::mask; zeroes per-run randomness, leaves structure and lengths intact."""
    h = bytearray(raw)
    random = 11  # record(5) + handshake header(4) + version(2)
    if len(h) < random + 32:
        raise ValueError(f"ClientHello truncated before the client random ends at offset {random + 32}")
    h[random : random + 32] = b"\x00" * 32
    if len(h) <= random + 32:
        raise ValueError(f"ClientHello truncated at the session id length, offset {random + 32}")
    sid_len = h[random + 32]
    sid = random + 33
    if len(h) < sid + sid_len:
        raise ValueError(f"ClientHello truncated before the session id ends at offset {sid + sid_len}")
    h[sid : sid + sid_len] = b"\x00" * sid_len
    p = sid + sid_len
    if len(h) < p + 2:
        raise ValueError(f"ClientHello truncated at the cipher suite length, offset {p}")
    cipher_len = struct.unpack("!H", h[p : p + 2])[0]
    p += 2 + cipher_len
    if len(h) <= p:
        raise ValueError(f"ClientHello truncated at the compression method length, offset {p}")
    comp_len = h[p]
    p += 1 + comp_len
    if len(h) < p + 2:
        raise ValueError(f"ClientHello truncated at the extensions length, offset {p}")
    ext_total = struct.unpack("!H", h[p : p + 2])[0]
    p += 2
    end = p + ext_total
    if len(h) < end:
        raise ValueError(f"ClientHello truncated before the extensions block ends at offset {end}")
    while p + 4 <= end:
        ty, length = struct.unpack("!HH", h[p : p + 4])
        data = p + 4
        if len(h) < data + length:
            raise ValueError(f"ClientHello truncated inside extension {ty}, offset {data + length}")
        if ty == 51:  # key_share: zero every group's payload, keep the group id and length
            q = data + 2
            while q + 4 <= data + length:
                klen = struct.unpack("!H", h[q + 2 : q + 4])[0]
                if len(h) < q + 4 + klen:
                    raise ValueError(f"ClientHello truncated inside a key share payload, offset {q + 4 + klen}")
                h[q + 4 : q + 4 + klen] = b"\x00" * klen
                q += 4 + klen
        p = data + length
    return bytes(h)


def _self_test() -> None:
    _self_test_mask_hello()
    _self_test_normalize_body()
    _self_test_declared_length()
    _self_test_count_selector()
    _self_test_capture_partition()
    _self_test_redact_body()
    _self_test_redact_head()
    _self_test_build_redacts()
    _self_test_resign_written()


def _self_test_declared_length() -> None:
    """A router capture whose head and body disagree on the length must stop the build, naming the entry."""
    global THROUGH_ROUTER
    bodies = [b'{"model":"a"}', b'{"model":"b"}', b'{"model":"c"}', b'{"model":"d"}']
    def entry(body):
        return {
            "request_line": "POST /v1/messages HTTP/1.1",
            "headers": ["Host: api.anthropic.com", f"Content-Length: {len(body)}"],
            "body": body.decode(),
        }

    with tempfile.TemporaryDirectory() as tmp:
        src = pathlib.Path(tmp) / "capture"
        src.mkdir()
        entries = [entry(b) for b in bodies]
        # The third entry declares one byte fewer than it carries, the drift the router capture cannot see.
        entries[2]["headers"][1] = f"Content-Length: {len(bodies[2]) - 1}"
        (src / "clean_modes.json").write_text(json.dumps(entries))
        for i, body in enumerate(bodies):
            (src / f"clean_body_{i}.json").write_bytes(body)
        THROUGH_ROUTER = True
        try:
            build(src, pathlib.Path(tmp) / "good")
        except SystemExit as e:
            if "entry 2" not in str(e) or "Content-Length" not in str(e):
                raise AssertionError(f"the build refused for the wrong reason: {e}") from e
            print("self-test passed:", e)
        else:
            raise AssertionError("the build accepted a body its declared Content-Length does not describe")
        finally:
            THROUGH_ROUTER = False


def _self_test_count_selector() -> None:
    """A count request's line starts with the inference path too, so the two selectors must not overlap."""
    entries = [
        {"request_line": "POST /v1/messages?beta=true HTTP/1.1", "headers": [], "body": ""},
        {"request_line": "POST /v1/messages/count_tokens?beta=true HTTP/1.1", "headers": [], "body": ""},
    ]
    posts = [i for i, _ in inference_entries(entries)]
    counts = [i for i, _ in count_entries(entries)]
    if posts != [0]:
        raise AssertionError(f"the inference selector took entries {posts}, want the first alone")
    if counts != [1]:
        raise AssertionError(f"the count selector took entries {counts}, want the second alone")
    print("self-test passed: the inference selector and the count selector take one entry each")


def _count_capture(root: pathlib.Path, session: bytes) -> pathlib.Path:
    """A capture directory shaped like the one the count script writes: one inference entry and one count entry
    per mode, the count body carrying a per-run session id for the normaliser to mask."""
    content = rb'{\"session_id\":\"' + session + rb'\"}'
    body = rb'{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"' + content + rb'"}]}'
    for mode in COUNT_MODES:
        (root / mode).mkdir(parents=True)
        entries = [
            {"request_line": "POST /v1/messages?beta=true HTTP/1.1", "headers": ["Content-Length: 2"], "body": "{}"},
            {
                "request_line": "POST /v1/messages/count_tokens?beta=true HTTP/1.1",
                "headers": ["Host: 127.0.0.1:48442", f"Content-Length: {len(body)}"],
                "body": body.decode(),
            },
        ]
        (root / mode / "clean_modes.json").write_text(json.dumps(entries))
        (root / mode / "clean_body_0.json").write_bytes(b"{}")
        (root / mode / "clean_body_1.json").write_bytes(body)
    return root


def _self_test_capture_partition() -> None:
    """The six count fixtures are exempt from every other capture's grade and required by the count's own.

    Both halves read the code under test rather than a copy of it: the headings come from `partition_names`,
    which `check` calls, and the missing report from `check_count` itself.
    """
    committed = {name: b"x" for name in sorted(COUNT_FIXTURES)}
    committed["control_token.http"] = b"y"
    committed["inference_oauth.http"] = b"z"
    fresh = {"inference_oauth.http": b"z"}
    for label, reachable in (("baseline", CONTROL_PLANE), ("router", UNCAPTURED_THROUGH_ROUTER)):
        uncaptured, counts, diffs = partition_names(committed, fresh, reachable | COUNT_FIXTURES)
        if counts != sorted(COUNT_FIXTURES):
            raise AssertionError(f"the {label} classification put {counts} under the count heading")
        if uncaptured != ["control_token.http"]:
            raise AssertionError(f"the {label} classification put {uncaptured} under its own heading")
        if diffs:
            raise AssertionError(f"the {label} classification read {diffs} as drift rather than as not captured")
    print("self-test passed: a count fixture no other capture writes is reported as not captured, not as drift")

    with tempfile.TemporaryDirectory() as tmp:
        recorded = b"fd27de0e-0e2a-4b3f-8a34-4bd0f39a0f6d"
        capture = _count_capture(pathlib.Path(tmp) / "capture", recorded)
        tree = pathlib.Path(tmp) / "committed"
        tree.mkdir()
        build_count(capture, tree)
        # The committed copy carries the session id of another run: a count body reaches normalize_body only
        # because normalize_tree routes count_ names to it, and without that this reads as drift.
        stale = (tree / "count_oauth.json").read_bytes().replace(recorded, b"b2b1f4ca-1f6e-4a52-9d54-2f9b4a6c0e11")
        (tree / "count_oauth.json").write_bytes(stale)
        if check_count(capture, tree) != 0:
            raise AssertionError("check_count reported drift against the fixtures it just built")
        (tree / "count_apikey.http").unlink()
        report = io.StringIO()
        with contextlib.redirect_stdout(report):
            status = check_count(capture, tree)
        if status == 0 or "count_apikey.http" not in report.getvalue():
            raise AssertionError(f"check_count passed with a count fixture missing: {report.getvalue()!r}")
        print("self-test passed: check_count requires every count fixture and names the one that is gone")

        # A recapture whose head and body disagree on the length must stop the build rather than write a short
        # fixture, the rule the inference build already carries.
        modes = json.loads((capture / "count_apikey" / "clean_modes.json").read_text())
        modes[1]["headers"][1] = "Content-Length: 1"
        (capture / "count_apikey" / "clean_modes.json").write_text(json.dumps(modes))
        try:
            build_count(capture, tree)
        except SystemExit as e:
            if "count_apikey" not in str(e) or "Content-Length" not in str(e):
                raise AssertionError(f"build_count refused for the wrong reason: {e}") from e
            print("self-test passed:", e)
        else:
            raise AssertionError("build_count wrote a count body its declared Content-Length does not describe")


# Invented ids, each of repeated digits, so no run's own value can pass for one of them.
_TEST_ACCOUNT_UUID = b"77777777-7777-4777-8777-777777777777"
_TEST_DEVICE_ID = b"5" * 64
_TEST_PROMPT_ID = b"88888888-8888-4888-8888-888888888888"
_TEST_REQUEST_ID = b"99999999-9999-4999-8999-999999999999"
_TEST_SESSION_ID = b"66666666-6666-4666-8666-666666666666"
_TEST_BODY_IDS = (_TEST_ACCOUNT_UUID, _TEST_DEVICE_ID, _TEST_PROMPT_ID, _TEST_SESSION_ID)


def _identified_body(model: bytes) -> bytes:
    """A body in the client's shape whose metadata.user_id and billing block carry the invented ids, with a
    nonempty account_uuid as a capture at the router's upstream carries."""
    user_id = (
        rb'{\"device_id\":\"' + _TEST_DEVICE_ID + rb'\",\"account_uuid\":\"' + _TEST_ACCOUNT_UUID
        + rb'\",\"session_id\":\"' + _TEST_SESSION_ID + rb'\"}'
    )
    return (
        rb'{"model":"' + model + rb'","system":[{"type":"text","text":"x-anthropic-billing-header: '
        + b"cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cch=00000; cc_prompt_id=" + _TEST_PROMPT_ID
        + rb';"}],"metadata":{"user_id":"' + user_id + rb'"}}'
    )


def _inference_capture(root: pathlib.Path) -> pathlib.Path:
    """A capture directory shaped like the one the baseline script writes: the hello probe, then the four
    inference posts, each head and body carrying the invented ids."""
    root.mkdir(parents=True)
    entries = [{"request_line": "HEAD /api/hello HTTP/1.1", "headers": ["Host: 127.0.0.1:48442"], "body": ""}]
    for i, name in enumerate(INFERENCE_BODIES, start=1):
        body = _identified_body(name.encode())
        entries.append(
            {
                "request_line": "POST /v1/messages?beta=true HTTP/1.1",
                "headers": [
                    "Authorization: Bearer sk-ant-oat01-DUMMYOAUTH",
                    "X-Claude-Code-Session-Id: " + _TEST_SESSION_ID.decode(),
                    "x-client-request-id: " + _TEST_REQUEST_ID.decode(),
                    "Host: 127.0.0.1:48442",
                    f"Content-Length: {len(body)}",
                ],
                "body": body.decode(),
            }
        )
        (root / f"clean_body_{i}.json").write_bytes(body)
    (root / "clean_modes.json").write_text(json.dumps(entries))
    (root / "connect.json").write_text(json.dumps([{"hello_len": INFERENCE_HELLO_LEN, "raw_hex": "16"}]))
    return root


def _self_test_redact_body() -> None:
    """A written body carries the placeholders and an empty account_uuid, and a body without the members comes
    back byte for byte, whatever metadata.user_id holds or whether it is there at all."""
    redacted = redact_body(_identified_body(b"claude-sonnet-4-5"))
    for wanted in (
        rb'\"account_uuid\":\"\"',
        rb'\"device_id\":\"' + DEVICE_ID_PLACEHOLDER.encode() + rb'\"',
        rb'\"session_id\":\"' + SESSION_ID_PLACEHOLDER.encode() + rb'\"',
        b"cc_prompt_id=" + PROMPT_ID_PLACEHOLDER.encode() + b";",
    ):
        if wanted not in redacted:
            raise AssertionError(f"redact_body wrote no {wanted!r}")
    for original in _TEST_BODY_IDS:
        if original in redacted:
            raise AssertionError(f"redact_body kept {original!r}")
    # The placeholders keep the fields' shape only if the escaping around them survives as well.
    members = json.loads(json.loads(redacted)["metadata"]["user_id"])
    if members != {"device_id": DEVICE_ID_PLACEHOLDER, "account_uuid": "", "session_id": SESSION_ID_PLACEHOLDER}:
        raise AssertionError(f"redact_body left metadata.user_id parsing as {members!r}")
    print("self-test passed: redact_body writes the placeholders and an empty account_uuid")

    for unrelated in (
        b'{"model":"claude-sonnet-4-5","system":[{"type":"text","text":'
        b'"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cch=00000;"}]}',
        b'{"model":"claude-sonnet-4-5","metadata":{"user_id":null}}',
    ):
        if redact_body(unrelated) != unrelated:
            raise AssertionError(f"redact_body changed a body without the members: {unrelated!r}")
    print("self-test passed: redact_body leaves a body without metadata.user_id's members unchanged")


def _self_test_redact_head() -> None:
    """A written head carries the session and request placeholders, and the credentials and the Host line
    redact as before."""
    cases = {
        "Authorization: Bearer sk-ant-oat01-REAL": "Authorization: Bearer sk-ant-oat01-PLACEHOLDER",
        "Host: 127.0.0.1:48442": "Host: api.anthropic.com",
        "x-api-key: sk-ant-api03-capture-dummy": "x-api-key: sk-ant-api03-PLACEHOLDER",
        "X-Claude-Code-Session-Id: " + _TEST_SESSION_ID.decode(): f"X-Claude-Code-Session-Id: {SESSION_ID_PLACEHOLDER}",
        "x-client-request-id: " + _TEST_REQUEST_ID.decode(): f"x-client-request-id: {REQUEST_ID_PLACEHOLDER}",
        "X-Stainless-Lang: js": "X-Stainless-Lang: js",
    }
    for line, wanted in cases.items():
        if redact(line) != wanted:
            raise AssertionError(f"redact({line!r}) gave {redact(line)!r}, want {wanted!r}")
    print("self-test passed: redact writes the session and request placeholders into a head")


def _self_test_build_redacts() -> None:
    """In both grading modes every written body is free of the capture's ids, and every inference head carries
    the two placeholders and declares the length of the redacted body it travels with."""
    global THROUGH_ROUTER
    with tempfile.TemporaryDirectory() as tmp:
        src = _inference_capture(pathlib.Path(tmp) / "capture")
        for through_router, mode in ((False, "native"), (True, "--through-router")):
            out = pathlib.Path(tmp) / mode.lstrip("-")
            THROUGH_ROUTER = through_router
            try:
                build(src, out, resign=False)
            finally:
                THROUGH_ROUTER = False
            bodies = sorted(out.glob("body_*.json"))
            if len(bodies) != len(INFERENCE_BODIES):
                raise AssertionError(f"the {mode} build wrote {len(bodies)} bodies, want {len(INFERENCE_BODIES)}")
            for path in bodies:
                body = path.read_bytes()
                leaked = [original for original in _TEST_BODY_IDS if original in body]
                if leaked:
                    raise AssertionError(f"the {mode} build kept {leaked!r} in {path.name}")
                if rb'\"account_uuid\":\"\"' not in body:
                    raise AssertionError(f"the {mode} build wrote {path.name} without an empty account_uuid")
            heads = sorted(out.glob("inference_*.http"))
            if len(heads) != len(INFERENCE_HEADS):
                raise AssertionError(f"the {mode} build wrote {len(heads)} heads, want {len(INFERENCE_HEADS)}")
            body_of = dict(zip(INFERENCE_HEADS, INFERENCE_BODIES))
            for path in heads:
                text = path.read_bytes().decode()
                length = len((out / body_of[path.name]).read_bytes())
                for line in (
                    f"Content-Length: {length}",
                    f"X-Claude-Code-Session-Id: {SESSION_ID_PLACEHOLDER}",
                    f"x-client-request-id: {REQUEST_ID_PLACEHOLDER}",
                ):
                    if f"\r\n{line}\r\n" not in text:
                        raise AssertionError(f"the {mode} build wrote {path.name} without {line!r}")
                for original in (_TEST_REQUEST_ID, _TEST_SESSION_ID):
                    if original.decode() in text:
                        raise AssertionError(f"the {mode} build kept {original!r} in {path.name}")
            print(f"self-test passed: the {mode} build writes redacted bodies and heads")


def _self_test_resign_written() -> None:
    """A body without a stamp is left alone before cargo is looked for, and a stamped body needs cargo."""
    with tempfile.TemporaryDirectory() as tmp:
        stampless = pathlib.Path(tmp) / "body_apikey.json"
        stampless.write_bytes(
            b'{"system":[{"type":"text","text":'
            b'"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli;"}]}'
        )
        stamped = pathlib.Path(tmp) / "body_oauth.json"
        stamped.write_bytes(_identified_body(b"claude-sonnet-4-5"))
        no_cargo = pathlib.Path(tmp) / "bin"
        no_cargo.mkdir()
        saved_path = os.environ.get("PATH")
        os.environ["PATH"] = str(no_cargo)
        try:
            before = stampless.read_bytes()
            report = io.StringIO()
            with contextlib.redirect_stdout(report):
                resign_written(stampless)
            if stampless.read_bytes() != before:
                raise AssertionError("resign_written changed a body without a stamp")
            if "body_apikey.json" not in report.getvalue() or "no cch= stamp" not in report.getvalue():
                raise AssertionError(f"resign_written skipped a body without a stamp silently: {report.getvalue()!r}")
            print("self-test passed: resign_written leaves a body without a stamp alone and says so")
            try:
                resign_written(stamped)
            except SystemExit as e:
                if str(e) != "cargo is needed to re-sign the redacted bodies":
                    raise AssertionError(f"resign_written refused for the wrong reason: {e}") from e
                print("self-test passed:", e)
            else:
                raise AssertionError("resign_written went on with a stamped body and no cargo")
        finally:
            if saved_path is None:
                del os.environ["PATH"]
            else:
                os.environ["PATH"] = saved_path


def _self_test_mask_hello() -> None:
    """Feeds mask_hello a key share declaring a 40-byte payload with only 5 bytes present; must raise ValueError."""
    random_bytes = b"\x00" * 32
    session_id = b""
    ciphers = b"\x13\x01"
    compression = b"\x00"
    key_share_body = (
        b"\x00\x09"  # key-share list length prefix, unused by mask_hello's own walk
        + b"\x00\x1d"  # group id (x25519)
        + struct.pack("!H", 40)  # declared key length: 40 bytes
        + b"\x01\x02\x03\x04\x05"  # only 5 bytes actually present
    )
    extension = struct.pack("!HH", 51, len(key_share_body)) + key_share_body
    handshake_body = (
        b"\x03\x03"  # client_version
        + random_bytes
        + struct.pack("!B", len(session_id))
        + session_id
        + struct.pack("!H", len(ciphers))
        + ciphers
        + struct.pack("!B", len(compression))
        + compression
        + struct.pack("!H", len(extension))
        + extension
    )
    handshake = struct.pack("!B", 1) + struct.pack("!I", len(handshake_body))[1:] + handshake_body
    record = struct.pack("!B", 0x16) + b"\x03\x01" + struct.pack("!H", len(handshake)) + handshake
    try:
        mask_hello(record)
    except ValueError as e:
        if "key share" not in str(e):
            raise AssertionError(f"mask_hello raised for the wrong reason: {e}") from e
        print("self-test passed:", e)
        return
    raise AssertionError("mask_hello did not raise on a truncated key share payload")


def _self_test_normalize_body() -> None:
    """One case per masked billing value: two bodies differing only in it must normalize to the same bytes."""
    block = (
        b'{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517;'
        b' cc_entrypoint=sdk-cli; cch=%s; cc_prompt_id=%s;"}]}'
    )
    first_id = b"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    second_id = b"33333333-3333-4333-8333-333333333333"
    cases = [
        ("the cch digits", block % (b"9b244", first_id), block % (b"e142e", first_id), b"9b244"),
        ("cc_prompt_id", block % (b"9b244", first_id), block % (b"9b244", second_id), first_id),
    ]
    for name, one, other, minted in cases:
        assert one != other, f"self-test case {name} feeds two identical bodies"
        masked = normalize_body(one)
        if masked != normalize_body(other):
            raise AssertionError(f"normalize_body leaves {name} in the body")
        if minted in masked:
            raise AssertionError(f"normalize_body kept {minted!r} after masking {name}")
        if b"cc_version=2.1.269.517" not in masked:
            raise AssertionError(f"masking {name} swallowed the rest of the billing block")
        print(f"self-test passed: normalize_body masks {name}")

    global THROUGH_ROUTER
    uuid_body = rb'"metadata":"{\"account_uuid\":\"00000000-0000-4000-8000-000000000001\"}"'
    length_head = b"POST /v1/messages HTTP/1.1\r\nContent-Length: 114749\r\n\r\n{}"
    THROUGH_ROUTER = True
    try:
        masked_body = normalize_body(uuid_body)
        masked_head = normalize_head(length_head)
    finally:
        THROUGH_ROUTER = False
    if b"00000000-0000-4000-8000-000000000001" in masked_body or b"ACCOUNT-UUID-PLACEHOLDER" not in masked_body:
        raise AssertionError("THROUGH_ROUTER did not mask account_uuid in normalize_body")
    if b"114749" in masked_head or b"Content-Length: LENGTH-PLACEHOLDER" not in masked_head:
        raise AssertionError("THROUGH_ROUTER did not mask Content-Length in normalize_head")
    if b"00000000-0000-4000-8000-000000000001" not in normalize_body(uuid_body):
        raise AssertionError("normalize_body masked account_uuid with THROUGH_ROUTER off")
    if b"114749" not in normalize_head(length_head):
        raise AssertionError("normalize_head masked Content-Length with THROUGH_ROUTER off")
    print("self-test passed: THROUGH_ROUTER masks account_uuid and Content-Length")

    # A baseline that never carried the restored fields, and a live capture where the relay filled them in, must
    # normalize to the same bytes under THROUGH_ROUTER: a field moving from absent to present is the point of
    # restoration, not drift.
    reduced_body = (
        b'{"system":[{"type":"text","text":'
        b'"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli;"}]}'
    )
    restored_body = (
        b'{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517;'
        b" cc_entrypoint=sdk-cli; cch=9b244; cc_prev_req=req_up_0001;"
        b' cc_prompt_id=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa;"}]}'
    )
    reduced_head = b"POST /v1/messages HTTP/1.1\r\nx-app: cli\r\n\r\n"
    restored_head = (
        b"POST /v1/messages HTTP/1.1\r\nx-app: cli\r\n"
        b"x-client-request-id: 33333333-3333-4333-8333-333333333333\r\n\r\n"
    )
    classed_head = b"POST /v1/messages HTTP/1.1\r\nx-app: cli\r\nx-claude-code-request-class: main\r\n\r\n"
    assert reduced_body != restored_body and reduced_head != restored_head, "self-test feeds identical fixtures"
    THROUGH_ROUTER = True
    try:
        if normalize_body(reduced_body) != normalize_body(restored_body):
            raise AssertionError("THROUGH_ROUTER left a restored billing field in the comparison")
        if normalize_head(reduced_head) != normalize_head(restored_head):
            raise AssertionError("THROUGH_ROUTER left x-client-request-id in the comparison")
        if normalize_head(classed_head) != normalize_head(reduced_head):
            raise AssertionError("THROUGH_ROUTER left x-claude-code-request-class in the comparison")
    finally:
        THROUGH_ROUTER = False
    if normalize_body(reduced_body) == normalize_body(restored_body):
        raise AssertionError("normalize_body dropped the restored billing fields with THROUGH_ROUTER off")
    if normalize_head(reduced_head) == normalize_head(restored_head):
        raise AssertionError("normalize_head dropped x-client-request-id with THROUGH_ROUTER off")
    print("self-test passed: THROUGH_ROUTER treats a restored field as absent, not drift")


def normalize_head(raw: bytes) -> bytes:
    """Replace the per-run session/request id header values so a diff reflects profile drift, not a random id.

    Under THROUGH_ROUTER two more values are masked, each keeping its header's name and position: a credential,
    which a live capture must never hold unredacted, and Content-Length, which the relay regenerates from the
    body it edited (a change there is what the body comparison already covers). `x-client-request-id` is dropped
    outright rather than masked, since the relay generates one when the caller sent none: a line moving from
    absent to present is the point of restoration, and a placeholder value could not make the two compare equal.
    `x-claude-code-request-class` is dropped the same way: the replay adds the header the recorded heads predate,
    and a line moving from absent to present is no drift of the profile.
    """
    head_bytes, sep, rest = raw.partition(b"\r\n\r\n")
    lines = head_bytes.decode("latin1").split("\r\n")
    out_lines = []
    for line in lines:
        name, colon, value = line.partition(":")
        key = name.strip().lower()
        if THROUGH_ROUTER and key in ("x-claude-code-request-class", "x-client-request-id"):
            continue
        if THROUGH_ROUTER and key in CREDENTIAL_HEADERS:
            placeholder = "CREDENTIAL-PLACEHOLDER"
        elif THROUGH_ROUTER and key == "content-length":
            placeholder = "LENGTH-PLACEHOLDER"
        else:
            placeholder = HEADER_PLACEHOLDERS.get(key)
        out_lines.append(f"{name}: {placeholder}" if colon and placeholder else line)
    return "\r\n".join(out_lines).encode("latin1") + sep + rest


def mask_billing(block: bytes) -> bytes:
    """Mask one billing block's per-capture values, leaving cc_version and cc_entrypoint to be compared.

    Under THROUGH_ROUTER, `strip_restored_billing_fields` runs instead: masking assumes a field is present on
    both sides, which a restored one is not.
    """
    if THROUGH_ROUTER:
        return strip_restored_billing_fields(block)
    block = BILLING_STAMP.sub(b"cch=CCH-PLACEHOLDER;", block)
    return BILLING_PROMPT_ID.sub(b"cc_prompt_id=PROMPT-ID-PLACEHOLDER;", block)


def strip_restored_billing_fields(block: bytes) -> bytes:
    """Drops `cch`, `cc_prev_req`, and `cc_prompt_id` from a `"x-anthropic-billing-header: ...;"` string, field by
    field rather than by masking a value in place, so a baseline that never carried one of them and a live
    capture the relay filled in normalize to the same bytes.
    """
    prefix = b'"x-anthropic-billing-header: '
    if not (block.startswith(prefix) and block.endswith(b'"')):
        return block
    fields = block[len(prefix) : -1].rstrip(b";").split(b"; ")
    kept = [f for f in fields if f.split(b"=", 1)[0] not in RESTORED_BILLING_FIELDS]
    joined = b"; ".join(kept)
    return prefix + joined + (b";" if kept else b"") + b'"'


def normalize_body(raw: bytes) -> bytes:
    """Replace metadata.user_id's device_id and session_id, and the billing block's cc_prompt_id and cch digits.

    Masking the digits is safe only because check_hashes() re-signs every stamped body; on its own it would drop
    the hash from the comparison. Under THROUGH_ROUTER, account_uuid is masked first: the relay splices the
    serving account's identity there, where a native capture's dummy token carries nothing.
    """
    if THROUGH_ROUTER:
        raw = ACCOUNT_UUID.sub(rb"\1ACCOUNT-UUID-PLACEHOLDER\2", raw)
    raw = re.sub(rb'(\\"device_id\\":\\")[^\\]*(\\")', rb"\1DEVICE-ID-PLACEHOLDER\2", raw)
    raw = re.sub(rb'(\\"session_id\\":\\")[^\\]*(\\")', rb"\1SESSION-ID-PLACEHOLDER\2", raw)
    # Every billing-header-shaped string is masked, not only system[0]: a decoy in a tool-use input carries the
    # same per-capture values, and the real block keeps its own hash gate.
    return BILLING_BLOCK.sub(lambda m: mask_billing(m.group(0)), raw)


def has_stamp(body: bytes) -> bool:
    """Whether the first billing block carries a cch=, which is the block phoebis_cch::find_cch signs."""
    block = BILLING_BLOCK.search(body)
    return bool(block and BILLING_STAMP.search(block.group(0)))


def resign_written(path: pathlib.Path) -> None:
    """Store in a written body the cch digits the signer computes over its redacted bytes.

    The stamp is looked for before cargo: a body whose billing block carries none has no digits to follow its
    bytes, and the example's write mode exits 1 on such a file.
    """
    if not has_stamp(path.read_bytes()):
        print(f"resign skipped: {path.name} carries no cch= stamp")
        return
    if shutil.which("cargo") is None:
        raise SystemExit("cargo is needed to re-sign the redacted bodies")
    run = subprocess.run(
        ["cargo", "run", "-q", "-p", "phoebis-cch", "--example", "resign", "--", "--write", str(path)],
        capture_output=True,
        text=True,
    )
    output = (run.stdout + run.stderr).strip()
    if run.returncode != 0:
        raise SystemExit(f"re-signing {path} failed:\n{output}")
    print(output)


def check_hashes(capture_dir: pathlib.Path) -> int:
    """Re-sign every raw inference body that carries a cch= stamp, the gate normalize_body's masking removes.

    The raw bodies are read because the written ones are redacted, and the client signed the bytes it sent. Each
    stamped body leaves a trace line, its SHA-256 beside its native digits, printed and written to
    raw-hash-trace.txt beside the capture: the evidence that the signer matches the client's formula, carrying no
    id of the run.
    """
    cm = json.load(open(capture_dir / "clean_modes.json"))
    stamped = []
    for (idx, _), name in zip(inference_entries(cm), INFERENCE_BODIES):
        path = capture_dir / f"clean_body_{idx}.json"
        if has_stamp(path.read_bytes()):
            stamped.append((name, path))
    if not stamped:
        return 0
    if shutil.which("cargo") is None:
        print("hash check skipped: cargo not found")
        return 2
    status = 0
    trace = []
    for name, path in stamped:
        raw = path.read_bytes()
        stamp = BILLING_STAMP.search(BILLING_BLOCK.search(raw).group(0)).group(0)
        line = f"raw {name} sha256={hashlib.sha256(raw).hexdigest()} cch={stamp[len(b'cch='):-1].decode()}"
        print(line)
        trace.append(line + "\n")
        run = subprocess.run(
            ["cargo", "run", "-q", "-p", "phoebis-cch", "--example", "resign", "--", str(path)],
            capture_output=True,
            text=True,
        )
        if run.returncode != 0:
            print(f"hash drift: {name}")
            for output in (run.stdout + run.stderr).splitlines():
                print(f"  {output}")
            status = 1
    (capture_dir / "raw-hash-trace.txt").write_text("".join(trace))
    return status


def normalize_tree(root: pathlib.Path) -> dict:
    normalized = {}
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        rel = str(path.relative_to(root))
        raw = path.read_bytes()
        if rel.endswith(".hex"):
            raw = mask_hello(bytes.fromhex(raw.decode().strip())).hex().encode() + b"\n"
        elif rel.endswith(".http"):
            raw = normalize_head(raw)
        elif rel.startswith(("body_", "count_")) and rel.endswith(".json"):
            raw = normalize_body(raw)
        normalized[rel] = raw
    return normalized


def restored_report(fresh_dir: pathlib.Path) -> list:
    """One line per inference run naming which of the fields `strip_restored_billing_fields` and `normalize_head`
    ignore the live capture actually carried, so an operator sees the restoration happened rather than reading
    it out of a grade that now passes either way.
    """
    lines = []
    for i, body_name in enumerate(INFERENCE_BODIES):
        block = BILLING_BLOCK.search((fresh_dir / body_name).read_bytes())
        carried = [
            field.decode()
            for field in RESTORED_BILLING_FIELDS
            if block and (field + b"=") in block.group(0)
        ]
        if i < len(INFERENCE_HEADS):
            head_bytes = (fresh_dir / INFERENCE_HEADS[i]).read_bytes()
            if re.search(rb"(?im)^x-client-request-id:", head_bytes):
                carried.append("x-client-request-id")
        named = ", ".join(carried) if carried else "none of the restored fields"
        lines.append(f"run {i} ({body_name}): carries {named}")
    return lines


def body_diff(name: str, committed: bytes, fresh: bytes) -> list:
    """Bounded unified diff of one body, pretty-printed as JSON so a changed value shows as a changed line.

    Lines are left at full width: a system prompt is a single JSON string, and the differing date sits far into
    it, so clipping would hide the very thing the diff is printed for.
    """
    if committed is None or fresh is None:
        return []
    try:
        was = json.dumps(json.loads(committed), indent=2, ensure_ascii=False).splitlines()
        now = json.dumps(json.loads(fresh), indent=2, ensure_ascii=False).splitlines()
    except (UnicodeDecodeError, ValueError):
        return []
    lines = list(difflib.unified_diff(was, now, fromfile=f"committed {name}", tofile=f"fresh {name}", lineterm=""))
    if len(lines) > DIFF_LINES:
        lines = lines[: DIFF_LINES - 1] + [f"... {len(lines) - DIFF_LINES + 1} further diff lines"]
    return lines


def partition_names(committed: dict, fresh: dict, uncaptured_set: set) -> tuple[list, list, list]:
    """Split every fixture name into the ones this capture cannot reach, the ones only the count capture
    writes, and the ones that differ, so an operator reads which capture would fill each missing name."""
    names = sorted(set(committed) | set(fresh))
    uncaptured = [name for name in names if name not in fresh and name in uncaptured_set]
    counts = [name for name in uncaptured if name in COUNT_FIXTURES]
    others = [name for name in uncaptured if name not in COUNT_FIXTURES]
    diffs = [name for name in names if name not in uncaptured and committed.get(name) != fresh.get(name)]
    return others, counts, diffs


def check(capture_dir: pathlib.Path, baseline: pathlib.Path | None = None, skip_hashes: bool = False) -> int:
    with tempfile.TemporaryDirectory() as tmp:
        fresh_dir = pathlib.Path(tmp) / "fixtures"
        build(capture_dir, fresh_dir, resign=False)
        if THROUGH_ROUTER:
            for line in restored_report(fresh_dir):
                print(line)
        hashes = 0 if skip_hashes else check_hashes(capture_dir)
        if baseline is not None:
            # The same two masks that apply to the router capture apply here too, so a live date or a live
            # account_uuid in the baseline's own bodies never shows up as drift against itself.
            baseline_dir = pathlib.Path(tmp) / "baseline"
            build(baseline, baseline_dir, resign=False)
            committed = normalize_tree(baseline_dir)
        else:
            committed = normalize_tree(FIXTURES)
        fresh = normalize_tree(fresh_dir)
        uncaptured_set = (UNCAPTURED_THROUGH_ROUTER if THROUGH_ROUTER else CONTROL_PLANE) | COUNT_FIXTURES
        uncaptured, uncaptured_count, diffs = partition_names(committed, fresh, uncaptured_set)
        if uncaptured:
            label = "not captured (router capture):" if THROUGH_ROUTER else "not captured (control plane):"
            print(label)
            for name in uncaptured:
                print(f"  {name}")
        if uncaptured_count:
            print("not captured (count capture):")
            for name in uncaptured_count:
                print(f"  {name}")
        if diffs:
            print("fixtures differ:")
            for name in diffs:
                print(f"  {name}")
                if name.startswith("body_") and name.endswith(".json"):
                    for line in body_diff(name, committed.get(name), fresh.get(name)):
                        print(f"    {line}")
            return 2 if hashes == 2 else 1
        if hashes:
            return hashes
        print("fixtures match")
        return 0


def check_count(capture_dir: pathlib.Path, committed_dir: pathlib.Path = FIXTURES) -> int:
    """Diff the count fixtures a capture produces against the committed ones. All six are required here: this
    is the only capture that writes them, so one absent is a run that recorded no count, not a plane it could
    not reach."""
    with tempfile.TemporaryDirectory() as tmp:
        fresh_dir = pathlib.Path(tmp) / "fixtures"
        fresh_dir.mkdir(parents=True)
        build_count(capture_dir, fresh_dir)
        committed = {k: v for k, v in normalize_tree(committed_dir).items() if k in COUNT_FIXTURES}
        fresh = {k: v for k, v in normalize_tree(fresh_dir).items() if k in COUNT_FIXTURES}
        missing = sorted(name for name in COUNT_FIXTURES if name not in committed or name not in fresh)
        # Nothing is exempt here, so a name one side lacks lands in the difference list as well; it is already
        # reported above, and reporting it twice would read as two faults.
        _, _, diffs = partition_names(committed, fresh, set())
        diffs = [name for name in diffs if name not in missing]
        if missing:
            print("count fixtures missing:")
            for name in missing:
                print(f"  {name} ({'not captured' if name not in fresh else 'not committed'})")
        if diffs:
            print("count fixtures differ:")
            for name in diffs:
                print(f"  {name}")
                for line in body_diff(name, committed.get(name), fresh.get(name)):
                    print(f"    {line}")
        if missing or diffs:
            return 1
        print("count fixtures match")
        return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "capture_dir", nargs="?", default=str(DEFAULT_CAPTURE), help="capture directory to build fixtures from"
    )
    parser.add_argument(
        "--check", action="store_true", help="diff the capture's fixtures against crates/profile/fixtures instead"
    )
    parser.add_argument(
        "--through-router",
        action="store_true",
        help="the capture was taken at the router's upstream, not at the measured client; grades it accordingly",
    )
    parser.add_argument(
        "--baseline",
        help="grade --check against this capture directory instead of the committed fixtures",
    )
    parser.add_argument(
        "--skip-hash-check",
        action="store_true",
        help="leave the cch re-sign to the caller instead of running it as part of --check",
    )
    parser.add_argument(
        "--count",
        metavar="DIR",
        help="build (or --check) the count_tokens fixtures from the capture directory the count script wrote",
    )
    parser.add_argument("--self-test", action="store_true", help="run mask_hello's bounds-check self-test and exit")
    args = parser.parse_args()
    if args.self_test:
        _self_test()
        return 0
    global THROUGH_ROUTER
    THROUGH_ROUTER = args.through_router
    if args.count:
        count_src = pathlib.Path(args.count).expanduser()
        if args.check:
            return check_count(count_src)
        build_count(count_src, FIXTURES)
        print("count fixtures written:", sorted(COUNT_FIXTURES))
        return 0
    src = pathlib.Path(args.capture_dir).expanduser()
    if args.check:
        baseline = pathlib.Path(args.baseline).expanduser() if args.baseline else None
        return check(src, baseline=baseline, skip_hashes=args.skip_hash_check)
    build(src, FIXTURES)
    print("fixtures written:", sorted(p.name for p in FIXTURES.rglob("*") if p.is_file()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
