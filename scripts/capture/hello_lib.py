# The ClientHello reader the capture receivers share, and the compare of two hellos that
# scripts/capture-feature-hellos.sh runs: `python3 -I scripts/capture/hello_lib.py <first.hex> <second.hex>` exits 0
# where the two are equal under the masks, 3 where they differ, after the two masked hellos and the first part that
# differs, and 1 for a file it cannot read as one hello.
import struct, sys
EXT={0:'server_name',5:'status_request',10:'supported_groups',11:'ec_point_formats',13:'signature_algorithms',16:'alpn',18:'sct',21:'padding',23:'extended_master_secret',27:'compress_certificate',35:'session_ticket',41:'pre_shared_key',43:'supported_versions',45:'psk_key_exchange_modes',51:'key_share',17513:'application_settings',65281:'renegotiation_info',65037:'ech'}
def parse_hello(b):
    out={}
    if len(b)<5 or b[0]!=0x16: return {'error':'not-tls','head':b[:16].hex()}
    rlen=struct.unpack('!H',b[3:5])[0]; hs=b[5:5+rlen]; out['record_version']=b[1:3].hex(); out['hello_len']=rlen; out['raw_hex']=b[:5+rlen].hex()
    if not hs or hs[0]!=1: return {'error':'not-clienthello'}
    p=4; out['client_version']=hs[p:p+2].hex(); p+=2; p+=32
    sl=hs[p]; p+=1; out['session_id_len']=sl; p+=sl
    cl=struct.unpack('!H',hs[p:p+2])[0]; p+=2; out['ciphers']=[hs[i:i+2].hex() for i in range(p,p+cl,2)]; p+=cl
    ml=hs[p]; p+=1; out['compression']=hs[p:p+ml].hex(); p+=ml
    el=struct.unpack('!H',hs[p:p+2])[0]; p+=2; end=p+el; exts=[]
    while p+4<=end:
        t,l=struct.unpack('!HH',hs[p:p+4]); d=hs[p+4:p+4+l]; p+=4+l
        e={'type':t,'name':EXT.get(t,'grease' if (t&0x0f0f)==0x0a0a and (t>>8)==(t&0xff) else 'unknown'),'len':l}
        try:
            if t==0: e['sni']=d[5:].decode('latin1')
            elif t==10: e['groups']=[d[i:i+2].hex() for i in range(2,len(d),2)]
            elif t==11: e['points']=d[1:].hex()
            elif t==13: e['sigalgs']=[d[i:i+2].hex() for i in range(2,len(d),2)]
            elif t==16:
                i=2; names=[]
                while i<len(d): n=d[i]; names.append(d[i+1:i+1+n].decode('latin1')); i+=1+n
                e['alpn']=names
            elif t==43: e['versions']=[d[i:i+2].hex() for i in range(1,len(d),2)]
            elif t==45: e['psk_modes']=d[1:].hex()
            elif t==51:
                i=2; ks=[]
                while i+4<=len(d): g=d[i:i+2].hex(); kl=struct.unpack('!H',d[i+2:i+4])[0]; ks.append([g,kl]); i+=4+kl
                e['key_shares']=ks
            elif t==27: e['algs']=[d[i:i+2].hex() for i in range(1,len(d),2)]
        except Exception as ex: e['parse_error']=str(ex)
        exts.append(e)
    out['extensions']=exts; return out
def summary(h):
    if 'error' in h: return str(h)
    s=[f"ver={h['client_version']} sid={h['session_id_len']} len={h['hello_len']} ciphers={','.join(h['ciphers'])}", "  ext-order: "+' '.join(e['name'] for e in h['extensions'])]
    for e in h['extensions']:
        extra={k:v for k,v in e.items() if k not in('type','name','len')}
        if extra: s.append(f"  {e['name']}(len={e['len']}): {extra}")
    return '\n'.join(s)


SERVER_NAME = 0
KEY_SHARE = 51


def walk(record):
    """The parts of a ClientHello record a compare reads, in their order, as (name, bytes) pairs, and the record with
    the masked bytes zeroed. The masks cover what follows the host or the connection rather than the client's TLS
    stack: the random, the session id, the server name and each key share's payload. The session id keeps its length,
    a key share its group and its length, and the server name's extension its type and its place."""
    masked = bytearray(record)
    if len(record) < 5 or record[0] != 0x16:
        raise ValueError("not a TLS handshake record")
    end = 5 + struct.unpack("!H", record[3:5])[0]
    if len(record) < end or record[5] != 1:
        raise ValueError("not a whole ClientHello record")
    p = 9
    parts = [("the record version", record[1:3]), ("the legacy version", record[p : p + 2])]
    p += 2
    masked[p : p + 32] = bytes(32)
    p += 32
    session = record[p]
    parts.append(("the session id's length", record[p : p + 1]))
    masked[p + 1 : p + 1 + session] = bytes(session)
    p += 1 + session
    ciphers = struct.unpack("!H", record[p : p + 2])[0]
    parts.append(("the cipher list", record[p + 2 : p + 2 + ciphers]))
    p += 2 + ciphers
    methods = record[p]
    parts.append(("the compression methods", record[p + 1 : p + 1 + methods]))
    p += 1 + methods
    last = p + 2 + struct.unpack("!H", record[p : p + 2])[0]
    p += 2
    if last > end:
        raise ValueError("an extensions block past the record's end")
    place = 0
    while p + 4 <= last:
        kind, size = struct.unpack("!HH", record[p : p + 4])
        data = p + 4
        if data + size > last:
            raise ValueError(f"extension {kind} runs past the extensions block")
        place += 1
        if kind == SERVER_NAME:
            masked[data : data + size] = bytes(size)
        elif kind == KEY_SHARE:
            q = data + 2
            while q + 4 <= data + size:
                share = struct.unpack("!H", record[q + 2 : q + 4])[0]
                masked[q + 4 : q + 4 + share] = bytes(share)
                q += 4 + share
        name = EXT.get(kind, "unknown")
        compared = b"" if kind == SERVER_NAME else bytes(masked[data : data + size])
        parts.append((f"extension {kind} ({name}) at place {place}", compared))
        p = data + size
    return parts, bytes(masked)


def first_difference(first, second):
    """The first part where two ClientHello records differ under the masks, named for a reader, or None where they
    are equal. A place counts the extensions from 1."""
    ours, theirs = walk(first)[0], walk(second)[0]
    for at in range(max(len(ours), len(theirs))):
        if at >= len(ours):
            return f"{theirs[at][0]}, which the first hello lacks"
        if at >= len(theirs):
            return f"{ours[at][0]}, which the second hello lacks"
        (name, data), (other, other_data) = ours[at], theirs[at]
        if name != other:
            return f"{name}, where the second hello holds {other}"
        if data != other_data:
            return name
    return None


def main(argv):
    if len(argv) != 3:
        print(f"usage: {argv[0]} <first.hex> <second.hex>", file=sys.stderr)
        return 1
    try:
        first, second = (bytes.fromhex(open(path).read().strip()) for path in argv[1:])
        difference = first_difference(first, second)
    except (IndexError, OSError, ValueError, struct.error) as error:
        print(f"hello_lib.py: {error}", file=sys.stderr)
        return 1
    if difference is None:
        print("equal under the masks")
        return 0
    print(f"masked first: {walk(first)[1].hex()}")
    print(f"masked second: {walk(second)[1].hex()}")
    print(f"differs at {difference}")
    return 3


if __name__ == "__main__":
    sys.exit(main(sys.argv))
