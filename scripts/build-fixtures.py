#!/usr/bin/env python3
"""Build crates/profile/fixtures from a capture directory, or --check a capture against them."""
import argparse
import collections
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

# The fixtures only the control plane produces. scripts/capture-control.sh records it and the baseline capture records
# none, so --check lists these names as not captured, rather than as drift, when no directory of control recordings
# is present.
CONTROL_PLANE = {
    "control_profile.http",
    "control_roles.http",
    "control_token.http",
    "control_token_code.http",
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

# The billing block, and the two values inside it a session mints fresh on every run. A new cc_prompt_id changes
# the cch digits with it, so masking the id alone would still leave --check reporting drift with no protocol change.
BILLING_BLOCK = re.compile(rb'"x-anthropic-billing-header:[^"]*"')
BILLING_PROMPT_ID = re.compile(rb"cc_prompt_id=[^;\"]*;")
BILLING_STAMP = re.compile(rb"cch=[0-9a-f]{5};")

# billing_line hands back the decoded line, so its stamp is matched as text.
LINE_STAMP = re.compile(r"cch=[0-9a-f]{5};")

# The field the client writes where its native patch does not run. It claims no hash, and the relay passes such a body
# unsigned (phoebis_cch::carries_only_the_placeholder), so the builder neither re-signs it nor grades it as drift.
UNPATCHED_STAMP = "cch=00000;"

# The billing fields the relay restores on a reduced-shape request; under THROUGH_ROUTER these are dropped
# outright rather than masked, since a baseline that never carried one has no value for a placeholder to replace.
RESTORED_BILLING_FIELDS = (b"cch", b"cc_prev_req", b"cc_prompt_id")

# The relay puts the serving account's identity into the escaped metadata string; a capture taken at the router's
# upstream carries it where the native capture, made with a dummy token, carried nothing.
ACCOUNT_UUID = re.compile(rb'(\\"account_uuid\\":\\")[^\\]*(\\")')

# The values a written fixture carries in place of the run's own ids. Each keeps its field's shape, 64 hex digits or
# a version 4 uuid, so every parser that reads the fields still parses a written fixture. The session header and
# the body's session_id share one placeholder, and so do the prompt id header and the billing block's cc_prompt_id,
# as the client sends one uuid in both, so the relay's reading of header and body stays consistent.
DEVICE_ID_PLACEHOLDER = "0123456789abcdef" * 4
PROMPT_ID_PLACEHOLDER = "33333333-3333-4333-8333-333333333333"
REQUEST_ID_PLACEHOLDER = "22222222-2222-4222-8222-222222222222"
SESSION_ID_PLACEHOLDER = "11111111-1111-4111-8111-111111111111"

# The header values the client mints fresh on every run, masked so --check reports profile drift, not the
# identifiers themselves.
HEADER_PLACEHOLDERS = {
    "x-claude-code-prompt-id": PROMPT_ID_PLACEHOLDER,
    "x-claude-code-session-id": "SESSION-ID-PLACEHOLDER",
    "x-client-request-id": "REQUEST-ID-PLACEHOLDER",
}

# The authorization-code grant's two nonces, which the client mints with randomBytes(32) for every login. A code
# fixture that kept them would report drift on every later capture of the same build. check() writes its fresh side
# through control() as well, so two logins of one build write the same bytes.
CODE_VERIFIER_PLACEHOLDER = "CODE-VERIFIER-PLACEHOLDER"
STATE_PLACEHOLDER = "STATE-PLACEHOLDER"

# The grant's code. The login driver pastes a dummy one, but an operator who completes a login by hand at the terminal
# pastes a real one-time code from a browser login.
CODE_PLACEHOLDER = "CODE-PLACEHOLDER"


def redact(line: str) -> str:
    line = re.sub(r"Bearer sk-ant-oat01-\S+", "Bearer sk-ant-oat01-PLACEHOLDER", line)
    line = re.sub(r"x-api-key: sk-ant-api03-\S+", "x-api-key: sk-ant-api03-PLACEHOLDER", line)
    line = re.sub(r"^Host: 127\.0\.0\.1:\d+$", "Host: api.anthropic.com", line)
    # The client spells the session and request names in different cases, so these match in any case and keep their
    # spelling. The prompt id name matches the same way to follow its neighbors.
    line = re.sub(r"(?i)^(x-claude-code-prompt-id:\s*)\S+$", rf"\g<1>{PROMPT_ID_PLACEHOLDER}", line)
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
    body = re.sub(r'"code":"[^"]*"', f'"code":"{CODE_PLACEHOLDER}"', body)
    body = re.sub(r'"code_verifier":"[^"]*"', f'"code_verifier":"{CODE_VERIFIER_PLACEHOLDER}"', body)
    body = re.sub(r'"state":"[^"]*"', f'"state":"{STATE_PLACEHOLDER}"', body)
    # control_receiver.py records the body's bytes decoded as latin1, so encoding it back the same way restores them.
    payload = body.encode("latin1")
    lines = [entry["request_line"]] + [redact(h) for h in entry["headers"]]
    lines = [l if not l.startswith("Content-Length:") else f"Content-Length: {len(payload)}" for l in lines]
    (out / name).write_bytes(("\r\n".join(lines) + "\r\n\r\n").encode() + payload)


def control_fixtures(control_dir: pathlib.Path, out: pathlib.Path, responses: pathlib.Path) -> list[str]:
    """Write the control fixtures from the control receiver's recordings in `control_dir`, one file per phase, and
    copy the canned responses the receiver answered with from `responses` into `out/responses/`.

    Each fixture is the first request of its kind across the phase files in name order, so a retried refresh grant
    leaves the first one pinned. A grant or read the recording lacks writes nothing, for check() to report as not
    captured. Returns the names of the fixtures written.
    """
    if not control_dir.is_dir():
        raise SystemExit(f"{control_dir} is not a directory of control recordings")
    (out / "responses").mkdir(parents=True, exist_ok=True)
    entries = []
    for path in sorted(control_dir.glob("*.json")):
        entries.extend(json.load(open(path)))
    # A connection that failed before its request was read leaves an error entry with no request line.
    requests = [e for e in entries if "request_line" in e]

    def grant(entry):
        try:
            body = json.loads(entry.get("body", ""))
        except ValueError:
            return None
        return body.get("grant_type") if isinstance(body, dict) else None

    def first(method, path, grant_type):
        for entry in requests:
            parts = entry["request_line"].split(" ")
            if len(parts) < 2 or parts[0] != method or parts[1].split("?", 1)[0] != path:
                continue
            if grant_type is None or grant(entry) == grant_type:
                return entry
        return None

    written = []
    for name, method, path, grant_type in (
        ("control_profile.http", "GET", "/api/oauth/profile", None),
        ("control_roles.http", "GET", "/api/oauth/claude_cli/roles", None),
        ("control_token.http", "POST", "/v1/oauth/token", "refresh_token"),
        ("control_token_code.http", "POST", "/v1/oauth/token", "authorization_code"),
        ("control_usage.http", "GET", "/api/oauth/usage", None),
    ):
        entry = first(method, path, grant_type)
        if entry is not None:
            control(entry, name, out)
            written.append(name)
    # The builder's own write passes the committed responses as the source and the tree holding them as `out`.
    if (out / "responses").resolve() != responses.resolve():
        for name in ("profile.json", "roles.json", "token.json", "usage.json"):
            shutil.copyfile(responses / name, out / "responses" / name)
    return written


def session_id(entry) -> str | None:
    """The value of the entry's x-claude-code-session-id header, or None when it carries none."""
    for line in entry["headers"]:
        name, colon, value = line.partition(":")
        if colon and name.strip().lower() == "x-claude-code-session-id":
            return value.strip()
    return None


def inference_posts(cm: list) -> list:
    """Every inference post of the capture, re-sends included, matched on the path alone: a count request's line
    starts with the inference path as well, and zipping one onto an inference fixture name would mis-pin that
    fixture."""
    return [
        (i, e)
        for i, e in enumerate(cm)
        if e["request_line"].split(" ")[1].split("?")[0] == "/v1/messages" and e["request_line"].startswith("POST ")
    ]


def inference_entries(cm: list) -> list:
    """The first inference post of each session in order of first appearance, and each post without the session
    header on its own.

    The client re-sends a request the capture receiver refused, with features stripped, in the same session. The
    relay of a healthy conversation sees the first post alone, so that post is the one a fixture pins."""
    seen = set()
    kept = []
    for i, e in inference_posts(cm):
        session = session_id(e)
        if session is not None:
            if session in seen:
                continue
            seen.add(session)
        kept.append((i, e))
    return kept


def resent_posts(cm: list) -> list[tuple[int, int]]:
    """For each session holding more than one inference post, in the selector's order, the index of the post
    `inference_entries` keeps and the session's number of inference posts."""
    posts = collections.Counter(session_id(e) for _, e in inference_posts(cm))
    resent = []
    for i, e in inference_entries(cm):
        session = session_id(e)
        if session is not None and posts[session] > 1:
            resent.append((i, posts[session]))
    return resent


def count_entries(cm: list) -> list:
    """The capture's token-count posts, the one request the count script's receiver provokes."""
    return [(i, e) for i, e in enumerate(cm) if e["request_line"].startswith("POST /v1/messages/count_tokens")]


def build(
    src: pathlib.Path,
    out: pathlib.Path,
    resign: bool = True,
    control: pathlib.Path | None = None,
    strict: bool = True,
) -> list[tuple[str, str]]:
    """Write the fixture tree at `out`, sourced from the capture directory `src`.

    The control plane is read from the control receiver's recordings in `control`, or in `src/control/` when
    `control` is None and that directory exists. A capture without either holds the inference plane only, and the
    control fixtures and canned responses are left unwritten for `check()` to report as not captured.

    The bodies are written redacted, and with `resign` their cch digits are re-signed over the redacted bytes.
    `check()` builds with `resign` off, since its comparison masks the digits and `check_hashes` grades the raw
    bodies' own.

    `check()` also builds with `strict` off, so a grade reaches the end of the capture: a missing hello probe or
    inference ClientHello leaves its fixture unwritten. A write to the committed tree keeps `strict` on and stops
    there instead.

    Returns the required fixtures left unwritten, each with its reason, and prints nothing about them, so the caller
    reports them.
    """
    # A fixture write targets the committed tree, so a mistyped directory stops the build before it rewrites the
    # inference plane.
    if control is not None and not control.is_dir():
        raise SystemExit(f"{control} is not a directory of control recordings")
    cm = json.load(open(src / "clean_modes.json"))
    # Selected by request line rather than by index: a capture that interleaves its HEAD /api/hello probes
    # differently still lands the same four bodies on the same four fixtures. Each session contributes its first
    # post alone, since the client re-sends a refused request in the same session.
    posts = inference_entries(cm)
    if len(posts) != len(INFERENCE_BODIES):
        raise SystemExit(f"{src}/clean_modes.json holds {len(posts)} POST /v1/messages sessions, want 4")
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
    hello = next((e for e in cm if e["request_line"].startswith("HEAD /api/hello")), None)
    # In a router capture, connect.json, which tls_raw.py always writes, holds the ClientHello the router itself sent
    # toward its upstream. crates/transport/tests/client_hello.rs grades that shape, so the build never opens the file
    # under THROUGH_ROUTER.
    conn = [] if THROUGH_ROUTER else json.load(open(src / "connect.json"))
    inf = next((h for h in conn if h.get("hello_len") == INFERENCE_HELLO_LEN), None)
    # Both stops come before the first write: a fixture write that stopped after it would leave the committed tree
    # half rewritten.
    if strict and not THROUGH_ROUTER:
        if hello is None:
            raise SystemExit(f"{src}/clean_modes.json holds no HEAD /api/hello entry")
        if inf is None:
            raise SystemExit(f"{src}/connect.json holds no {INFERENCE_HELLO_LEN}-byte ClientHello")
    (out / "responses").mkdir(parents=True, exist_ok=True)
    for body, name in zip(captured, INFERENCE_BODIES):
        (out / name).write_bytes(redact_body(body))
        if resign:
            resign_written(out / name)
    for (_, entry), name, body_name in zip(posts, INFERENCE_HEADS, INFERENCE_BODIES):
        (out / name).write_bytes(head(entry, len((out / body_name).read_bytes())))
    unwritten = []
    # The relay answers HEAD /api/hello itself, so a router capture never carries one; both it and the hex
    # files below are optional in this mode.
    if hello is None:
        if not THROUGH_ROUTER:
            unwritten.append(("hello.http", "the capture holds no HEAD /api/hello"))
    else:
        (out / "hello.http").write_bytes(head(hello))
    if not THROUGH_ROUTER:
        if inf is not None:
            (out / "hello_inference.hex").write_text(inf["raw_hex"] + "\n")
        else:
            lengths = ", ".join(str(n) for n in sorted({h["hello_len"] for h in conn if "hello_len" in h})) or "none"
            unwritten.append(
                (
                    "hello_inference.hex",
                    f"connect.json holds no {INFERENCE_HELLO_LEN}-byte ClientHello (recorded lengths: {lengths})",
                )
            )
        ctl = next((h for h in conn if h.get("hello_len") == CONTROL_HELLO_LEN), None)
        if ctl is not None:
            (out / "hello_control.hex").write_text(ctl["raw_hex"] + "\n")
    if control is None:
        control = src / "control"
        if not control.is_dir():
            return unwritten
    # The control receiver answers from the committed responses, so they are the canned bodies this capture saw.
    control_fixtures(control, out, FIXTURES / "responses")
    return unwritten


def build_count(src: pathlib.Path, out: pathlib.Path, resign: bool = True) -> None:
    """Write count_<mode>.json and count_<mode>.http for each mode directory under `src`, from the one
    count_tokens request each run recorded; the inference entries beside it are the client's turn and are not
    extracted, since the inference fixtures stay pinned to the build that measured them.

    The bodies are written redacted, and with `resign` their cch digits are re-signed over the redacted bytes, as
    `build` does. `check_count` builds with `resign` off, since its comparison masks the digits."""
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
        written = redact_body(body)
        (out / f"{mode}.json").write_bytes(written)
        if resign:
            # The re-sign rewrites the five digits alone, so the length below still holds.
            resign_written(out / f"{mode}.json")
        (out / f"{mode}.http").write_bytes(head(entry, len(written)))


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
    _self_test_resend_selector()
    _self_test_check_reports_resends()
    _self_test_check_reports_probe_and_hello()
    _self_test_build_stops_without_client_hello()
    _self_test_build_reports_no_recorded_length()
    _self_test_capture_partition()
    _self_test_redact_body()
    _self_test_redact_head()
    _self_test_build_redacts()
    _self_test_placeholder()
    _self_test_billing_line_decoy()
    _self_test_resign_written()
    _self_test_check_hashes_placeholder()
    _self_test_control_selection()
    _self_test_build_reads_control()
    _self_test_build_count_redacts()


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


def _self_test_resend_selector() -> None:
    """The client re-sends a refused request within its session, so the inference selector keeps the first post of
    each session and every post without the session header, and matches the header's name in any case."""

    def post(*headers: str, path: str = "/v1/messages?beta=true") -> dict:
        return {"request_line": f"POST {path} HTTP/1.1", "headers": list(headers), "body": ""}

    first = "x-claude-code-session-id: " + _TEST_SESSION_IDS[0].decode()
    other = "x-claude-code-session-id: " + _TEST_SESSION_IDS[1].decode()
    # The count post carries the first session's id as well, and must not add to that session's posts.
    entries = [post(first), post(first), post(first, path="/v1/messages/count_tokens?beta=true"), post(other), post()]
    posts = [i for i, _ in inference_entries(entries)]
    if posts != [0, 3, 4]:
        raise AssertionError(f"the inference selector took entries {posts}, want [0, 3, 4]")
    counts = [i for i, _ in count_entries(entries)]
    if counts != [2]:
        raise AssertionError(f"the count selector took entries {counts}, want [2]")
    if resent_posts(entries) != [(0, 2)]:
        raise AssertionError(f"resent_posts returned {resent_posts(entries)}, want [(0, 2)]")
    print("self-test passed: the inference selector keeps the first post of each session and a post without the header")

    entries = [post(), post()]
    posts = [i for i, _ in inference_entries(entries)]
    if posts != [0, 1]:
        raise AssertionError(f"the inference selector took entries {posts} of two posts without the header")
    if resent_posts(entries) != []:
        raise AssertionError(f"resent_posts returned {resent_posts(entries)} for posts without the header, want []")
    print("self-test passed: the inference selector keeps every post without the session header")

    value = _TEST_SESSION_IDS[0].decode()
    entries = [post("X-Claude-Code-Session-Id: " + value), post("x-claude-code-session-id: " + value)]
    posts = [i for i, _ in inference_entries(entries)]
    if posts != [0]:
        raise AssertionError(f"the inference selector took entries {posts} of one session spelled two ways")
    if resent_posts(entries) != [(0, 2)]:
        raise AssertionError(f"resent_posts returned {resent_posts(entries)} for one session spelled two ways")
    print("self-test passed: the inference selector matches the session header's name in any case")


def _count_capture(root: pathlib.Path, session: bytes, members: bytes = b"") -> pathlib.Path:
    """A capture directory shaped like the one the count script writes: one inference entry and one count entry
    per mode, the count body carrying the escaped `members` given and a per-run session id for the normalizer to
    mask."""
    content = b"{" + members + rb'\"session_id\":\"' + session + rb'\"}'
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
        written = (tree / "count_oauth.json").read_bytes()
        stale = written.replace(SESSION_ID_PLACEHOLDER.encode(), b"b2b1f4ca-1f6e-4a52-9d54-2f9b4a6c0e11")
        (tree / "count_oauth.json").write_bytes(stale)
        if check_count(capture, tree) != 0:
            raise AssertionError("check_count reported drift against the fixtures it just built")
        (tree / "count_apikey.http").unlink()
        report = io.StringIO()
        with contextlib.redirect_stdout(report):
            status = check_count(capture, tree)
        if status == 0 or "count_apikey.http" not in report.getvalue():
            raise AssertionError(f"check_count passed with a count fixture missing: {report.getvalue()!r}")
        # The comparison masks the digits, so the fresh side is built without the re-sign step, as check() builds its.
        if "resign" in report.getvalue():
            raise AssertionError(f"check_count re-signed its fresh side: {report.getvalue()!r}")
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

# The capture's four runs are four sessions, and the inference selector keeps one post of each. Each id is the test
# id with its last digit replaced by the run's number, so it keeps the uuid shape.
_TEST_SESSION_IDS = tuple(_TEST_SESSION_ID[:-1] + str(run).encode() for run in range(1, len(INFERENCE_BODIES) + 1))


def _identified_body(model: bytes, session: bytes = _TEST_SESSION_ID) -> bytes:
    """A body in the client's shape whose metadata.user_id and billing block carry the invented ids, `session` as
    its session_id, with a nonempty account_uuid as a capture at the router's upstream carries."""
    user_id = (
        rb'{\"device_id\":\"' + _TEST_DEVICE_ID + rb'\",\"account_uuid\":\"' + _TEST_ACCOUNT_UUID
        + rb'\",\"session_id\":\"' + session + rb'\"}'
    )
    return (
        rb'{"model":"' + model + rb'","system":[{"type":"text","text":"x-anthropic-billing-header: '
        + b"cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cch=4f1c2; cc_prompt_id=" + _TEST_PROMPT_ID
        + rb';"}],"metadata":{"user_id":"' + user_id + rb'"}}'
    )


def _inference_capture(root: pathlib.Path) -> pathlib.Path:
    """A capture directory shaped like the one the baseline script writes: the hello probe, then the four
    inference posts, each head and body carrying the invented ids and its run's own session id."""
    root.mkdir(parents=True)
    entries = [{"request_line": "HEAD /api/hello HTTP/1.1", "headers": ["Host: 127.0.0.1:48442"], "body": ""}]
    for i, name in enumerate(INFERENCE_BODIES, start=1):
        session = _TEST_SESSION_IDS[i - 1]
        body = _identified_body(name.encode(), session)
        entries.append(
            {
                "request_line": "POST /v1/messages?beta=true HTTP/1.1",
                "headers": [
                    "Authorization: Bearer sk-ant-oat01-DUMMYOAUTH",
                    "X-Claude-Code-Session-Id: " + session.decode(),
                    "x-claude-code-prompt-id: " + _TEST_PROMPT_ID.decode(),
                    "x-client-request-id: " + _TEST_REQUEST_ID.decode(),
                    "Host: 127.0.0.1:48442",
                    f"Content-Length: {len(body)}",
                ],
                "body": body.decode(),
            }
        )
        (root / f"clean_body_{i}.json").write_bytes(body)
    (root / "clean_modes.json").write_text(json.dumps(entries))
    hello = {"hello_len": INFERENCE_HELLO_LEN, "raw_hex": _client_hello().hex()}
    (root / "connect.json").write_text(json.dumps([hello]))
    return root


def _client_hello() -> bytes:
    """A minimal ClientHello that mask_hello reads to its end: no session id, one cipher suite, no extensions.
    check() masks every written hello, so a synthetic capture it grades has to carry one that parses."""
    body = b"\x03\x03" + b"\x00" * 32 + b"\x00" + b"\x00\x02\x13\x01" + b"\x01\x00" + b"\x00\x00"
    handshake = b"\x01" + struct.pack("!I", len(body))[1:] + body
    return b"\x16\x03\x01" + struct.pack("!H", len(handshake)) + handshake


def _written_files(out: pathlib.Path) -> list[str]:
    """The files a build left under `out`, by their path relative to it."""
    return sorted(p.relative_to(out).as_posix() for p in out.rglob("*") if p.is_file())


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
    """A written head carries the prompt, session and request placeholders, and the credentials and the Host line
    redact as before."""
    cases = {
        "Authorization: Bearer sk-ant-oat01-DUMMYOAUTH": "Authorization: Bearer sk-ant-oat01-PLACEHOLDER",
        "Host: 127.0.0.1:48442": "Host: api.anthropic.com",
        "x-api-key: sk-ant-api03-capture-dummy": "x-api-key: sk-ant-api03-PLACEHOLDER",
        "x-claude-code-prompt-id: " + _TEST_PROMPT_ID.decode(): f"x-claude-code-prompt-id: {PROMPT_ID_PLACEHOLDER}",
        "x-client-request-id: " + _TEST_REQUEST_ID.decode(): f"x-client-request-id: {REQUEST_ID_PLACEHOLDER}",
        "X-Stainless-Lang: js": "X-Stainless-Lang: js",
    }
    for session in _TEST_SESSION_IDS:
        cases["X-Claude-Code-Session-Id: " + session.decode()] = f"X-Claude-Code-Session-Id: {SESSION_ID_PLACEHOLDER}"
    for line, wanted in cases.items():
        if redact(line) != wanted:
            raise AssertionError(f"redact({line!r}) gave {redact(line)!r}, want {wanted!r}")
    print("self-test passed: redact writes the prompt, session and request placeholders into a head")


def _self_test_build_redacts() -> None:
    """In both grading modes every written body is free of the capture's ids, and every inference head carries
    the three placeholders and declares the length of the redacted body it travels with."""
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
                leaked = [original for original in _TEST_BODY_IDS + _TEST_SESSION_IDS if original in body]
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
                    f"x-claude-code-prompt-id: {PROMPT_ID_PLACEHOLDER}",
                    f"X-Claude-Code-Session-Id: {SESSION_ID_PLACEHOLDER}",
                    f"x-client-request-id: {REQUEST_ID_PLACEHOLDER}",
                ):
                    if f"\r\n{line}\r\n" not in text:
                        raise AssertionError(f"the {mode} build wrote {path.name} without {line!r}")
                for original in (_TEST_PROMPT_ID, _TEST_REQUEST_ID, *_TEST_SESSION_IDS):
                    if original.decode() in text:
                        raise AssertionError(f"the {mode} build kept {original!r} in {path.name}")
            print(f"self-test passed: the {mode} build writes redacted bodies and heads")


