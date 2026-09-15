#!/usr/bin/env python3
"""Build crates/profile/fixtures from a capture directory, or --check a capture against them."""
import argparse
import difflib
import json
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

# The inference runs the capture drives, in the order it drives them, and the fixture each one's body becomes.
INFERENCE_BODIES = ["body_oauth.json", "body_fp_9b244.json", "body_apikey.json", "body_fp_e142e.json"]

# The three of those runs whose request head is pinned. The fourth repeats the first-party override and is kept
# for its body alone, as a second hash vector.
INFERENCE_HEADS = ["inference_oauth.http", "inference_oauth_fp.http", "inference_apikey.http"]

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


def redact(line: str) -> str:
    line = re.sub(r"Bearer sk-ant-oat01-\S+", "Bearer sk-ant-oat01-PLACEHOLDER", line)
    line = re.sub(r"x-api-key: sk-ant-api03-\S+", "x-api-key: sk-ant-api03-PLACEHOLDER", line)
    line = re.sub(r"^Host: 127\.0\.0\.1:\d+$", "Host: api.anthropic.com", line)
    return line


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


def build(src: pathlib.Path, out: pathlib.Path) -> None:
    """Write the fixture tree at `out`, sourced from the capture directory `src`.

    `src/mitm/` is optional: a capture without it holds the inference plane only, and the control fixtures and
    canned responses are left unwritten for `check()` to report as not captured.
    """
    (out / "responses").mkdir(parents=True, exist_ok=True)
    cm = json.load(open(src / "clean_modes.json"))
    # Selected by request line rather than by index: a capture that interleaves its HEAD /api/hello probes
    # differently still lands the same four bodies on the same four fixtures.
    posts = [(i, e) for i, e in enumerate(cm) if e["request_line"].startswith("POST /v1/messages")]
    if len(posts) != len(INFERENCE_BODIES):
        raise SystemExit(f"{src}/clean_modes.json holds {len(posts)} POST /v1/messages entries, want 4")
    for (idx, _), name in zip(posts, INFERENCE_BODIES):
        # Byte-for-byte: the captured CCH digits in the heads are the hash of exactly these bytes.
        (out / name).write_bytes((src / f"clean_body_{idx}.json").read_bytes())
    for (_, entry), name, body_name in zip(posts, INFERENCE_HEADS, INFERENCE_BODIES):
        (out / name).write_bytes(head(entry, len((out / body_name).read_bytes())))
    hello = next((e for e in cm if e["request_line"].startswith("HEAD /api/hello")), None)
    if hello is None:
        raise SystemExit(f"{src}/clean_modes.json holds no HEAD /api/hello entry")
    (out / "hello.http").write_bytes(head(hello))
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


def normalize_head(raw: bytes) -> bytes:
    """Replace the per-run session/request id header values so a diff reflects profile drift, not a random id."""
    head_bytes, sep, rest = raw.partition(b"\r\n\r\n")
    lines = head_bytes.decode("latin1").split("\r\n")
    out_lines = []
    for line in lines:
        name, colon, value = line.partition(":")
        placeholder = HEADER_PLACEHOLDERS.get(name.strip().lower())
        out_lines.append(f"{name}: {placeholder}" if colon and placeholder else line)
    return "\r\n".join(out_lines).encode("latin1") + sep + rest


def mask_billing(block: bytes) -> bytes:
    """Mask one billing block's per-capture values, leaving cc_version and cc_entrypoint to be compared."""
    block = BILLING_STAMP.sub(b"cch=CCH-PLACEHOLDER;", block)
    return BILLING_PROMPT_ID.sub(b"cc_prompt_id=PROMPT-ID-PLACEHOLDER;", block)


def normalize_body(raw: bytes) -> bytes:
    """Replace metadata.user_id's device_id and session_id, and the billing block's cc_prompt_id and cch digits.

    Masking the digits is safe only because check_hashes() re-signs every stamped body; on its own it would drop
    the hash from the comparison.
    """
    raw = re.sub(rb'(\\"device_id\\":\\")[^\\]*(\\")', rb"\1DEVICE-ID-PLACEHOLDER\2", raw)
    raw = re.sub(rb'(\\"session_id\\":\\")[^\\]*(\\")', rb"\1SESSION-ID-PLACEHOLDER\2", raw)
    # Every billing-header-shaped string is masked, not only system[0]: a decoy in a tool-use input carries the
    # same per-capture values, and the real block keeps its own hash gate.
    return BILLING_BLOCK.sub(lambda m: mask_billing(m.group(0)), raw)


def has_stamp(body: bytes) -> bool:
    """Whether the first billing block carries a cch=, which is the block phoebis_cch::find_cch signs."""
    block = BILLING_BLOCK.search(body)
    return bool(block and BILLING_STAMP.search(block.group(0)))


def check_hashes(fresh_dir: pathlib.Path) -> int:
    """Re-sign every freshly captured body that carries a cch= stamp, the gate normalize_body's masking removes."""
    stamped = [p for p in sorted(fresh_dir.glob("body_*.json")) if has_stamp(p.read_bytes())]
    if not stamped:
        return 0
    if shutil.which("cargo") is None:
        print("hash check skipped: cargo not found")
        return 2
    status = 0
    for path in stamped:
        run = subprocess.run(
            ["cargo", "run", "-q", "-p", "phoebis-cch", "--example", "resign", "--", str(path)],
            capture_output=True,
            text=True,
        )
        if run.returncode != 0:
            print(f"hash drift: {path.name}")
            for line in (run.stdout + run.stderr).splitlines():
                print(f"  {line}")
            status = 1
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
        elif rel.startswith("body_") and rel.endswith(".json"):
            raw = normalize_body(raw)
        normalized[rel] = raw
    return normalized


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


def check(capture_dir: pathlib.Path) -> int:
    with tempfile.TemporaryDirectory() as tmp:
        fresh_dir = pathlib.Path(tmp) / "fixtures"
        build(capture_dir, fresh_dir)
        hashes = check_hashes(fresh_dir)
        committed = normalize_tree(FIXTURES)
        fresh = normalize_tree(fresh_dir)
        names = sorted(set(committed) | set(fresh))
        uncaptured = [name for name in names if name not in fresh and name in CONTROL_PLANE]
        diffs = [name for name in names if name not in uncaptured and committed.get(name) != fresh.get(name)]
        if uncaptured:
            print("not captured (control plane):")
            for name in uncaptured:
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "capture_dir", nargs="?", default=str(DEFAULT_CAPTURE), help="capture directory to build fixtures from"
    )
    parser.add_argument(
        "--check", action="store_true", help="diff the capture's fixtures against crates/profile/fixtures instead"
    )
    parser.add_argument("--self-test", action="store_true", help="run mask_hello's bounds-check self-test and exit")
    args = parser.parse_args()
    if args.self_test:
        _self_test()
        return 0
    src = pathlib.Path(args.capture_dir).expanduser()
    if args.check:
        return check(src)
    build(src, FIXTURES)
    print("fixtures written:", sorted(p.name for p in FIXTURES.rglob("*") if p.is_file()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
