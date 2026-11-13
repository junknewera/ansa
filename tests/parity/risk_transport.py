"""Synthetic localhost HTTP/TLS/proxy fixtures for the M1 transport probes."""
import gzip
import json
import os
from pathlib import Path
import select
import socket
import ssl
import struct
import subprocess
import threading
from urllib.parse import urlsplit

BODY = b'data: response.completed\n\n'
PROXY_KEYS = ('http_proxy','HTTP_PROXY','https_proxy','HTTPS_PROXY','all_proxy','ALL_PROXY','no_proxy','NO_PROXY')
WIRE_CASES = ('http-valid','http-interim','http-duplicate-mime','http-wrong-mime','http-missing-mime',
              'http-charset','http-bad-charset','http-redirect','http-truncated','http-trailers',
              'http-mime-trailer','http-eof-headers','http-large-headers','http-header-count',
              'http-control-header','http-obsfold','http-body-limit','http-compressed',
              'tls-valid','tls-invalid-ca','tls-hostname','http-info-count','http-head-trailer-count',
              'http-info-byte-budget','http-long-reason','http-chunk-budget')
PROXY_CASES = ('proxy-connect','proxy-https','proxy-connect-denied','proxy-socks5','proxy-socks5h',
               'proxy-socks4','proxy-socks4a','proxy-socks-denied','proxy-unsupported',
               'proxy-lower-precedence','proxy-uppercase-https','proxy-all-fallback',
               'proxy-no-proxy','proxy-no-proxy-precedence','proxy-uppercase-http-ignored','proxy-lower-http')
H2_CASES = ('h2-valid','h2-trailer','h2-mime-trailer','h2-reset','h2-duplicate-mime','h2-large-headers','h2-invalid-ca')
CASES = WIRE_CASES + PROXY_CASES + H2_CASES


def read_head(connection):
    data = b''
    while b'\r\n\r\n' not in data:
        chunk = connection.recv(4096)
        if not chunk:
            raise EOFError('fixture request ended')
        data += chunk
        if len(data) > 65536:
            raise ValueError('fixture request limit')
    return data


def drain_request_body(connection, data):
    head,body=data.split(b'\r\n\r\n',1)
    length=0
    for line in head.split(b'\r\n')[1:]:
        if line.lower().startswith(b'content-length:'):
            length=int(line.split(b':',1)[1])
    if not 0<=length<=16384: raise ValueError('fixture request body limit')
    if len(body)<length: exact(connection,length-len(body))


def exact(connection, count):
    result = b''
    while len(result) < count:
        chunk = connection.recv(count-len(result))
        if not chunk:
            raise EOFError('proxy handshake ended')
        result += chunk
    return result


def cstring(connection):
    data = b''
    while len(data) <= 255:
        byte = exact(connection, 1)
        if byte == b'\0':
            return data
        data += byte
    raise ValueError('proxy field limit')


def tls_context(fixtures):
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(fixtures/'transport/tls/server.pem',fixtures/'transport/tls/server-key.pem')
    context.set_alpn_protocols(['http/1.1'])
    return context