def _self_test_placeholder() -> None:
    """The unpatched placeholder is told from a stamp, from a body without one, and from a line that claims a hash in a
    second cch= field, an escaped one included, as phoebis_cch::carries_only_the_placeholder tells them."""
    cases = (
        (b"cc_version=2.1.269.517; cch=00000;", True),
        (b"cc_version=2.1.269.517; cch=4f1c2;", False),
        (b"cc_version=2.1.269.517;", False),
        (b"cc_version=2.1.269.517; cch=00000; cch=4f1c2;", False),
        # The second field's first letter spelled as a JSON unicode escape, which the raw bytes do not show as cch=.
        (b"cc_version=2.1.269.517; cch=00000; " + bytes([0x5C]) + b"u0063ch=4f1c2;", False),
    )
    for line, wanted in cases:
        body = b'{"system":[{"type":"text","text":"x-anthropic-billing-header: ' + line + b'"}],"messages":[]}'
        if carries_only_the_placeholder(body) != wanted:
            raise AssertionError(f"carries_only_the_placeholder read {line!r} as {not wanted}")
    print("self-test passed: the unpatched placeholder is told from a stamp and from a line claiming a hash")


def _self_test_billing_line_decoy() -> None:
    """A billing header in a tool-use input, which the client serializes ahead of the top-level system array, leaves
    the stamp to system[0].text, the line phoebis_cch::find_cch reads."""
    header = b"x-anthropic-billing-header: cc_version=2.1.269.517; "

    def body(decoy: bytes, real: bytes) -> bytes:
        tool_use = (
            b'{"type":"tool_use","id":"toolu_1","name":"echo","input":{"system":[{"type":"text","text":"'
            + header + decoy + b'"}]}}'
        )
        return (
            b'{"messages":[{"role":"assistant","content":[' + tool_use + b']}],'
            b'"system":[{"type":"text","text":"' + header + real + b'"}]}'
        )

    stamped = body(b"cch=00000;", b"cch=4f1c2;")
    if not has_stamp(stamped) or carries_only_the_placeholder(stamped):
        raise AssertionError("a placeholder decoy ahead of the stamped line hid the stamp")
    if not carries_only_the_placeholder(body(b"cch=4f1c2;", b"cch=00000;")):
        raise AssertionError("a stamped decoy ahead of the placeholder line hid the placeholder")
    print("self-test passed: a billing header in a tool-use input leaves the stamp to system[0].text")


