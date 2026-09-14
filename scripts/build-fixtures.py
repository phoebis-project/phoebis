#!/usr/bin/env python3
"""Build crates/profile/fixtures from the 2026-09-13 capture directory. Run once; commit the output."""
import base64, json, os, pathlib, re
SRC = pathlib.Path.home() / "workspace/cc-capture-2026-09-13/cc-capture"
OUT = pathlib.Path("crates/profile/fixtures"); (OUT / "responses").mkdir(parents=True, exist_ok=True)
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
cm = json.load(open(SRC / "clean_modes.json"))
bodies = {1: "body_oauth.json", 3: "body_fp_9b244.json", 5: "body_apikey.json", 7: "body_fp_e142e.json"}
for idx, name in bodies.items():
    # Byte-for-byte: the captured CCH digits in the heads are the hash of exactly these bytes.
    (OUT / name).write_bytes((SRC / f"clean_body_{idx}.json").read_bytes())
(OUT / "inference_oauth.http").write_bytes(head(cm[1], len((OUT / "body_oauth.json").read_bytes())))
(OUT / "inference_oauth_fp.http").write_bytes(head(cm[3], len((OUT / "body_fp_9b244.json").read_bytes())))
(OUT / "inference_apikey.http").write_bytes(head(cm[5], len((OUT / "body_apikey.json").read_bytes())))
(OUT / "hello.http").write_bytes(head(cm[0]))
login = json.load(open(SRC / "mitm/login.json")); status = json.load(open(SRC / "mitm/status.json"))
def control(entry, name):
    body = entry.get("body", "")
    body = re.sub(r"sk-ant-ort01-\S+?\"", 'sk-ant-ort01-PLACEHOLDER"', body)
    lines = [entry["request_line"]] + [redact(h) for h in entry["headers"]]
    lines = [l if not l.startswith("Content-Length:") else f"Content-Length: {len(body)}" for l in lines]
    (OUT / name).write_bytes(("\r\n".join(lines) + "\r\n\r\n").encode() + body.encode())
control(login[0], "control_token.http"); control(login[1], "control_profile.http")
control(login[3], "control_roles.http"); control(status[15], "control_usage.http")
conn = json.load(open(SRC / "connect.json"))
inf = next(h for h in conn if h["hello_len"] == 1499); ctl = next(h for h in conn if h["hello_len"] == 1473)
(OUT / "hello_inference.hex").write_text(inf["raw_hex"] + "\n"); (OUT / "hello_control.hex").write_text(ctl["raw_hex"] + "\n")
# The tunnel harness answered these synthetic bodies; they are the fake upstream's canned responses.
harness = (SRC / "mitm/mitm5.py").read_text()
for name, var in [("token", "TOKEN"), ("profile", "PROFILE"), ("usage", "USAGE")]:
    m = re.search(rf"^{var}=json\.dumps\((.*)\)\.encode\(\)$", harness, re.M)
    (OUT / "responses" / f"{name}.json").write_text(json.dumps(eval(m.group(1), {"False": False, "None": None, "True": True}), separators=(",", ":")) + "\n")
(OUT / "responses/roles.json").write_text('{"organization_role":"admin","workspace_role":"workspace_admin"}\n')
print("fixtures written:", sorted(p.name for p in OUT.rglob("*") if p.is_file()))
