import os, socket, threading, sys, time, json, struct, signal
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__))); from hello_lib import parse_hello
PORT=int(sys.argv[1]); OUT=sys.argv[2]; DUR=int(sys.argv[3]); res=[]; lock=threading.RLock()
def dump(*_): 
    with lock: json.dump(res,open(OUT,'w'),indent=1)
signal.signal(signal.SIGTERM, lambda *a:(dump(),sys.exit(0)))
def handle(c):
    try:
        c.settimeout(10); buf=b''
        while b'\r\n\r\n' not in buf:
            d=c.recv(4096)
            if not d: break
            buf+=d
        head=buf.split(b'\r\n\r\n',1)[0].decode('latin1'); line=head.split('\r\n')[0]
        if not line.startswith('CONNECT'):
            with lock: res.append({'kind':'plain','head':head[:800]}); dump()
            return
        target=line.split()[1]; c.sendall(b'HTTP/1.1 200 Connection established\r\n\r\n')
        tb=buf.split(b'\r\n\r\n',1)[1]
        while len(tb)<5: tb+=c.recv(4096)
        need=5+struct.unpack('!H',tb[3:5])[0]
        while len(tb)<need:
            d=c.recv(4096)
            if not d: break
            tb+=d
        h=parse_hello(tb); h['target']=target; h['proxy_head']=head.split('\r\n')[1:]; h['t']=round(time.time(),2)
        with lock: res.append(h); dump()
    except Exception as e:
        with lock: res.append({'error':str(e)}); dump()
    finally: c.close()
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',PORT)); s.listen(16); s.settimeout(1); end=time.time()+DUR
while time.time()<end:
    try: c,_=s.accept(); threading.Thread(target=handle,args=(c,),daemon=True).start()
    except socket.timeout: pass
dump()