def _self_test_resign_written() -> None:
    """A body without a stamp and a body carrying only the unpatched placeholder are left alone before cargo is
    looked for, and a stamped body needs cargo."""
    with tempfile.TemporaryDirectory() as tmp:
        stampless = pathlib.Path(tmp) / "body_apikey.json"
        stampless.write_bytes(
            b'{"system":[{"type":"text","text":'
            b'"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli;"}]}'
        )
        stamped = pathlib.Path(tmp) / "body_oauth.json"
        stamped.write_bytes(_identified_body(b"claude-sonnet-4-5"))
        placeholder = pathlib.Path(tmp) / "body_fp_9b244.json"
        placeholder.write_bytes(_identified_body(b"claude-sonnet-4-5").replace(b"cch=4f1c2;", b"cch=00000;"))
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
            before = placeholder.read_bytes()
            report = io.StringIO()
            with contextlib.redirect_stdout(report):
                resign_written(placeholder)
            if placeholder.read_bytes() != before:
                raise AssertionError("resign_written changed a body carrying the unpatched placeholder")
            if report.getvalue() != "resign skipped: body_fp_9b244.json carries the unpatched placeholder cch=00000\n":
                raise AssertionError(f"resign_written skipped the placeholder with the note {report.getvalue()!r}")
            print("self-test passed: resign_written leaves a body carrying the unpatched placeholder alone and says so")
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


