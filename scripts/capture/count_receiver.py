# Records every HTTP/1.1 request sent to it like http_raw.py and answers the first POST /v1/messages with a canned
# streamed tool_use answer for Read on the file named on the command line, so the client runs the tool and its
# pre-turn token count reaches the wire; every other request is answered a canned 400. Never connects anywhere.
# Usage: count_receiver.py <port> <output-dir> <seconds> <file-path>
import json, os, signal, socket, sys, threading, time
PORT=int(sys.argv[1]); OUT=sys.argv[2]; DUR=int(sys.argv[3]); FILE=sys.argv[4]; reqs=[]; lock=threading.RLock(); answered=[False]
os.makedirs(OUT,exist_ok=True)
def dump(*_):
    with lock: json.dump(reqs,open(os.path.join(OUT,'clean_modes.json'),'w'),indent=1)
signal.signal(signal.SIGTERM, lambda *a:(dump(),sys.exit(0)))
def sse(events):
    return ''.join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()
def tool_use_answer():
    inp=json.dumps({"file_path":FILE})
    return sse([
        {"type":"message_start","message":{"id":"msg_capture_0001","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":None,"stop_sequence":None,"usage":{"input_tokens":10,"output_tokens":1}}},
        {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_capture_0001","name":"Read","input":{}}},
        {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":inp}},
        {"type":"content_block_stop","index":0},
        {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":None},"usage":{"output_tokens":20}},
        {"type":"message_stop"},
    ])
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
        line=lines[0].decode()
        with lock:
            open(os.path.join(OUT,'clean_body_%d.json'%len(reqs)),'wb').write(rest)
            reqs.append({'t':round(time.time(),2),'request_line':line,'headers':[l.decode() for l in lines[1:]],'body':rest.decode('utf8','replace')})
            dump()
            first=line.split(' ')[1].split('?')[0]=='/v1/messages' and not answered[0]
            if first: answered[0]=True
        if first:
            body=tool_use_answer()
            c.sendall(b'HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: '+str(len(body)).encode()+b'\r\nConnection: close\r\n\r\n'+body)
        else:
            body=b'{"type":"error","error":{"type":"invalid_request_error","message":"capture"}}'
            c.sendall(b'HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: '+str(len(body)).encode()+b'\r\nConnection: close\r\n\r\n'+body)
    finally: c.close()
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',PORT)); s.listen(16); s.settimeout(1); end=time.time()+DUR
while time.time()<end:
    try: c,_=s.accept(); threading.Thread(target=handle,args=(c,),daemon=True).start()
    except socket.timeout: pass
dump()
