import os, socket, threading, sys, time, json, struct, signal
HERE=os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0,HERE); from hello_lib import parse_hello
PORT=int(sys.argv[1]); OUT=sys.argv[2]; DUR=int(sys.argv[3]); res=[]; lock=threading.RLock()
def label():
    # Optional per-connection tag from a sibling `label` file; capture-baseline.sh separates runs by output file instead.
    try: return open(os.path.join(HERE,'label')).read().strip()
    except OSError: return ''
def dump(*_):
    with lock: json.dump(res,open(OUT,'w'),indent=1)
signal.signal(signal.SIGTERM, lambda *a:(dump(),sys.exit(0)))
def handle(c,label):
    try:
        c.settimeout(5); buf=b''
        while len(buf)<5: buf+=c.recv(4096)
        need=5+struct.unpack('!H',buf[3:5])[0]
        while len(buf)<need:
            d=c.recv(4096)
            if not d: break
            buf+=d
        h=parse_hello(buf); h['label']=label
        with lock: res.append(h); dump()
    except Exception as e:
        with lock: res.append({'error':str(e),'label':label}); dump()
    finally: c.close()
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',PORT)); s.listen(16); s.settimeout(1); end=time.time()+DUR
while time.time()<end:
    try: c,_=s.accept(); threading.Thread(target=handle,args=(c,label()),daemon=True).start()
    except socket.timeout: pass
dump()