def _self_test_check_hashes_placeholder() -> None:
    """A raw body carrying only the unpatched placeholder leaves a placeholder line with its SHA-256 in the output and
    in the trace file, counts as neither drift nor a re-signed body, and needs no cargo."""
    with tempfile.TemporaryDirectory() as tmp:
        capture = _inference_capture(pathlib.Path(tmp) / "capture")
        placeholder = _identified_body(b"claude-sonnet-4-5").replace(b"cch=4f1c2;", b"cch=00000;")
        (capture / "clean_body_1.json").write_bytes(placeholder)
        stampless = b'{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517;"}]}'
        for i in range(2, len(INFERENCE_BODIES) + 1):
            (capture / f"clean_body_{i}.json").write_bytes(stampless)
        no_cargo = pathlib.Path(tmp) / "bin"
        no_cargo.mkdir()
        saved_path = os.environ.get("PATH")
        os.environ["PATH"] = str(no_cargo)
        try:
            report = io.StringIO()
            with contextlib.redirect_stdout(report):
                status = check_hashes(capture)
        finally:
            if saved_path is None:
                del os.environ["PATH"]
            else:
                os.environ["PATH"] = saved_path
        line = f"placeholder body_oauth.json sha256={hashlib.sha256(placeholder).hexdigest()}\n"
        if status != 0:
            raise AssertionError(f"check_hashes graded the placeholder with status {status}: {report.getvalue()!r}")
        if report.getvalue() != line:
            raise AssertionError(f"check_hashes printed {report.getvalue()!r}, want {line!r}")
        trace = capture / "raw-hash-trace.txt"
        if not trace.exists() or trace.read_text() != line:
            raise AssertionError(f"check_hashes wrote no placeholder line to {trace.name}")
        print("self-test passed: check_hashes traces the unpatched placeholder without grading it")


