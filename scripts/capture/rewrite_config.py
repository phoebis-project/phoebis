# Writes a capture instance's configuration from the deployment's own: the three listeners move to loopback
# ports the capture owns, the two public ones over plain HTTP with whatever `tls` the deployment names removed, and
# the two upstream overrides point the resolver at the TLS receiver. A key that is absent is inserted, because a
# deployment's file may carry no `upstream` section at all and both overrides default. A key or section that appears
# twice is refused, because which line governs is then a guess.
# Usage: rewrite_config.py <in.yaml> <out.yaml> <tls-port> <ca-pem>
#        rewrite_config.py --self-test
import re
import sys


def rewrite(text: str, tls_port: int, ca_pem: str) -> str:
    lines = text.splitlines()
    wanted = [
        ("admin", "listen", "127.0.0.1:48081"),
        ("admin", "tls", None),
        ("proxy", "listen", "127.0.0.1:48317"),
        ("proxy", "tls", None),
        ("stats", "metrics_listen", "127.0.0.1:49464"),
        ("upstream", "extra_ca_pem", ca_pem),
        ("upstream", "resolve_override", f"127.0.0.1:{tls_port}"),
    ]
    for section, key, value in wanted:
        at = [i for i, line in enumerate(lines) if line.startswith(f"{section}:")]
        if len(at) > 1:
            raise SystemExit(f"{section}: appears {len(at)} times; which one governs is ambiguous")
        if not at:
            if value is not None:
                lines += [f"{section}:", f"  {key}: {value}"]
            continue
        start = at[0] + 1
        end = next((i for i in range(start, len(lines)) if lines[i] and not lines[i].startswith((" ", "\t", "#"))), len(lines))
        keyed = [i for i in range(start, end) if re.match(rf"\s+{re.escape(key)}:", lines[i])]
        if len(keyed) > 1:
            raise SystemExit(f"{section}.{key} appears {len(keyed)} times; which one governs is ambiguous")
        replacement = [] if value is None else [f"  {key}: {value}"]
        if keyed:
            # A mapping value (a `tls` pair) leaves with its key: the lines indented deeper than the key are its
            # children, and left under a new scalar, or under no key at all, they would not parse. Blank and comment
            # lines do not end the mapping, which a valid file may space out; the next entry at the key's depth or
            # shallower does, and the blank and comment lines just before that entry stay with it.
            at_key = keyed[0]
            depth = len(lines[at_key]) - len(lines[at_key].lstrip())
            past = at_key + 1
            for i in range(at_key + 1, len(lines)):
                stripped = lines[i].strip()
                if not stripped or stripped.startswith("#"):
                    continue
                if len(lines[i]) - len(lines[i].lstrip()) <= depth:
                    break
                past = i + 1
            lines[at_key:past] = replacement
        else:
            lines[start:start] = replacement
    return "\n".join(lines) + "\n"


def self_test() -> None:
    out = rewrite(open("deploy/config.example.yaml").read(), 49443, "/tmp/ca.pem")
    for expected in ("  listen: 127.0.0.1:48081", "  listen: 127.0.0.1:48317", "  metrics_listen: 127.0.0.1:49464",
                     "upstream:", "  extra_ca_pem: /tmp/ca.pem", "  resolve_override: 127.0.0.1:49443"):
        assert expected in out, f"the example's rewrite lacks {expected!r}"
    assert "127.0.0.1:8317" not in out, "the deployment's own listener survived the rewrite"
    import yaml
    loaded = yaml.safe_load(out)
    assert "tls" not in loaded["admin"] and "tls" not in loaded["proxy"], (
        "the capture's two loopback listeners serve plain HTTP")
    paired = rewrite("proxy:\n  listen: 0.0.0.0:8317\n  tls:\n    cert_file: /a\n    key_file: /b\nstats:\n"
                     "  retention_days: 90\n", 49443, "/tmp/ca.pem")
    assert "cert_file" not in paired and "key_file" not in paired, f"a pair's children survived: {paired!r}"
    lines = paired.splitlines()
    block = lines[lines.index("proxy:") + 1:lines.index("stats:")]
    assert sorted(block) == ["  listen: 127.0.0.1:48317"], f"the proxy block: {block!r}"
    assert "tls" not in paired, f"both listeners serve plain HTTP: {paired!r}"
    yaml.safe_load(paired)
    assert yaml.safe_load(paired)["upstream"] == {"extra_ca_pem": "/tmp/ca.pem",
                                                  "resolve_override": "127.0.0.1:49443"}, (
        f"the appended upstream block: {paired!r}")
    gapped = rewrite("proxy:\n  listen: 0.0.0.0:8317\n  tls:\n    cert_file: /a\n\n    # the key beside it\n"
                     "    key_file: /b\n\n# the stats block\nstats:\n  retention_days: 90\n", 49443, "/tmp/ca.pem")
    assert "cert_file" not in gapped and "key_file" not in gapped, f"a gapped pair's children survived: {gapped!r}"
    assert "# the stats block" in gapped, f"the comment past the pair left with it: {gapped!r}"
    assert yaml.safe_load(gapped)["proxy"] == {"listen": "127.0.0.1:48317"}, f"the proxy block: {gapped!r}"
    try:
        rewrite("proxy:\n  listen: a\n  listen: b\n", 1, "x")
    except SystemExit:
        pass
    else:
        raise AssertionError("a duplicate key was rewritten rather than refused")
    print("rewrite_config self-test: ok")


if sys.argv[1] == "--self-test":
    self_test()
else:
    src, dst, tls_port, ca_pem = sys.argv[1:5]
    open(dst, "w").write(rewrite(open(src).read(), int(tls_port), ca_pem))
