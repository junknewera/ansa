"""Loopback-only peer for the actual M7 auth/provider transport bridge."""
import hashlib
import socket
import threading
from pathlib import Path
from protocol_network import ProtocolPeer
from risk_transport import tls_context

CASES=('valid-control','interim-control','duplicate-head-mime','wrong-mime','repeated-charset','unterminated-charset','truncated-after-terminal','mime-trailer-after-terminal','missing-head-mime-with-trailer','info-count','info-bytes','head-trailer-count','long-reason','chunk-budget')
def wire(case,body):
    if case in CASES[:9]:return ProtocolPeer.expected_wire(case,body)
    valid=ProtocolPeer.expected_wire('valid-control',body)
    if case=='info-count':return b'HTTP/1.1 103 Early Hints\r\n\r\n'*257+valid
    if case=='info-bytes':return (b'HTTP/1.1 103 Early Hints\r\nX-Padding: '+b'x'*33000+b'\r\n\r\n')*2+valid
    if case=='long-reason':return valid.replace(b'200 OK',b'200 '+b'x'*66000,1)
    if case=='head-trailer-count':
        head=b'HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n'+b''.join(b'X-A-%d: x\r\n'%i for i in range(200))+b'Transfer-Encoding: chunked\r\n\r\n'
        return head+(b'%x\r\n'%len(body))+body+b'\r\n0\r\n'+b''.join(b'X-T-%d: x\r\n'%i for i in range(100))+b'\r\n'
    if case=='chunk-budget':
        body=b':'+b'x'*16000+b'\n\n'+body
        return b'HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n'+b''.join(b'1\r\n'+bytes([c])+b'\r\n' for c in body)+b'0\r\n\r\n'
    raise ValueError('unknown bridge case')
class Peer:
    def __init__(self,case,body,fixtures,tls):
        if case not in CASES:raise ValueError('bridge peer case')
        self.case=case;self.response=wire(case,body);self.tls=tls;self.fixtures=fixtures;self.records=[];self.error=None
        self.socket=socket.socket();self.socket.bind(('127.0.0.1',0));self.socket.listen(1);self.socket.settimeout(4);self.port=self.socket.getsockname()[1];self.active=None
        self.thread=threading.Thread(target=self.serve);self.thread.start()
    def url(self):return ('https' if self.tls else 'http')+'://localhost:'+str(self.port)+'/'+self.case
    def serve(self):
        connection=None
        try:
            connection,_=self.socket.accept();self.active=connection;connection.settimeout(3)
            if self.tls:connection=tls_context(self.fixtures).wrap_socket(connection,server_side=True);self.active=connection
            method,target,version,head,body,authorization=ProtocolPeer._read_request(connection)
            sent=0
            try:
                for offset in range(0,len(self.response),1024):piece=self.response[offset:offset+1024];connection.sendall(piece);sent+=len(piece)
            except OSError:pass
            values={}
            for line in head.split(b'\r\n')[1:]:
                key,_,value=line.partition(b':');values.setdefault(key.decode().lower(),[]).append(value.strip().decode())
            self.records.append({'method':method,'target':target,'version':version,'request_body_hex':body.hex(),'authorization_present':authorization,'authorization_matches':values.get('authorization')==['Bearer synthetic-bridge-bearer'],'content_types':values.get('content-type',[]),'hosts':values.get('host',[]),'response_wire_hex':self.response.hex(),'sent':sent,'response_sha256':hashlib.sha256(self.response).hexdigest()})
        except (OSError,EOFError,ValueError) as error:self.error=type(error).__name__
        finally:
            if connection:
                try:connection.shutdown(socket.SHUT_RDWR)
                except OSError:pass
                connection.close()
    def close(self):
        self.socket.close();self.thread.join(timeout=4)
        if self.thread.is_alive():
            if self.active:self.active.close()
            self.thread.join(timeout=1)
            raise AssertionError('bridge peer escaped join')