def _self_test_check_reports_resends() -> None:
    """A capture whose first run the client re-sent grades with status 0, and the report names the run's fixture and
    its post count."""
    with tempfile.TemporaryDirectory() as tmp:
        capture = _inference_capture(pathlib.Path(tmp) / "capture")
        entries = json.loads((capture / "clean_modes.json").read_text())
        entries.append(entries[1])
        (capture / f"clean_body_{len(entries) - 1}.json").write_bytes((capture / "clean_body_1.json").read_bytes())
        (capture / "clean_modes.json").write_text(json.dumps(entries))
        report = io.StringIO()
        with contextlib.redirect_stdout(report):
            status = check(capture, baseline=capture, skip_hashes=True)
        if status != 0:
            raise AssertionError(f"check graded a capture holding a re-sent run with status {status}")
        wanted = "re-sent after the receiver's answer:\n  body_oauth.json: 2 posts, the first kept\n"
        if wanted not in report.getvalue():
            raise AssertionError(f"check did not report the re-sent run: {report.getvalue()!r}")
        print("self-test passed: check grades a capture holding a re-sent run with status 0 and reports the re-send")


def _self_test_check_reports_probe_and_hello() -> None:
    """A capture without the hello probe and without the inference ClientHello grades to its end in check mode, the
    two missing fixtures named, while the fixture write stops on the missing probe before it writes a file."""
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        first = _inference_capture(root / "first")
        second = _inference_capture(root / "second")
        entries = json.loads((second / "clean_modes.json").read_text())[1:]
        (second / "clean_modes.json").write_text(json.dumps(entries))
        # The probe was entry 0, so each post moves up one index and its body has to follow it.
        for i in range(1, len(entries) + 1):
            (second / f"clean_body_{i}.json").rename(second / f"clean_body_{i - 1}.json")
        (second / "connect.json").write_text(json.dumps([{"hello_len": INFERENCE_HELLO_LEN + 4, "raw_hex": "16"}]))
        report = io.StringIO()
        with contextlib.redirect_stdout(report):
            status = check(second, baseline=first, skip_hashes=True)
        output = report.getvalue()
        if status != 1:
            raise AssertionError(f"check graded a capture without the probe and the ClientHello with status {status}")
        for line in (
            "hello.http: the capture holds no HEAD /api/hello",
            f"hello_inference.hex: connect.json holds no {INFERENCE_HELLO_LEN}-byte ClientHello "
            f"(recorded lengths: {INFERENCE_HELLO_LEN + 4})",
        ):
            if line not in output.splitlines():
                raise AssertionError(f"check printed no {line!r}: {output!r}")
        listed = "fixtures differ:\n  hello.http\n  hello_inference.hex\n"
        if not output.endswith(listed + "fixtures not captured: hello.http, hello_inference.hex\n"):
            raise AssertionError(f"check listed other fixtures than the probe and the ClientHello: {output!r}")
        print("self-test passed: check reports a capture without the hello probe and the inference ClientHello")
        # A baseline that lacks them too leaves the same two fixtures unwritten on both sides, where the comparison
        # finds no difference.
        report = io.StringIO()
        with contextlib.redirect_stdout(report):
            status = check(second, baseline=second, skip_hashes=True)
        output = report.getvalue()
        if status != 1:
            raise AssertionError(
                f"check graded two captures without the probe and the ClientHello with status {status}"
            )
        for line in (
            "baseline hello.http: the capture holds no HEAD /api/hello",
            f"baseline hello_inference.hex: connect.json holds no {INFERENCE_HELLO_LEN}-byte ClientHello "
            f"(recorded lengths: {INFERENCE_HELLO_LEN + 4})",
            "fixtures not captured: hello.http, hello_inference.hex",
        ):
            if line not in output.splitlines():
                raise AssertionError(f"check printed no {line!r}: {output!r}")
        if "fixtures match" in output:
            raise AssertionError(f"check printed fixtures match for two incomplete captures: {output!r}")
        print("self-test passed: check grades two captures without the probe and the ClientHello with status 1")
        try:
            build(second, root / "strict", resign=False)
        except SystemExit as e:
            if "HEAD /api/hello" not in str(e):
                raise AssertionError(f"the build refused for the wrong reason: {e}") from e
            written = _written_files(root / "strict")
            if written:
                raise AssertionError(f"the build wrote {written} before it stopped on the missing probe")
            print("self-test passed:", e)
        else:
            raise AssertionError("the build wrote fixtures from a capture without the hello probe")


