#!/usr/bin/env python3
"""Synthetic same-Pod provider and client. Emits aggregate evidence only."""
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
import os
from pathlib import Path
import socket
import sys
import threading
import time

TOKEN = Path('/run/secrets/gateway-local-token')
KEY = 'sk-SYNTHETIC-REVOKED-BETA2-NOT-A-KEY'
counts = {'requests': 0, 'local_header_received': 0, 'active': 0, 'peak_active': 0, 'synthetic_plaintext_received': 0, 'provider_key_mismatch':0, 'local_token_value_received':0}
lock = threading.Lock()


class Provider(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_GET(self):
        body = json.dumps(counts).encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        request = json.loads(body)
        with lock:
            counts['requests'] += 1
            counts['synthetic_plaintext_received'] += int(b'ghp_SYNTHETICREVOKED' in body)
            counts['local_header_received'] += int(self.headers.get('X-Gateway-Local-Token') is not None)
            counts['provider_key_mismatch'] += int(self.headers.get('Authorization') != 'Bearer '+KEY)
            counts['local_token_value_received'] += int(any('SYNTHETIC-BETA2-LOCAL-TOKEN' in value for value in self.headers.values()))
            counts['active'] += 1
            counts['peak_active'] = max(counts['active'], counts['peak_active'])
        try:
            scenario = request.get('metadata',{}).get('synthetic_scenario')
            if scenario == 'stall-json':
                time.sleep(20)
            if request.get('stream'):
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Connection', 'close')
                self.end_headers()
                if scenario == 'stall-sse':
                    time.sleep(20)
                chunks=1000 if scenario=='slow-downstream' else 150 if scenario=='long-sse' else 5 if scenario=='short-sse' else 50
                chunk=b'data: '+b'x'*32768+b'\n\n' if scenario=='slow-downstream' else b'data: {"synthetic":"safe"}\n\n'
                for _ in range(chunks):
                    self.wfile.write(chunk)
                    self.wfile.flush()
                    time.sleep(0.005 if scenario=='slow-downstream' else 0.2)
                terminal = b'data: [DONE]\n\n' if self.path.endswith('completions') else b'event: response.completed\ndata: {"type":"response.completed","response":{"status":"completed"}}\n\n'
                self.wfile.write(terminal)
                self.wfile.flush()
                self.close_connection = True
            else:
                reply = b'{"synthetic":"safe","status":"completed"}'
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(reply)))
                self.end_headers()
                self.wfile.write(reply)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            with lock:
                counts['active'] -= 1


