# http_raw.py behind TLS: peeks the ClientHello before the handshake, since nothing upstream of the router ever
# sees it otherwise, then answers each HTTP/1.1 request with a canned 400 over TLS. A header whose name is
# authorization, cookie, proxy-authorization, or x-api-key is written with its value replaced by REDACTED, so a
# live account's token from a paired operator run never reaches disk.
# Usage: tls_raw.py <port> <output-dir> <seconds> <leaf.pem> <leaf.key>
# Writes <output-dir>/clean_modes.json and <output-dir>/clean_body_<i>.json, the shape http_raw.py writes, plus
# <output-dir>/connect.json, the shape connect_capture.py writes, one entry per connection.
import json, os, signal, socket, ssl, sys, threading, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from hello_lib import parse_hello

PORT = int(sys.argv[1]); OUT = sys.argv[2]; DUR = int(sys.argv[3]); LEAF_PEM = sys.argv[4]; LEAF_KEY = sys.argv[5]
reqs = []; conns = []; lock = threading.RLock()
os.makedirs(OUT, exist_ok=True)

CREDENTIAL_HEADERS = {b'authorization', b'cookie', b'proxy-authorization', b'x-api-key'}


def redact(line: bytes) -> bytes:
    name, sep, _ = line.partition(b':')
    if sep and name.strip().lower() in CREDENTIAL_HEADERS:
        return name + b': REDACTED'
    return line


def dump(*_):
    with lock:
        json.dump(reqs, open(os.path.join(OUT, 'clean_modes.json'), 'w'), indent=1)
        json.dump(conns, open(os.path.join(OUT, 'connect.json'), 'w'), indent=1)


signal.signal(signal.SIGTERM, lambda *a: (dump(), sys.exit(0)))

context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(LEAF_PEM, LEAF_KEY)
context.set_alpn_protocols(['http/1.1'])


def peek_hello(c, deadline):
    """MSG_PEEK never consumes the bytes, so the handshake below reads the same record fresh."""
    c.settimeout(1)
    head = b''
    while len(head) < 5:
        if time.time() > deadline:
            return None
        try:
            head = c.recv(5, socket.MSG_PEEK)
        except socket.timeout:
            continue
    need = 5 + int.from_bytes(head[3:5], 'big')
    record = head
    while len(record) < need:
        if time.time() > deadline:
            return None
        try:
            record = c.recv(need, socket.MSG_PEEK)
        except socket.timeout:
            continue
    return record


def handle(c):
    try:
        record = peek_hello(c, time.time() + 8)
        if record is None:
            c.close()
            return
        with lock:
            conns.append({**parse_hello(record), 't': round(time.time(), 2)})
            dump()
        tls = context.wrap_socket(c, server_side=True)
    except (OSError, ssl.SSLError):
        c.close()
        return
    try:
        tls.settimeout(8); buf = b''
        while b'\r\n\r\n' not in buf:
            d = tls.recv(65536)
            if not d: break
            buf += d
        head, _, rest = buf.partition(b'\r\n\r\n'); lines = head.split(b'\r\n'); clen = 0
        for ln in lines[1:]:
            k, _, v = ln.partition(b':')
            if k.strip().lower() == b'content-length': clen = int(v.strip())
        while len(rest) < clen:
            d = tls.recv(65536)
            if not d: break
            rest += d
        with lock:
            open(os.path.join(OUT, 'clean_body_%d.json' % len(reqs)), 'wb').write(rest)
            reqs.append({'t': round(time.time(), 2), 'request_line': lines[0].decode(), 'headers': [redact(l).decode() for l in lines[1:]], 'body': rest.decode('utf8', 'replace')})
            dump()
        body = b'{"type":"error","error":{"type":"invalid_request_error","message":"capture"}}'
        tls.sendall(b'HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: ' + str(len(body)).encode() + b'\r\nConnection: close\r\n\r\n' + body)
    finally:
        tls.close()


s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(('127.0.0.1', PORT)); s.listen(16); s.settimeout(1); end = time.time() + DUR
while time.time() < end:
    try: c, _ = s.accept(); threading.Thread(target=handle, args=(c,), daemon=True).start()
    except socket.timeout: pass
dump()