def _self_test_build_stops_without_client_hello() -> None:
    """A fixture write from a capture that keeps the hello probe but holds no inference ClientHello stops before it
    writes a file."""
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        capture = _inference_capture(root / "capture")
        (capture / "connect.json").write_text(json.dumps([{"hello_len": INFERENCE_HELLO_LEN + 4, "raw_hex": "16"}]))
        try:
            build(capture, root / "strict", resign=False)
        except SystemExit as e:
            if f"{INFERENCE_HELLO_LEN}-byte ClientHello" not in str(e):
                raise AssertionError(f"the build refused for the wrong reason: {e}") from e
            written = _written_files(root / "strict")
            if written:
                raise AssertionError(f"the build wrote {written} before it stopped on the missing ClientHello")
            print("self-test passed: build stops on a capture without the inference ClientHello before it writes")
        else:
            raise AssertionError("the build wrote fixtures from a capture without the inference ClientHello")


def _self_test_build_reports_no_recorded_length() -> None:
    """A non-strict build over a connect.json whose records carry no length, here a plain-http connection's error
    entry in hello_lib.parse_hello's shape, returns none as the recorded lengths and prints nothing itself."""
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        capture = _inference_capture(root / "capture")
        (capture / "connect.json").write_text(json.dumps([{"error": "not-tls", "head": b"POST /v1/message".hex()}]))
        report = io.StringIO()
        with contextlib.redirect_stdout(report):
            unwritten = build(capture, root / "fresh", resign=False, strict=False)
        wanted = (
            "hello_inference.hex",
            f"connect.json holds no {INFERENCE_HELLO_LEN}-byte ClientHello (recorded lengths: none)",
        )
        if unwritten != [wanted]:
            raise AssertionError(f"build returned {unwritten!r}, want [{wanted!r}]")
        if report.getvalue():
            raise AssertionError(f"build printed {report.getvalue()!r} about the fixture it left unwritten")
        print("self-test passed: a connect.json without a recorded length reports none as the lengths")


def _control_entry(seq: int, request_line: str, body: str = "") -> dict:
    """A request entry in the layout scripts/capture/control_receiver.py records."""
    host = "platform.claude.com" if " /v1/oauth/" in request_line else "api.anthropic.com"
    headers = ["Accept: application/json, text/plain, */*", f"Host: {host}"]
    if body:
        headers += ["Content-Type: application/json", f"Content-Length: {len(body)}"]
    return {
        "seq": seq,
        "target": f"{host}:443",
        "request_line": request_line,
        "headers": headers,
        "body": body,
        "answered": 200,
    }


def _code_grant(state: str, verifier: str) -> str:
    """An authorization-code grant body carrying the two nonces given."""
    members = {"grant_type": "authorization_code", "code": "DUMMYCODE", "state": state, "code_verifier": verifier}
    return json.dumps(members, separators=(",", ":"))


def _self_test_control_selection() -> None:
    """The first request of each kind becomes its fixture, a grant the recording lacks writes nothing, the code
    grant's nonces leave no trace in its fixture, and the canned responses land beside the fixtures."""
    refresh = '{"grant_type":"refresh_token","refresh_token":"sk-ant-ort01-X"}'
    retry = '{"grant_type":"refresh_token","refresh_token":"sk-ant-ort01-Y","scope":"user:inference"}'
    first_nonces = ("A" * 43, "B" * 43)
    other_nonces = ("C" * 43, "D" * 43)
    responses_names = ("profile.json", "roles.json", "token.json", "usage.json")
    all_names = [
        "control_profile.http",
        "control_roles.http",
        "control_token.http",
        "control_token_code.http",
        "control_usage.http",
    ]

    def recording(root: pathlib.Path, nonces: tuple | None) -> pathlib.Path:
        """Two phase files in the receiver's layout, opening with a failed connection's error entry, the code
        grant left out when `nonces` is None."""
        root.mkdir()
        first = [
            {"seq": 0, "target": "?", "error": "ConnectionError('closed after 0 bytes of an unfinished head')"},
            _control_entry(1, "POST /v1/oauth/token HTTP/1.1", refresh),
            _control_entry(2, "GET /api/oauth/profile HTTP/1.1"),
        ]
        second = [
            _control_entry(3, "POST /v1/oauth/token HTTP/1.1", retry),
            _control_entry(4, "GET /api/oauth/claude_cli/roles HTTP/1.1"),
        ]
        if nonces is not None:
            second.append(_control_entry(5, "POST /v1/oauth/token HTTP/1.1", _code_grant(*nonces)))
        second.append(_control_entry(6, "GET /api/oauth/usage?x=1 HTTP/1.1"))
        # Written in reverse, so the order read is the names' rather than the order the files were made in. The retry
        # sits in the second file, so a read in any other order pins it in place of the first refresh grant.
        (root / "phase-2.json").write_text(json.dumps(second))
        (root / "phase-1.json").write_text(json.dumps(first))
        return root

    def split(path: pathlib.Path) -> tuple[list, bytes]:
        head_bytes, _, body = path.read_bytes().partition(b"\r\n\r\n")
        return head_bytes.decode().split("\r\n"), body

    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        responses = root / "responses"
        responses.mkdir()
        for name in responses_names:
            (responses / name).write_text(json.dumps({"canned": name}) + "\n")

        full = root / "full"
        names = control_fixtures(recording(root / "control-full", first_nonces), full, responses)
        if sorted(names) != all_names:
            raise AssertionError(f"control_fixtures returned {names}, want {all_names}")
        written = sorted(p.name for p in full.glob("control_*.http"))
        if written != all_names:
            raise AssertionError(f"control_fixtures wrote {written}, want {all_names}")
        _, body = split(full / "control_token.http")
        if body != b'{"grant_type":"refresh_token","refresh_token":"sk-ant-ort01-PLACEHOLDER"}':
            raise AssertionError(f"the token fixture carries {body!r}, want the first refresh grant redacted")
        lines, body = split(full / "control_token_code.http")
        for wanted in (
            b'"grant_type":"authorization_code"',
            b'"code":"' + CODE_PLACEHOLDER.encode() + b'"',
            b'"code_verifier":"' + CODE_VERIFIER_PLACEHOLDER.encode() + b'"',
            b'"state":"' + STATE_PLACEHOLDER.encode() + b'"',
        ):
            if wanted not in body:
                raise AssertionError(f"the code fixture carries no {wanted!r}: {body!r}")
        for value in first_nonces + ("DUMMYCODE",):
            if value.encode() in body:
                raise AssertionError(f"the code fixture kept the grant's {value!r}")
        if f"Content-Length: {len(body)}" not in lines:
            raise AssertionError(f"the code fixture's head declares no Content-Length {len(body)}: {lines!r}")
        usage_line = split(full / "control_usage.http")[0][0]
        if usage_line != "GET /api/oauth/usage?x=1 HTTP/1.1":
            raise AssertionError(f"the usage fixture opens with {usage_line!r}, want its query kept")
        for name in responses_names:
            if (full / "responses" / name).read_bytes() != (responses / name).read_bytes():
                raise AssertionError(f"control_fixtures did not copy responses/{name}")
        print("self-test passed: control_fixtures takes the first request of each kind and copies the responses")

        # The builder's own write reads the canned responses from the tree it writes to.
        control_fixtures(root / "control-full", full, full / "responses")
        for name in responses_names:
            if (full / "responses" / name).read_bytes() != (responses / name).read_bytes():
                raise AssertionError(f"control_fixtures changed responses/{name} copying it onto itself")
        print("self-test passed: control_fixtures leaves the responses alone when source and target are one")

        partial = root / "partial"
        names = control_fixtures(recording(root / "control-partial", None), partial, responses)
        want = [name for name in all_names if name != "control_token_code.http"]
        if sorted(names) != want or (partial / "control_token_code.http").exists():
            raise AssertionError(f"a recording without a code grant wrote {names}, want {want}")
        print("self-test passed: a recording without a code grant writes the four other fixtures")

        again = root / "again"
        control_fixtures(recording(root / "control-again", other_nonces), again, responses)
        if (again / "control_token_code.http").read_bytes() != (full / "control_token_code.http").read_bytes():
            raise AssertionError("two code grants differing in their nonces alone wrote different fixtures")
        print("self-test passed: two code grants differing in their nonces alone write the same fixture")

        reordered = {"code_verifier": other_nonces[1], "grant_type": "authorization_code", "state": other_nonces[0]}
        entry = _control_entry(7, "POST /v1/oauth/token HTTP/1.1", json.dumps(reordered, separators=(",", ":")))
        control(entry, "reordered.http", root)
        body = (root / "reordered.http").read_bytes()
        if any(nonce.encode() in body for nonce in other_nonces):
            raise AssertionError(f"control kept a nonce of a grant in another member order: {body!r}")
        print("self-test passed: control replaces the nonces in any member order")

        # The receiver records a body's bytes decoded as latin1, so a byte above 0x7f has to reach the fixture as the
        # same byte, and Content-Length has to count the bytes written.
        wire = b'{"grant_type":"refresh_token","name":"caf\xc3\xa9"}'
        control(_control_entry(8, "POST /v1/oauth/token HTTP/1.1", wire.decode("latin1")), "latin1.http", root)
        lines, body = split(root / "latin1.http")
        if body != wire or f"Content-Length: {len(wire)}" not in lines:
            raise AssertionError(f"control wrote {body!r} under {lines!r}, want the recorded bytes and their length")
        print("self-test passed: control writes the recorded bytes and declares their length")