def call(endpoint='chat', size=1024, stream=False, invalid=False, auth=True, slow=False, cancel=False, shape='safe', duplicate=None):
    text = ('synthetic safe text ' * (size // 20 + 1))[:size]
    if shape == 'findings':
        text = ' '.join(f'ghp_SYNTHETICREVOKED{i:020}' for i in range(100))
    request = {'model': 'gpt-4o-mini', 'stream': stream}
    if slow:
        shape='slow-downstream'
    if shape.startswith('stall-') or shape in ('slow-downstream','long-sse','short-sse'):
        request['metadata']={'synthetic_scenario':shape}
    if endpoint == 'chat':
        request['messages'] = [{'role': 'user', 'content': text}]
        path = '/v1/chat/completions'
    else:
        request.update({'store': False, 'input': text, 'instructions': 'synthetic instructions'})
        path = '/v1/responses'
    if shape == 'dense':
        parameters={'type':'object','properties':{f'p_{i}':{'type':'string','description':'synthetic description '*16} for i in range(64)},'required':[f'p_{i}' for i in range(64)],'additionalProperties':False}
        function={'name':'synthetic_lookup','description':'synthetic tool','parameters':parameters,'strict':True}
        request['tools']=[{'type':'function','function':function}] if endpoint=='chat' else [{'type':'function',**function}]
    if invalid:
        request['unsupported_synthetic_field'] = 'SYNTHETIC-REJECTED-NOT-A-SECRET'
    headers = {'Content-Type': 'application/json', 'Authorization': 'Bearer ' + KEY}
    if auth:
        headers['X-Gateway-Local-Token'] = 'SYNTHETIC-WRONG-LOCAL-TOKEN-NOT-REAL-00000' if auth=='wrong' else TOKEN.read_text().strip()
    started = time.monotonic()
    connection = http.client.HTTPConnection('127.0.0.1', 8787, timeout=15)
    result = {'status': 0, 'truncated': False, 'terminal': False}
    try:
        if duplicate:
            encoded=json.dumps(request).encode()
            connection.putrequest('POST',path)
            connection.putheader('Content-Length',str(len(encoded)))
            for name,value in headers.items():
                connection.putheader(name,value)
                if name==duplicate:
                    connection.putheader(name,value)
            connection.endheaders(encoded)
        else:
            connection.request('POST', path, json.dumps(request), headers)
        response = connection.getresponse()
        result['status'] = response.status
        if cancel:
            response.read(1)
        elif slow:
            for _ in range(12):
                response.read(1)
                time.sleep(0.3)
        else:
            data = response.read(2 * 1024 * 1024)
            result['terminal'] = b'[DONE]' in data or b'response.completed' in data
    except (OSError, http.client.HTTPException):
        result['truncated'] = True
    finally:
        connection.close()
    result['milliseconds'] = (time.monotonic() - started) * 1000
    return result


def get(path, port=8787):
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=3)
    try:
        connection.request('GET', path)
        response = connection.getresponse()
        return {'status': response.status, 'body': json.loads(response.read(131072))}
    finally:
        connection.close()


def load(seconds, clients, size, shape):
    deadline = time.monotonic() + seconds
    rows = []
    def worker(index):
        local = []
        while time.monotonic() < deadline:
            local.append(call('chat' if index % 2 else 'responses', size=size, shape=shape))
        return local
    with ThreadPoolExecutor(max_workers=clients) as pool:
        for result in pool.map(worker, range(clients)):
            rows.extend(result)
    times = sorted(row['milliseconds'] for row in rows)
    statuses = {str(code): sum(row['status'] == code for row in rows) for code in sorted({row['status'] for row in rows})}
    successful_times=sorted(row['milliseconds'] for row in rows if row['status']==200)
    print(json.dumps({'requests': len(rows), 'clients': clients, 'text_bytes': size,
                      'seconds': seconds, 'shape':shape, 'statuses': statuses,
                      'roundtrip_ms': {f'p{percent}': times[min(len(times)-1, len(times)*percent//100)] if times else None for percent in (50,95,99)},
                      'successful_roundtrip_ms':{f'p{percent}':successful_times[min(len(successful_times)-1,len(successful_times)*percent//100)] if successful_times else None for percent in (50,95,99)}}))


if __name__ == '__main__':
    mode = sys.argv[1]
    if mode == 'provider':
        class DualStack(ThreadingHTTPServer):
            address_family = socket.AF_INET6
            def server_bind(self):
                self.socket.setsockopt(socket.IPPROTO_IPV6,socket.IPV6_V6ONLY,0)
                super().server_bind()
        DualStack(('::', 9000), Provider).serve_forever()
    elif mode == 'idle':
        import signal
        signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
        print('application-started-after-gateway-startup-probe', flush=True)
        while True:
            time.sleep(30)
    elif mode == 'load':
        load(float(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]),sys.argv[5] if len(sys.argv)>5 else 'safe')
    elif mode == 'snapshot':
        print(json.dumps({'gateway': get('/metrics'), 'provider': get('/stats', 9000)}))
    elif mode == 'reject':
        assert call(invalid=True)['status'] == 422
        assert call(auth=False)['status'] == 401
        assert call(auth='wrong')['status'] == 401
        assert call(duplicate='X-Gateway-Local-Token')['status']==401
        assert call(duplicate='Authorization')['status']==400
        print(json.dumps({'unsupported_status': 422, 'missing_token_status': 401, 'duplicate_local_token_status':401,'duplicate_provider_auth_status':400}))
    elif mode == 'single':
        print(json.dumps(call()))
    elif mode == 'retry-ambiguity':
        results=[call(shape='stall-json') for _ in range(2)]
        assert all(row['status']==504 for row in results)
        print(json.dumps({'statuses':[row['status'] for row in results],'explicit_client_retries':1,'gateway_automatic_retries':0,'exactly_once_claim':False}))
    elif mode == 'json-stall':
        print(json.dumps(call(shape='stall-json')))
    elif mode == 'stall':
        print(json.dumps({'json':call(shape='stall-json'),'sse':call(stream=True,shape='stall-sse')}))
    elif mode == 'complete-stream':
        print(json.dumps({endpoint:call(endpoint,stream=True,shape='short-sse') for endpoint in ['chat','responses']}))
    elif mode in ('cancel', 'slow', 'stream'):
        print(json.dumps(call(stream=True, cancel=mode=='cancel', slow=mode=='slow',shape='long-sse' if mode=='stream' else 'safe')))
    elif mode == 'direct':
        address = sys.argv[2]
        try:
            with socket.create_connection((address, 9000), timeout=2):
                reachable = True
        except OSError:
            reachable = False
        print(json.dumps({'direct_reachable': reachable}))
    elif mode == 'watch-egress':
        import signal
        stopped=[False]
        signal.signal(signal.SIGUSR1,lambda *_:stopped.__setitem__(0,True))
        print(json.dumps({'watch_started':True,'pid':os.getpid()}),flush=True)
        attempts=0
        reachable=0
        started=time.monotonic()
        while not stopped[0]:
            for address in sys.argv[2:]:
                attempts+=1
                try:
                    with socket.create_connection((address,9000),timeout=0.2):
                        reachable+=1
                except OSError:
                    pass
            time.sleep(0.02)
        print(json.dumps({'attempts':attempts,'direct_reachable':reachable,'seconds':time.monotonic()-started}),flush=True)
    elif mode == 'stop-watch':
        import signal
        os.kill(int(sys.argv[2]),signal.SIGUSR1)
        print(json.dumps({'stop_sent':True}))
    elif mode == 'identity':
        status = Path('/proc/self/status').read_text()
        safe = {line.split(':')[0]: line.split(':',1)[1].strip() for line in status.splitlines() if line.split(':')[0] in ('Uid','Gid','CapEff','NoNewPrivs','Seccomp')}
        safe['api_token_mounted'] = Path('/var/run/secrets/kubernetes.io/serviceaccount/token').exists()
        safe['root_read_only'] = os.statvfs('/').f_flag & os.ST_RDONLY != 0
        print(json.dumps(safe))
