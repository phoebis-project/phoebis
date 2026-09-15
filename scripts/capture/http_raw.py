import socket, threading, sys, time, json, signal, base64
PORT=int(sys.argv[1]); OUT=sys.argv[2]; DUR=int(sys.argv[3]); reqs=[]; lock=threading.RLock()
def dump(*_):
    with lock: json.dump(reqs,open(OUT,'w'),indent=1)
signal.signal(signal.SIGTERM, lambda *a:(dump(),sys.exit(0)))
def handle(c):
    try:
        c.settimeout(8); buf=b''
        while b'\r\n\r\n' not in buf:
            d=c.recv(65536)
            if not d: break
            buf+=d
        head,_,rest=buf.partition(b'\r\n\r\n'); lines=head.split(b'\r\n'); clen=0
        for ln in lines[1:]:
            k,_,v=ln.partition(b':')
            if k.strip().lower()==b'content-length': clen=int(v.strip())
        while len(rest)<clen:
            d=c.recv(65536)
            if not d: break
            rest+=d
        with lock: reqs.append({'request_line':lines[0].decode(),'headers':[l.decode() for l in lines[1:]],'body_b64':base64.b64encode(rest).decode()}); dump()
        body=b'{"type":"error","error":{"type":"invalid_request_error","message":"capture"}}'
        c.sendall(b'HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: '+str(len(body)).encode()+b'\r\nConnection: close\r\n\r\n'+body)
    finally: c.close()
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',PORT)); s.listen(16); s.settimeout(1); end=time.time()+DUR
while time.time()<end:
    try: c,_=s.accept(); threading.Thread(target=handle,args=(c,),daemon=True).start()
    except socket.timeout: pass
dump()