def response(case):
    body = gzip.compress(BODY, mtime=0) if case == 'http-compressed' else BODY
    if case == 'http-eof-headers':
        return b'HTTP/1.1 200 OK\r\nContent-Type: text/event-stream'
    prefix = b''
    if case == 'http-interim':
        prefix = b'HTTP/1.1 103 Early Hints\r\nContent-Type: ignored/interim\r\n\r\n'
    if case == 'http-info-count':
        prefix = b'HTTP/1.1 103 Early Hints\r\n\r\n' * 257
    if case == 'http-info-byte-budget':
        prefix = (b'HTTP/1.1 103 Early Hints\r\nX: '+b'a'*300+b'\r\n\r\n')*200
    if case == 'http-long-reason':
        prefix = (b'HTTP/1.1 103 '+b'a'*40000+b'\r\n\r\n')*2
    head = b'HTTP/1.1 302 Found\r\nLocation: http://localhost:1/forbidden\r\n' if case == 'http-redirect' else b'HTTP/1.1 200 OK\r\n'
    if case != 'http-missing-mime':
        mime = b'application/json' if case == 'http-wrong-mime' else b'text/event-stream'
        if case == 'http-charset': mime += b'; charset="UTF-8"'
        if case == 'http-bad-charset': mime += b'; charset=latin1'
        head += b'Content-Type: '+mime+b'\r\n'
    if case == 'http-duplicate-mime': head += b'Content-Type: text/event-stream\r\n'
    if case == 'http-large-headers': head += b'X-Large: '+b'a'*(70*1024)+b'\r\n'
    if case == 'http-header-count': head += b'X-Count: value\r\n'*257
    if case == 'http-control-header': head += b'X-Control: bad\x01value\r\n'
    if case == 'http-obsfold': head += b'X-Fold: first\r\n second\r\n'
    if case == 'http-compressed': head += b'Content-Encoding: gzip\r\n'
    if case == 'http-chunk-budget':
        body=b'a'*14000
        return head+b'Transfer-Encoding: chunked\r\n\r\n'+b'1\r\na\r\n'*len(body)+b'0\r\n\r\n'
    if case in ('http-trailers','http-mime-trailer','http-head-trailer-count'):
        name = b'Content-Type: text/event-stream' if case == 'http-mime-trailer' else b'X-Check: 1'
        if case == 'http-head-trailer-count':
            head += b'X-Count: value\r\n'*253
            name += b'\r\nX-Second: 2'
        return prefix+head+b'Transfer-Encoding: chunked\r\n\r\n'+f'{len(body):x}\r\n'.encode()+body+b'\r\n0\r\n'+name+b'\r\n\r\n'
    length = len(body)+10 if case == 'http-truncated' else len(body)
    return prefix+head+f'Content-Length: {length}\r\nConnection: close\r\n\r\n'.encode()+body


