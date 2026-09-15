import struct
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
