import os, socket, ssl, threading, sys, time, json, signal
PORT=int(sys.argv[1]); OUT=sys.argv[2]; DUR=int(sys.argv[3]); res=[]; lock=threading.RLock()
HERE=os.path.dirname(os.path.abspath(__file__))
ctx=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(os.path.join(HERE,'leaf.pem'),os.path.join(HERE,'leaf.key')); ctx.set_alpn_protocols(['http/1.1'])
TOKEN=json.dumps({"token_type":"Bearer","access_token":"sk-ant-oat01-FAKEACCESS","refresh_token":"sk-ant-ort01-FAKEROTATED","expires_in":28800,"refresh_token_expires_in":2592000,"scope":"user:profile user:inference","account":{"uuid":"00000000-0000-4000-8000-000000000001","email_address":"test@example.invalid"},"organization":{"uuid":"00000000-0000-4000-8000-000000000002","name":"Test Org"}}).encode()
PROFILE=json.dumps({"account":{"uuid":"00000000-0000-4000-8000-000000000001","email":"test@example.invalid","display_name":"Test","full_name":"Test User","has_extra_usage_enabled":False},"organization":{"uuid":"00000000-0000-4000-8000-000000000002","name":"Test Org","organization_type":"claude_max","billing_type":"stripe_subscription","rate_limit_tier":"default_claude_max_5x"}}).encode()
USAGE=json.dumps({"five_hour":{"utilization":12.5,"resets_at":"2026-09-13T15:00:00+00:00"},"seven_day":{"utilization":40.0,"resets_at":"2026-09-18T00:00:00+00:00"},"seven_day_oauth_apps":None,"seven_day_opus":None,"extra_usage":{"is_enabled":False,"monthly_limit":None,"used_credits":None,"utilization":None}}).encode()
def dump(*_):
    with lock: json.dump(res,open(OUT,'w'),indent=1)
signal.signal(signal.SIGTERM, lambda *a:(dump(),sys.exit(0)))
def resp(status, body): return (f'HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n').encode()+body
def handle(c):
    target='?'
    try:
        c.settimeout(15); buf=b''
        while b'\r\n\r\n' not in buf:
            d=c.recv(4096)
            if not d: return
            buf+=d
        head=buf.split(b'\r\n\r\n',1)[0].decode('latin1'); line=head.split('\r\n')[0]
        if not line.startswith('CONNECT'): return
        target=line.split()[1]; c.sendall(b'HTTP/1.1 200 Connection established\r\n\r\n')
        s=ctx.wrap_socket(c, server_side=True); s.settimeout(15)
        rb=b''
        while b'\r\n\r\n' not in rb:
            d=s.recv(65536)
            if not d: break
            rb+=d
        h,_,rest=rb.partition(b'\r\n\r\n'); lines=h.split(b'\r\n'); rl=lines[0].decode('latin1'); path=rl.split()[1] if len(rl.split())>1 else ''
        with lock: res.append({'target':target,'request_line':rl,'headers':[l.decode('latin1') for l in lines[1:]],'body':rest[:2000].decode('latin1')}); dump()
        if path.startswith('/v1/oauth/token'): s.sendall(resp('200 OK',TOKEN))
        elif path.startswith('/api/oauth/profile'): s.sendall(resp('200 OK',PROFILE))
        elif path.startswith('/api/oauth/usage'): s.sendall(resp('200 OK',USAGE))
        elif path.startswith('/api/oauth/claude_cli/roles'): s.sendall(resp('200 OK',b'{"organization_role":"admin","workspace_role":"workspace_admin"}'))
        else: s.sendall(resp('404 Not Found',b'{}'))
    except Exception as e:
        with lock: res.append({'target':target,'error':repr(e)[:160]}); dump()
    finally:
        try: c.close()
        except: pass
srv=socket.socket(); srv.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); srv.bind(('127.0.0.1',PORT)); srv.listen(32); srv.settimeout(1); end=time.time()+DUR
while time.time()<end:
    try: c,_=srv.accept(); threading.Thread(target=handle,args=(c,),daemon=True).start()
    except socket.timeout: pass
dump()