class FixtureServer:
    def __init__(self, fixtures, case, tls=False, proxy=None, denied=False):
        self.fixtures,self.case,self.tls,self.proxy,self.denied=fixtures,case,tls,proxy,denied
        self.socket=socket.socket()
        self.socket.bind(('127.0.0.1',0)); self.socket.listen(4); self.socket.settimeout(.1)
        self.port=self.socket.getsockname()[1]
        self.stop=threading.Event(); self.connections=[]; self.streams=[]; self.observations=[]
        self.thread=threading.Thread(target=self.serve,daemon=True); self.thread.start()
    def serve(self):
        while not self.stop.is_set():
            try: stream,_=self.socket.accept()
            except socket.timeout: continue
            except OSError: break
            self.connections.append(stream); stream.settimeout(4)
            try:
                if self.tls:
                    stream=tls_context(self.fixtures).wrap_socket(stream,server_side=True,do_handshake_on_connect=False)
                    self.connections.append(stream)
                    if self.stop.is_set(): break
                    stream.do_handshake()
                if self.proxy:
                    self.proxy_request(stream)
                else:
                    head=read_head(stream)
                    drain_request_body(stream,head)
                    self.observations.append({'request':True,'accept_encoding':b'accept-encoding:' in head.lower()})
                    stream.sendall(response(self.case))
            except (OSError,EOFError,ValueError):
                pass  # Deliberately rejected TLS/transfers are part of the corpus.
            finally:
                try: stream.close()
                except OSError: pass
    def proxy_request(self, stream):
        first=exact(stream,1)
        if first==b'\x05':
            methods=exact(stream,exact(stream,1)[0]); stream.sendall(b'\x05\x00')
            header=exact(stream,4); atyp=header[3]
            if atyp==1: host=socket.inet_ntoa(exact(stream,4))
            elif atyp==3: host=exact(stream,exact(stream,1)[0]).decode()
            elif atyp==4: host=socket.inet_ntop(socket.AF_INET6,exact(stream,16))
            else: raise ValueError('SOCKS type')
            port=struct.unpack('!H',exact(stream,2))[0]
            self.observations.append({'proxy':'socks5','dns':'remote' if atyp==3 else 'local'})
            if self.denied: stream.sendall(b'\x05\x05\x00\x01'+b'\0'*6); return
            stream.sendall(b'\x05\x00\x00\x01\x7f\0\0\x01'+struct.pack('!H',port))
            initial=b''
        elif first==b'\x04':
            tail=exact(stream,7); port=struct.unpack('!H',tail[1:3])[0]; ip=tail[3:7]
            cstring(stream)
            remote=ip[:3]==b'\0\0\0' and ip[3]!=0
            host=cstring(stream).decode() if remote else socket.inet_ntoa(ip)
            self.observations.append({'proxy':'socks4','dns':'remote' if remote else 'local'})
            stream.sendall(b'\0\x5b'+b'\0'*6 if self.denied else b'\0\x5a'+struct.pack('!H',port)+b'\x7f\0\0\x01')
            if self.denied: return
            initial=b''
        else:
            head=first
            while b'\r\n\r\n' not in head:
                head += exact(stream,1)
                if len(head)>65536: raise ValueError('CONNECT header limit')
            line=head.split(b'\r\n',1)[0]
            connect=line.startswith(b'CONNECT ')
            self.observations.append({'proxy':'connect' if connect else 'forward'})
            if self.denied: stream.sendall(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n'); return
            host='localhost'; port=self.proxy.port
            if connect:
                stream.sendall(b'HTTP/1.1 200 Connection Established\r\nContent-Type: ignored/proxy\r\n\r\n')
                initial=b''
            else:
                method,url,version=line.split(b' ',2)
                from urllib.parse import urlsplit
                parsed=urlsplit(url.decode()); host=parsed.hostname; port=parsed.port
                initial=method+b' '+(parsed.path or '/').encode()+b' '+version+b'\r\n'+head.split(b'\r\n',1)[1]
        if host not in ('localhost','127.0.0.1','::1') or port!=self.proxy.port:
            raise ValueError('non-local proxy destination')
        upstream=socket.create_connection(('127.0.0.1',port),timeout=2)
        self.connections.append(upstream)
        if self.stop.is_set(): return
        if initial: upstream.sendall(initial)
        def pump(source,target):
            try:
                while not self.stop.is_set():
                    data=source.recv(16384)
                    if not data: break
                    target.sendall(data)
            except OSError: pass
            finally:
                try: target.shutdown(socket.SHUT_WR)
                except OSError: pass
        thread=threading.Thread(target=pump,args=(stream,upstream),daemon=True)
        self.streams.append(thread); thread.start(); pump(upstream,stream)
    def close(self):
        self.stop.set(); self.socket.close()
        for connection in self.connections:
            try: connection.shutdown(socket.SHUT_RDWR)
            except OSError: pass
            connection.close()
        self.thread.join(timeout=3)
        for thread in self.streams: thread.join(timeout=3)
        assert not self.thread.is_alive() and not any(t.is_alive() for t in self.streams), 'fixture thread escaped'


def execute(argv, request, env, cwd):
    environment=os.environ.copy()
    for key in PROXY_KEYS: environment[key]=''
    environment.update(env)
    try:
        result=subprocess.run(list(map(str,argv)),input=(json.dumps(request)+'\n').encode(),capture_output=True,
                              cwd=cwd,env=environment,timeout=6,check=False)
        result.timed_out=False
    except subprocess.TimeoutExpired as error:
        result=subprocess.CompletedProcess(argv,124,error.stdout or b'',error.stderr or b'')
        result.timed_out=True
    return result


def validate_request(case, request, fixtures):
    tls=case in H2_CASES or case.startswith(('tls-','proxy-')) and case not in ('proxy-uppercase-http-ignored','proxy-lower-http')
    url=urlsplit(request['url'])
    assert url.scheme==('https' if tls else 'http') and url.hostname==('127.0.0.1' if case=='tls-hostname' else 'localhost')
    assert url.port and 0<url.port<65536 and url.path=='/' and not url.query and not url.username
    expected={'op':'http','url':request['url'],'proxy_env':{}}
    if case in H2_CASES: expected['h2']=True
    if case=='http-body-limit': expected['body_limit']=4
    if case not in ('tls-invalid-ca','h2-invalid-ca'):
        expected.update(ca_der=str(fixtures/'transport/tls/ca.der'),ca_pem=str(fixtures/'transport/tls/ca.pem'))
    if case in PROXY_CASES:
        env=request['proxy_env']
        keys={'proxy-lower-precedence':{'https_proxy','HTTPS_PROXY'},'proxy-uppercase-https':{'HTTPS_PROXY'},
              'proxy-all-fallback':{'ALL_PROXY'},'proxy-no-proxy':{'https_proxy','NO_PROXY'},
              'proxy-no-proxy-precedence':{'https_proxy','no_proxy','NO_PROXY'},
              'proxy-uppercase-http-ignored':{'HTTP_PROXY'},'proxy-lower-http':{'http_proxy'}}.get(case,{'https_proxy'})
        assert set(env)==keys
        scheme={'proxy-https':'https','proxy-socks5':'socks5','proxy-socks5h':'socks5h','proxy-socks4':'socks4',
                'proxy-socks4a':'socks4a','proxy-socks-denied':'socks5h','proxy-unsupported':'ftp'}.get(case,'http')
        for key,value in env.items():
            if key in ('no_proxy','NO_PROXY'):
                assert value==('example.invalid' if key=='no_proxy' else 'localhost')
            else:
                proxy=urlsplit(value)
                assert proxy.scheme==scheme and proxy.hostname=='localhost' and proxy.port and 0<proxy.port<65536
                assert not proxy.path and not proxy.query and not proxy.username
        if case=='proxy-lower-precedence': assert env['https_proxy']!=env['HTTPS_PROXY']
        expected['proxy_env']=env
    assert request==expected,(case,'request contract',request)


def validate_case(case, record):
    if record['exit_code'] != 0 or record.get('timed_out'):
        raise AssertionError(f'adapter failed {case}: {record["stderr"][:1000]!r}')
    outcome=json.loads(record['stdout'])
    if outcome.get('accepted') is True:
        expected_body=gzip.compress(BODY,mtime=0) if case=='http-compressed' else b'a'*14000 if case=='http-chunk-budget' else BODY
        expected_interim=[103] if case=='http-interim' else [103]*257 if case=='http-info-count' else []
        expected_trailers=['x-check:1'] if case in ('http-trailers','h2-trailer') else []
        assert outcome.get('body_hex')==expected_body.hex(),(case,'body differs',outcome)
        assert outcome.get('interim')==expected_interim,(case,'interim differs',outcome)
        assert outcome.get('trailers')==expected_trailers,(case,'trailers differ',outcome)
    observations=record['observations']
    if case in PROXY_CASES:
        origin_count=len(observations[0]); proxy_count=len(observations[1]); trap_count=len(observations[2])
        assert trap_count==0,(case,'wrong precedence',observations)
        bypass=case in ('proxy-no-proxy','proxy-uppercase-http-ignored')
        fail=case in ('proxy-connect-denied','proxy-socks-denied','proxy-unsupported')
        assert origin_count==(0 if fail else 1),(case,'origin count',observations)
        assert proxy_count==(0 if bypass or case=='proxy-unsupported' else 1),(case,'proxy count',observations)
        if proxy_count:
            mode='forward' if case=='proxy-lower-http' else ('socks5' if 'socks5' in case or case=='proxy-socks-denied' else 'socks4' if 'socks4' in case else 'connect')
            assert observations[1][0]['proxy']==mode,(case,'proxy mode',observations)
        if case in ('proxy-socks5','proxy-socks4'): assert observations[1][0]['dns']=='local'
        if case in ('proxy-socks5h','proxy-socks4a'): assert observations[1][0]['dns']=='remote'
    if case=='http-compressed':
        assert not observations[0][0]['accept_encoding']
        assert outcome.get('body_hex')==gzip.compress(BODY,mtime=0).hex()
    if case in ('http-truncated','h2-reset','http-mime-trailer','h2-mime-trailer','http-head-trailer-count'):
        assert outcome.get('accepted') is False and outcome.get('uncommitted_body_bytes')==len(BODY),(case,'missing terminal prefix',outcome)
    return outcome


def run_case(case, argv, server_binary, fixtures, cwd):
    servers=[]; h2=None
    try:
        if case in H2_CASES:
            h2=subprocess.Popen([str(server_binary),'--serve-h2',str(fixtures),case],stdout=subprocess.PIPE,stderr=subprocess.PIPE,cwd=cwd)
            if not select.select([h2.stdout],[],[],3)[0]: raise AssertionError('H2 fixture handshake')
            port=int(h2.stdout.readline())
            request={'op':'http','url':f'https://localhost:{port}/','h2':True}
            env={}
        else:
            tls=case.startswith(('tls-','proxy-')) and case not in ('proxy-uppercase-http-ignored','proxy-lower-http')
            origin=FixtureServer(fixtures,case,tls=tls); servers.append(origin)
            host='127.0.0.1' if case=='tls-hostname' else 'localhost'
            request={'op':'http','url':f'{"https" if tls else "http"}://{host}:{origin.port}/'}
            env={}
            if case in PROXY_CASES:
                proxy=FixtureServer(fixtures,case,tls=case=='proxy-https',proxy=origin,denied=case in ('proxy-connect-denied','proxy-socks-denied'))
                trap=FixtureServer(fixtures,case,proxy=origin,denied=True)
                servers += [proxy,trap]
                scheme={'proxy-https':'https','proxy-socks5':'socks5','proxy-socks5h':'socks5h','proxy-socks4':'socks4','proxy-socks4a':'socks4a','proxy-socks-denied':'socks5h','proxy-unsupported':'ftp'}.get(case,'http')
                good=f'{scheme}://localhost:{proxy.port}'
                bad=f'http://localhost:{trap.port}'
                env={'https_proxy':good}
                if case=='proxy-lower-precedence': env={'https_proxy':good,'HTTPS_PROXY':bad}
                elif case=='proxy-uppercase-https': env={'HTTPS_PROXY':good}
                elif case=='proxy-all-fallback': env={'ALL_PROXY':good}
                elif case=='proxy-no-proxy': env={'https_proxy':good,'NO_PROXY':'localhost'}
                elif case=='proxy-no-proxy-precedence': env={'https_proxy':good,'no_proxy':'example.invalid','NO_PROXY':'localhost'}
                elif case=='proxy-uppercase-http-ignored': env={'HTTP_PROXY':bad}
                elif case=='proxy-lower-http': env={'http_proxy':good}
            if case=='http-body-limit': request['body_limit']=4
        request['proxy_env']=env
        if case not in ('tls-invalid-ca','h2-invalid-ca'):
            request['ca_der']=str(fixtures/'transport/tls/ca.der')
            request['ca_pem']=str(fixtures/'transport/tls/ca.pem')
        result=execute(argv,request,env,cwd)
        observations=[server.observations[:] for server in servers]
        return {'request':request,'argv':list(map(str,argv)),'stdout':result.stdout,'stderr':result.stderr,
                'exit_code':result.returncode,'timed_out':result.timed_out,'observations':observations}
    finally:
        for server in reversed(servers): server.close()
        if h2:
            try: h2.communicate(timeout=2)
            except subprocess.TimeoutExpired: h2.kill(); h2.communicate(timeout=2)