def _self_test_build_reads_control() -> None:
    """build reads the control plane from the capture's control/ directory, or from the directory `control`
    names in its place, copies the committed canned responses beside it, and refuses a `control` that is not a
    directory."""
    refresh = '{"grant_type":"refresh_token","refresh_token":"sk-ant-ort01-X"}'
    token = _control_entry(0, "POST /v1/oauth/token HTTP/1.1", refresh)
    profile = _control_entry(1, "GET /api/oauth/profile HTTP/1.1")
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        src = _inference_capture(root / "capture")
        (src / "control").mkdir()
        (src / "control" / "refresh.json").write_text(json.dumps([token, profile]))
        named = root / "named"
        named.mkdir()
        (named / "refresh.json").write_text(json.dumps([token]))

        build(src, root / "own", resign=False)
        own = sorted(p.name for p in (root / "own").glob("control_*.http"))
        if own != ["control_profile.http", "control_token.http"]:
            raise AssertionError(f"the build of a capture with control/ wrote {own}")
        for name in ("profile.json", "roles.json", "token.json", "usage.json"):
            if (root / "own" / "responses" / name).read_bytes() != (FIXTURES / "responses" / name).read_bytes():
                raise AssertionError(f"the build wrote a responses/{name} other than the committed one")
        print("self-test passed: build reads the capture's control/ directory and copies the committed responses")

        build(src, root / "other", resign=False, control=named)
        other = sorted(p.name for p in (root / "other").glob("control_*.http"))
        if other != ["control_token.http"]:
            raise AssertionError(f"the build given another control directory wrote {other}")
        print("self-test passed: build reads the control directory it is given in place of the capture's")

        # The write targets the committed tree, so a mistyped --control must stop the build before its first write.
        try:
            build(src, root / "missing", resign=False, control=root / "absent")
        except SystemExit as e:
            if str(root / "absent") not in str(e):
                raise AssertionError(f"the build refused for the wrong reason: {e}") from e
            if (root / "missing").exists():
                written = sorted(p.name for p in (root / "missing").rglob("*"))
                raise AssertionError(f"the build wrote {written} before it refused the control directory")
            print("self-test passed: build refuses a control directory that does not exist before it writes")
        else:
            raise AssertionError("the build went on with a control directory that does not exist")


def _self_test_build_count_redacts() -> None:
    """A written count body carries the placeholders in place of the capture's ids, and its head declares the
    length of the body written. Each body passes through the re-sign step, which leaves one without a stamp as the
    redaction wrote it and needs no cargo for it."""
    # The emptied account_uuid shortens the body, so a head sized from the captured body would declare too much.
    members = rb'\"device_id\":\"' + _TEST_DEVICE_ID + rb'\",\"account_uuid\":\"' + _TEST_ACCOUNT_UUID + rb'\",'
    with tempfile.TemporaryDirectory() as tmp:
        capture = _count_capture(pathlib.Path(tmp) / "capture", _TEST_SESSION_ID, members)
        out = pathlib.Path(tmp) / "out"
        out.mkdir()
        no_cargo = pathlib.Path(tmp) / "bin"
        no_cargo.mkdir()
        saved_path = os.environ.get("PATH")
        os.environ["PATH"] = str(no_cargo)
        report = io.StringIO()
        try:
            with contextlib.redirect_stdout(report):
                build_count(capture, out)
        finally:
            if saved_path is None:
                del os.environ["PATH"]
            else:
                os.environ["PATH"] = saved_path
        for mode in COUNT_MODES:
            body = (out / f"{mode}.json").read_bytes()
            if body != redact_body((capture / mode / "clean_body_1.json").read_bytes()):
                raise AssertionError(f"build_count wrote a {mode}.json other than its redacted capture")
            if f"resign skipped: {mode}.json carries no cch= stamp\n" not in report.getvalue():
                raise AssertionError(f"build_count wrote {mode}.json past the re-sign step: {report.getvalue()!r}")
            for wanted in (
                rb'\"device_id\":\"' + DEVICE_ID_PLACEHOLDER.encode() + rb'\"',
                rb'\"session_id\":\"' + SESSION_ID_PLACEHOLDER.encode() + rb'\"',
            ):
                if wanted not in body:
                    raise AssertionError(f"build_count wrote {mode}.json without {wanted!r}")
            for original in (_TEST_ACCOUNT_UUID, _TEST_DEVICE_ID, _TEST_SESSION_ID):
                if original in body:
                    raise AssertionError(f"build_count kept {original!r} in {mode}.json")
            lines = (out / f"{mode}.http").read_bytes().decode().split("\r\n")
            if f"Content-Length: {len(body)}" not in lines:
                raise AssertionError(f"build_count wrote {mode}.http without Content-Length {len(body)}")
        print("self-test passed: build_count writes redacted bodies and heads sized to them")
        print("self-test passed: build_count passes each count body through the re-sign step")


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
    first_id = b"44444444-4444-4444-8444-444444444444"
    second_id = b"55555555-5555-4555-8555-555555555555"
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
        b' cc_prompt_id=44444444-4444-4444-8444-444444444444;"}]}'
    )
    reduced_head = b"POST /v1/messages HTTP/1.1\r\nx-app: cli\r\n\r\n"
    restored_head = (
        b"POST /v1/messages HTTP/1.1\r\nx-app: cli\r\n"
        b"x-client-request-id: " + _TEST_REQUEST_ID + b"\r\n\r\n"
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

    prompt_head = b"POST /v1/messages HTTP/1.1\r\nx-claude-code-prompt-id: " + _TEST_PROMPT_ID + b"\r\n\r\n"
    masked_prompt = normalize_head(prompt_head)
    wanted = f"\r\nx-claude-code-prompt-id: {PROMPT_ID_PLACEHOLDER}\r\n".encode()
    if _TEST_PROMPT_ID in masked_prompt or wanted not in masked_prompt:
        raise AssertionError(f"normalize_head gave {masked_prompt!r} for a head carrying a raw prompt id")
    print("self-test passed: normalize_head masks x-claude-code-prompt-id")


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


def billing_line(body: bytes) -> str | None:
    """The decoded top-level system[0].text when it is a billing header, the one line phoebis_cch::find_cch reads, or
    None. A tool-use input can carry a billing header of its own, and the client serializes messages ahead of system,
    so a search of the bytes finds that one first. A body that does not parse carries no billing line."""
    try:
        parsed = json.loads(body)
    except (RecursionError, ValueError):
        return None
    system = parsed.get("system") if isinstance(parsed, dict) else None
    first = system[0] if isinstance(system, list) and system else None
    text = first.get("text") if isinstance(first, dict) else None
    if isinstance(text, str) and text.startswith("x-anthropic-billing-header:"):
        return text
    return None


def has_stamp(body: bytes) -> bool:
    """Whether the billing line carries a cch= with five hex digits and its semicolon, the field
    phoebis_cch::find_cch signs."""
    line = billing_line(body)
    return line is not None and LINE_STAMP.search(line) is not None


def carries_only_the_placeholder(body: bytes) -> bool:
    """Whether the billing line's one and only cch= field is the unpatched placeholder, read the way
    phoebis_cch::carries_only_the_placeholder reads it."""
    line = billing_line(body)
    stamp = LINE_STAMP.search(line) if line is not None else None
    if stamp is None or stamp.group(0) != UNPATCHED_STAMP:
        return False
    # Beside a second cch=, the line claims a hash after all. Counted over the decoded line, so an escaped spelling
    # counts as the server reads it.
    return line.count("cch=") == 1


def resign_written(path: pathlib.Path) -> None:
    """Store in a written body the cch digits the signer computes over its redacted bytes.

    The stamp is looked for before cargo. A body whose billing block carries none has no digits to follow its bytes,
    and the example's write mode exits 1 on such a file. A body carrying only the unpatched placeholder claims no
    hash, and signing it would store digits the client never sent.
    """
    raw = path.read_bytes()
    if carries_only_the_placeholder(raw):
        print(f"resign skipped: {path.name} carries the unpatched placeholder cch=00000")
        return
    if not has_stamp(raw):
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
    id of the run. A body carrying only the unpatched placeholder claims no hash and is not graded. It leaves a
    placeholder line with its SHA-256 instead, since normalize_body masks the digits and a move between a stamp and
    the placeholder shows nowhere else.
    """
    cm = json.load(open(capture_dir / "clean_modes.json"))
    stamped = []
    trace = []
    for (idx, _), name in zip(inference_entries(cm), INFERENCE_BODIES):
        path = capture_dir / f"clean_body_{idx}.json"
        raw = path.read_bytes()
        if carries_only_the_placeholder(raw):
            line = f"placeholder {name} sha256={hashlib.sha256(raw).hexdigest()}"
            print(line)
            trace.append(line + "\n")
        elif has_stamp(raw):
            stamped.append((name, path))
    status = 0
    if stamped and shutil.which("cargo") is None:
        print("hash check skipped: cargo not found")
        stamped, status = [], 2
    for name, path in stamped:
        raw = path.read_bytes()
        stamp = LINE_STAMP.search(billing_line(raw)).group(0)
        line = f"raw {name} sha256={hashlib.sha256(raw).hexdigest()} cch={stamp[len('cch='):-1]}"
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
    if trace:
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


def check(
    capture_dir: pathlib.Path,
    baseline: pathlib.Path | None = None,
    skip_hashes: bool = False,
    control: pathlib.Path | None = None,
) -> int:
    with tempfile.TemporaryDirectory() as tmp:
        fresh_dir = pathlib.Path(tmp) / "fixtures"
        unwritten = build(capture_dir, fresh_dir, resign=False, control=control, strict=False)
        for name, reason in unwritten:
            print(f"{name}: {reason}")
        cm = json.load(open(capture_dir / "clean_modes.json"))
        resent = resent_posts(cm)
        if resent:
            print("re-sent after the receiver's answer:")
            kept = [idx for idx, _ in inference_entries(cm)]
            for idx, count in resent:
                print(f"  {INFERENCE_BODIES[kept.index(idx)]}: {count} posts, the first kept")
        if THROUGH_ROUTER:
            for line in restored_report(fresh_dir):
                print(line)
        hashes = 0 if skip_hashes else check_hashes(capture_dir)
        if baseline is not None:
            # The same two masks that apply to the router capture apply here too, so a live date or a live
            # account_uuid in the baseline's own bodies never shows up as drift against itself.
            baseline_dir = pathlib.Path(tmp) / "baseline"
            for name, reason in build(baseline, baseline_dir, resign=False, strict=False):
                print(f"baseline {name}: {reason}")
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
        # A baseline that lacks the same fixture leaves it unwritten on both sides, where the comparison finds no
        # difference, so the capture's own list decides as well.
        if unwritten:
            print("fixtures not captured: " + ", ".join(name for name, _ in unwritten))
        if diffs or unwritten:
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
        build_count(capture_dir, fresh_dir, resign=False)
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
    parser.add_argument(
        "--control",
        metavar="DIR",
        help="read the control plane from this directory of control receiver recordings instead of capture_dir/control",
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
    control = pathlib.Path(args.control).expanduser() if args.control else None
    if args.check:
        baseline = pathlib.Path(args.baseline).expanduser() if args.baseline else None
        return check(src, baseline=baseline, skip_hashes=args.skip_hash_check, control=control)
    build(src, FIXTURES, control=control)
    print("fixtures written:", sorted(p.name for p in FIXTURES.rglob("*") if p.is_file()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
