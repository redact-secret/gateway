#!/usr/bin/env python3
"""Native Kubernetes Beta 2 qualification. Text/JSON evidence only; no publication.

Exact candidate and separate non-release qualification image are never confused.
Requires kind, kubectl, Docker, Python stdlib and a native Linux runner.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import selectors
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
NODE_IMAGE = 'kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a'
HELPER_SOURCE = 'python:3.13-alpine@sha256:2dd78ad5cf13a0b68f5134dc49aa9950203a8cf4b7463431b9f3b398287c5059'
HELPER = 'rsg-beta2-helper:local'
CLUSTER = 'gateway-beta2'
NS = 'gateway-beta2'
OUT = ROOT / 'qualification/evidence/beta2'
OUT.mkdir(parents=True, exist_ok=True)


CURRENT_STAGE='setup'


def stage(name):
    global CURRENT_STAGE
    CURRENT_STAGE=name
    print('beta2 stage: '+name,flush=True)


def command(*args, data=None, timeout=180, diagnostic=False):
    result = subprocess.run(args, input=data, capture_output=True, text=True, timeout=timeout, cwd=ROOT)
    if result.returncode:
        if diagnostic:
            print('safe OS-counter helper diagnostic: '+result.stderr[-2000:],file=sys.stderr,flush=True)
        # Commands include no live credential; suppress subprocess output to avoid
        # accidentally dumping objects containing the throwaway Secret.
        raise RuntimeError(f'qualification command failed: {args[0]} (exit {result.returncode})')
    return result.stdout.strip()


def kube(*args, **kwargs):
    return command('kubectl', '--context', 'kind-' + CLUSTER, '-n', NS, *args, **kwargs)


def apply(value):
    kube('apply', '-f', '-', data=json.dumps(value))


def evidence(name, value):
    (OUT / (name + '.json')).write_text(json.dumps(value, sort_keys=True, indent=2) + '\n')


def wait_pod(name, ready=True, seconds=180):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        pod = json.loads(kube('get', 'pod', name, '-o', 'json'))
        if not ready or any(c['type'] == 'Ready' and c['status'] == 'True' for c in pod.get('status', {}).get('conditions', [])):
            return pod
        time.sleep(1)
    raise RuntimeError('Pod readiness deadline exceeded')


def actor(pod, mode, *args, timeout=180):
    return json.loads(kube('exec', pod, '-c', 'app', '--', 'python', '/tools/actor.py', mode, *map(str, args), timeout=timeout))


def pod_spec(name, image, qualified=False, enforced=False, cpu='1', memory='256Mi', failure=None):
    config = json.loads((ROOT / 'examples/kubernetes/config.json').read_text())
    config['resources']['limits'].update({'shutdown_drain_ms': 2000, 'stream_idle_ms': 3000,
        'stream_lifetime_ms': 15000, 'stream_write_stall_ms': 2000, 'upstream_header_ms': 3000,
        'upstream_total_ms': 15000})
    if failure == 'config':
        config['schema_version'] = 99
    elif failure == 'token':
        config['deployment']['local_auth']['token']['file'] = '/nonexistent/synthetic-token'
    evidence(name+'-config',config)
    config_name = name + '-config'
    apply({'apiVersion': 'v1', 'kind': 'ConfigMap', 'metadata': {'name': config_name},
           'data': {'config.json': json.dumps(config)}})
    context = {'runAsNonRoot': True, 'runAsUser': 65532, 'runAsGroup': 65532,
               'allowPrivilegeEscalation': False, 'readOnlyRootFilesystem': True, 'capabilities': {'drop': ['ALL']}}
    probe = {'exec': {'command': ['/usr/local/bin/redact-secret-gateway', 'probe', 'ready', '127.0.0.1:8787']},
             'timeoutSeconds': 3, 'periodSeconds': 2, 'failureThreshold': 60}
    gateway = {'name': 'gateway', 'image': image, 'imagePullPolicy': 'Never', 'restartPolicy': 'Always',
               'args': ['serve-observed', '/etc/gateway/config.json'], 'securityContext': context,
               'resources': {'requests': {'cpu': '250m', 'memory': memory if memory in ('4Mi','8Mi') else '128Mi'}, 'limits': {'cpu': cpu, 'memory': memory}},
               'startupProbe': probe, 'readinessProbe': probe,
               'livenessProbe': {**probe, 'periodSeconds': 10, 'failureThreshold': 6},
               'volumeMounts': [{'name': 'config', 'mountPath': '/etc/gateway', 'readOnly': True},
                                {'name': 'token', 'mountPath': '/run/secrets', 'readOnly': True}]}
    init = []
    if enforced:
        init.append({'name': 'operator-egress', 'image': 'rsg-beta2-operator:local', 'imagePullPolicy': 'Never',
                     'command': ['/bin/sh', '-c', 'ip -6 addr add fd00:be7a:2::1/128 dev lo && /bin/sh /tools/install-owner-egress.sh'],
                     'securityContext': {**context, 'runAsNonRoot': False, 'runAsUser': 0, 'runAsGroup': 0,
                                         'capabilities': {'drop': ['ALL'], 'add': ['NET_ADMIN']}},
                     'volumeMounts': [{'name': 'tools', 'mountPath': '/tools', 'readOnly': True},
                                      {'name': 'operator-run', 'mountPath': '/run'}]})
    if qualified:
        init.append({'name': 'synthetic-provider', 'image': HELPER, 'imagePullPolicy':'Never', 'restartPolicy': 'Always',
                     'command': ['python', '/tools/actor.py', 'provider'], 'securityContext': {**context, 'runAsUser': 20001, 'runAsGroup': 20001},
                     'startupProbe': {'exec': {'command': ['python', '-c', 'import socket;socket.create_connection(("127.0.0.1",9000),2).close()']}, 'periodSeconds': 1},
                     'volumeMounts': [{'name': 'tools', 'mountPath': '/tools', 'readOnly': True}]})
        gateway['args'] += ['--fake-provider', '127.0.0.1:9000']
    init.append(gateway)
    return {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': name}, 'spec': {
        'automountServiceAccountToken': False, 'enableServiceLinks': False, 'terminationGracePeriodSeconds': 15,
        'securityContext': {'runAsNonRoot': True, 'fsGroup': 65532, 'seccompProfile': {'type': 'RuntimeDefault'}},
        'initContainers': init,
        'containers': [{'name': 'app', 'image': HELPER, 'imagePullPolicy':'Never', 'command': ['python', '/tools/actor.py', 'idle'],
                        'securityContext': {**context, 'runAsUser': 10001, 'runAsGroup': 10001},
                        'volumeMounts': [{'name': 'tools', 'mountPath': '/tools', 'readOnly': True},
                                         {'name': 'token', 'mountPath': '/run/secrets', 'readOnly': True}]}],
        'volumes': [{'name': 'config', 'configMap': {'name': config_name}}, {'name': 'tools', 'configMap': {'name': 'tools'}},
                    {'name': 'operator-run', 'emptyDir': {'medium': 'Memory', 'sizeLimit': '1Mi'}},
                    {'name': 'token', 'secret': {'secretName': 'local-token', 'defaultMode': 0o440,
                                               'items': [{'key': 'token', 'path': 'gateway-local-token'}]}}]}}


def runtime_sample(pod):
    data = json.loads(kube('get', 'pod', pod, '-o', 'json'))
    gateway = next(s for s in data['status']['initContainerStatuses'] if s['name'] == 'gateway')
    container_id = gateway['containerID'].split('://', 1)[1]
    node = CLUSTER + '-control-plane'
    inspect = json.loads(command('docker', 'exec', node, 'crictl', 'inspect', container_id))
    pid = inspect['info']['pid']
    # Read fixed OS counters only, never cmdline/environ/maps or request memory.
    script = r'''
import json, os
from pathlib import Path
pid = int(os.environ['GATEWAY_PID'])
status = dict(line.split(':',1) for line in Path(f'/proc/{pid}/status').read_text().splitlines() if ':' in line)
cgroup = next(line.split(':',2)[2] for line in Path(f'/proc/{pid}/cgroup').read_text().splitlines() if line.startswith('0::'))
base = Path('/sys/fs/cgroup') / cgroup.lstrip('/')
values = {key: (base/key).read_text().strip() if (base/key).exists() else None for key in ['cpu.max','cpu.stat','memory.max','memory.current','memory.peak','memory.events']}
print(json.dumps({'pid':pid,'rss_kib':int(status.get('VmRSS','0 kB').split()[0]),'threads':int(status['Threads']),
'fd_count':len(list(Path(f'/proc/{pid}/fd').iterdir())),
'root_read_only': bool(os.statvfs(f'/proc/{pid}/root').f_flag & os.ST_RDONLY),
'api_token_mounted':Path(f'/proc/{pid}/root/var/run/secrets/kubernetes.io/serviceaccount/token').exists(), 'identity':{k:status[k].strip() for k in ['Uid','Gid','CapEff','NoNewPrivs','Seccomp']}, 'cgroup':values}))
'''
    # kind nodes have no Python assumption: read host-visible proc/cgroup through
    # a transient pinned helper sharing node PID/cgroup namespaces, read-only.
    raw = command('docker', 'run', '--rm', '--pid', 'container:' + node, '--cgroupns', 'host',
                  '--network', 'none', '--read-only', '--cap-drop', 'ALL', '--cap-add', 'SYS_PTRACE', '--cap-add', 'DAC_READ_SEARCH', '--security-opt', 'apparmor=unconfined', '--security-opt', 'no-new-privileges',
                  '-e', f'GATEWAY_PID={pid}', HELPER, 'python', '-c', script, diagnostic=True)
    result=json.loads(raw)
    assert all(result['cgroup'][key] is not None for key in ['cpu.max','cpu.stat','memory.max','memory.current','memory.peak','memory.events'])
    assert result['identity']['Uid'].split()==['65532']*4
    assert int(result['identity']['CapEff'],16)==0 and result['identity']['NoNewPrivs']=='1' and result['identity']['Seccomp']=='2'
    assert result['root_read_only'] is True and result['api_token_mounted'] is False
    return result


def sampled_load(pod, seconds, clients, size, shape='safe'):
    process=subprocess.Popen(['kubectl','--context','kind-'+CLUSTER,'-n',NS,'exec',pod,'-c','app','--','python','/tools/actor.py','load',str(seconds),str(clients),str(size),shape],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    samples=[]
    deadline=time.monotonic()+60
    while process.poll() is None:
        if time.monotonic()>deadline:
            process.kill()
            raise RuntimeError('sampled workload deadline exceeded')
        samples.append(runtime_sample(pod))
        time.sleep(1)
    output,_=process.communicate(timeout=5)
    if process.returncode: raise RuntimeError('sampled workload client failed')
    result=json.loads(output)
    assert result['statuses'].get('200',0)>0
    result['runtime_samples']=samples
    return result


def restart(pod, signal):
    data = json.loads(kube('get', 'pod', pod, '-o', 'json'))
    before = next(s for s in data['status']['initContainerStatuses'] if s['name'] == 'gateway')
    cid = before['containerID'].split('://',1)[1]
    inspected = json.loads(command('docker', 'exec', CLUSTER + '-control-plane', 'crictl', 'inspect', cid))
    command('docker', 'exec', CLUSTER + '-control-plane', 'kill', '-' + signal, str(inspected['info']['pid']))
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        data = json.loads(kube('get', 'pod', pod, '-o', 'json'))
        after = next(s for s in data['status']['initContainerStatuses'] if s['name'] == 'gateway')
        if after['restartCount'] > before['restartCount']:
            wait_pod(pod)
            return {'signal': signal, 'restarts': after['restartCount'], 'last_termination': after.get('lastState', {}).get('terminated')}
        time.sleep(1)
    raise RuntimeError('gateway restart deadline exceeded')


def tls_cases():
    # Operator-controlled TLS/DNS stand-in. Uses the exact candidate's fixed origin,
    # system hosts resolution and trust store; no product routing/CA test flag.
    rows=[]
    with tempfile.TemporaryDirectory() as temp:
        work=Path(temp)
        certs=work/'certs'
        command('bash','-c','. scripts/deployment-evidence/lib.sh; gen_certs "$1"','--',str(certs))
        command('openssl','req','-new','-key',str(certs/'leaf.key'),'-subj','/CN=synthetic-wrong.example','-out',str(certs/'wrong.csr'))
        (certs/'wrong.ext').write_text('basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:synthetic-wrong.example\n')
        command('openssl','x509','-req','-in',str(certs/'wrong.csr'),'-CA',str(certs/'ca.pem'),'-CAkey',str(certs/'ca.key'),'-CAcreateserial','-days','2','-sha256','-extfile',str(certs/'wrong.ext'),'-out',str(certs/'wrong.pem'))
        cid=command('docker','create','rsg-beta2-candidate:local')
        command('docker','cp',cid+':/etc/ssl/certs/ca-certificates.crt',str(work/'system.crt'))
        command('docker','rm',cid)
        bundle=(work/'system.crt').read_text()+(certs/'ca.pem').read_text()
        apply({'apiVersion':'v1','kind':'ConfigMap','metadata':{'name':'tls-tools'},'data':{
            'tls_stand_in.py':(ROOT/'scripts/deployment-evidence/tools/tls_stand_in.py').read_text()}})
        public='93.184.216.34'
        cases=[
            ('loopback-v4',['127.0.0.1'],'system','upstream_unavailable',0,0,0),
            ('loopback-v6',['::1'],'system','upstream_unavailable',0,0,0),
            ('private-10',['10.77.0.1'],'system','upstream_unavailable',0,0,0),
            ('private-172',['172.31.0.1'],'system','upstream_unavailable',0,0,0),
            ('private-192',['192.168.77.1'],'system','upstream_unavailable',0,0,0),
            ('metadata-v4',['169.254.169.254'],'system','upstream_unavailable',0,0,0),
            ('metadata-v6',['fd00:ec2::254'],'system','upstream_unavailable',0,0,0),
            ('cgnat',['100.64.0.1'],'system','upstream_unavailable',0,0,0),
            ('benchmark',['198.18.0.1'],'system','upstream_unavailable',0,0,0),
            ('mixed',[public,'10.77.0.1'],'system','upstream_unavailable',0,0,0),
            ('public-untrusted',[public],'system','upstream_tls_failure',1,1,0),
            ('public-operator-ca',[public],'operator',None,1,0,1),
            ('hostname-mismatch',[public],'wrong','upstream_tls_failure',1,1,0),
        ]
        for label,addresses,trust,code,accepted,tls_failed,requests in cases:
            name='tls-'+label
            certificate='wrong.pem' if trust=='wrong' else 'leaf.pem'
            apply({'apiVersion':'v1','kind':'Secret','metadata':{'name':name+'-cert'},'stringData':{
                'leaf.pem':(certs/certificate).read_text(),'leaf.key':(certs/'leaf.key').read_text(),'bundle.crt':bundle}})
            pod=pod_spec(name,'rsg-beta2-candidate:local')
            spec=pod['spec']
            spec['hostAliases']=[{'ip':ip,'hostnames':['api.openai.com']} for ip in addresses]
            context=spec['initContainers'][0]['securityContext']
            # Addresses exist only on Pod loopback. Installer is a trusted lab
            # control, never a requirement of basic Gateway installation.
            aliases='; '.join('ip addr add '+ip+'/32 dev lo' for ip in [public,'10.77.0.1','172.31.0.1','192.168.77.1','169.254.169.254','100.64.0.1','198.18.0.1'])+'; ip -6 addr add fd00:ec2::254/128 dev lo'
            installer={'name':'operator-addresses','image':'rsg-beta2-operator:local','imagePullPolicy':'Never',
                'command':['/bin/sh','-ec',aliases],
                'securityContext':{**context,'runAsNonRoot':False,'runAsUser':0,'runAsGroup':0,'capabilities':{'drop':['ALL'],'add':['NET_ADMIN']}}}
            provider={'name':'synthetic-tls-provider','image':HELPER,'imagePullPolicy':'Never','restartPolicy':'Always',
                'command':['python','/tls-tools/tls_stand_in.py','serve','--cert','/certs/leaf.pem','--key','/certs/leaf.key','--ipv6'],
                'securityContext':{**context,'runAsUser':20001,'runAsGroup':20001,'capabilities':{'drop':['ALL'],'add':['NET_BIND_SERVICE']}},
                'startupProbe':{'exec':{'command':['python','/tls-tools/tls_stand_in.py','sync']},'periodSeconds':1,'timeoutSeconds':3,'failureThreshold':30},
                'volumeMounts':[{'name':'certs','mountPath':'/certs','readOnly':True},{'name':'tls-tools','mountPath':'/tls-tools','readOnly':True}]}
            gateway=spec['initContainers'][0]
            if trust!='system':
                gateway['volumeMounts'].append({'name':'certs','mountPath':'/etc/ssl/certs/ca-certificates.crt','subPath':'bundle.crt','readOnly':True})
            spec['initContainers']=[installer,provider,gateway]
            spec['containers'][0]['volumeMounts'].append({'name':'tls-tools','mountPath':'/tls-tools','readOnly':True})
            spec['volumes'] += [{'name':'certs','secret':{'secretName':name+'-cert','defaultMode':0o440}},
                                {'name':'tls-tools','configMap':{'name':'tls-tools'}}]
            apply(pod)
            wait_pod(name)
            before=json.loads(kube('exec',name,'-c','app','--','python','/tls-tools/tls_stand_in.py','sync'))
            # Print only fixed status/error, never the response/body/key.
            result=json.loads(kube('exec',name,'-c','app','--','python','-c',
                'import http.client,json,pathlib;c=http.client.HTTPConnection("127.0.0.1",8787,timeout=10);c.request("POST","/v1/chat/completions",json.dumps({"model":"synthetic-model","messages":[{"role":"user","content":"synthetic evidence ping"}]}),{"Content-Type":"application/json","Authorization":"Bearer sk-SYNTHETIC-REVOKED-BETA2-NOT-A-KEY","X-Gateway-Local-Token":pathlib.Path("/run/secrets/gateway-local-token").read_text().strip()});r=c.getresponse();d=json.loads(r.read(65536));print(json.dumps({"status":r.status,"code":d.get("error",{}).get("code"),"relayed":d.get("id")=="synthetic-stand-in"}))'))
            after=json.loads(kube('exec',name,'-c','app','--','python','/tls-tools/tls_stand_in.py','sync'))
            delta={key:after[key]-before[key] for key in ['accepted','tls_failed','requests']}
            assert result['code']==code and delta=={'accepted':accepted,'tls_failed':tls_failed,'requests':requests}
            assert result['relayed']==(requests==1)
            rows.append({'case':label,'host_answers':addresses,'trust':trust,'result':result,'provider_delta':delta,
                         'candidate_sha256':hashlib.sha256((ROOT/'target/release/redact-secret-gateway').read_bytes()).hexdigest(),
})
            evidence('resolver-tls',rows)
            kube('delete','pod',name,'--wait=true')
            kube('delete','secret',name+'-cert')
    return rows


def shipped_manifest():
    """Execute the shipped Deployment; only substitute images/app test tooling."""
    manifest=(ROOT/'examples/kubernetes/sidecar.yaml').read_text().replace('GATEWAY_IMAGE_DIGEST','rsg-beta2-candidate:local').replace('APPLICATION_IMAGE_DIGEST',HELPER)
    deployment=json.loads(kube('create','--dry-run=client','--validate=false','-f','-','-o','json',data=manifest))
    spec=deployment['spec']['template']['spec']
    spec['initContainers'][0]['imagePullPolicy']='Never'
    app=spec['containers'][0]
    app['imagePullPolicy']='Never'
    app['command']=['python','/tools/actor.py','idle']
    app['volumeMounts'].append({'name':'tools','mountPath':'/tools','readOnly':True})
    spec['volumes'].append({'name':'tools','configMap':{'name':'tools'}})
    spec['volumes'][1]['secret']['secretName']='local-token'
    apply({'apiVersion':'v1','kind':'ConfigMap','metadata':{'name':'gateway-config'},'data':{'config.json':(ROOT/'examples/kubernetes/config.json').read_text()}})
    apply(deployment)
    kube('rollout','status','deployment/gateway-companion','--timeout=180s',timeout=200)
    pod=json.loads(kube('get','pods','-l','app=gateway-companion','-o','json'))['items'][0]
    name=pod['metadata']['name']
    identity=actor(name,'identity')
    assert identity['Uid'].split()==['10001']*4 and int(identity['CapEff'],16)==0
    assert identity['NoNewPrivs']=='1' and identity['Seccomp']=='2'
    assert identity['root_read_only'] is True and identity['api_token_mounted'] is False
    evidence('shipped-deployment',{'manifest_sha256':hashlib.sha256((ROOT/'examples/kubernetes/sidecar.yaml').read_bytes()).hexdigest(),
        'qualification_adaptations':['local candidate image reference','pinned helper application and tool mount','same synthetic Secret under local-token name'],
        'pod_status':pod['status'],'app_identity':identity,'gateway_runtime':runtime_sample(name)})
    kube('delete','deployment','gateway-companion','--wait=true')


def start_egress_watch(pod):
    info=json.loads(kube('get','pod',pod,'-o','json'))
    process=subprocess.Popen(['kubectl','--context','kind-'+CLUSTER,'-n',NS,'exec',pod,'-c','app','--','python','/tools/actor.py','watch-egress',info['status']['podIP'],'fd00:be7a:2::1'],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout,selectors.EVENT_READ)
        if not selector.select(timeout=10):
            process.kill()
            raise RuntimeError('egress watcher start deadline exceeded')
        started=json.loads(process.stdout.readline(1024))
    assert started['watch_started'] is True
    return process,started['pid']


def stop_egress_watch(pod,watch):
    process,pid=watch
    actor(pod,'stop-watch',pid)
    output,_=process.communicate(timeout=10)
    assert process.returncode==0
    result=json.loads(output)
    assert result['attempts']>0 and result['direct_reachable']==0
    return result


def rolling_replacement():
    template=pod_spec('rolling','rsg-beta2-qualification:local',qualified=True,enforced=True)
    spec=template['spec']
    apply({'apiVersion':'apps/v1','kind':'Deployment','metadata':{'name':'qualified-rolling'},
        'spec':{'replicas':1,'selector':{'matchLabels':{'app':'qualified-rolling'}},
        'template':{'metadata':{'labels':{'app':'qualified-rolling'}},'spec':spec}}})
    kube('rollout','status','deployment/qualified-rolling','--timeout=180s',timeout=200)
    old=json.loads(kube('get','pods','-l','app=qualified-rolling','-o','json'))['items'][0]
    name=old['metadata']['name']
    clients=[]
    for mode in ['stream','json-stall']:
        clients.append(subprocess.Popen(['kubectl','--context','kind-'+CLUSTER,'-n',NS,'exec',name,'-c','app','--','python','/tools/actor.py',mode],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True))
    deadline=time.monotonic()+10
    while actor(name,'snapshot')['provider']['body']['active']<2:
        if time.monotonic()>deadline: raise RuntimeError('rolling active request barrier exceeded')
        time.sleep(0.1)
    started=time.monotonic()
    kube('rollout','restart','deployment/qualified-rolling')
    kube('rollout','status','deployment/qualified-rolling','--timeout=180s',timeout=200)
    new=next(p for p in json.loads(kube('get','pods','-l','app=qualified-rolling','-o','json'))['items'] if p['metadata']['uid']!=old['metadata']['uid'] and not p['metadata'].get('deletionTimestamp'))
    recovered=wait_pod(new['metadata']['name'])
    assert actor(new['metadata']['name'],'direct',recovered['status']['podIP'])['direct_reachable'] is False
    assert actor(new['metadata']['name'],'load',3,1,1024)['statuses'].get('200',0)>0
    outcomes=[]
    for process in clients:
        output,_=process.communicate(timeout=30)
        result=json.loads(output) if output.strip() else None
        if result: assert result['terminal'] is False
        outcomes.append({'client_exit':process.returncode,'aggregate_result':result,'automatic_retry':False})
    evidence('rolling-replacement',{'old_uid':old['metadata']['uid'],'new_uid':new['metadata']['uid'],
        'elapsed_seconds':time.monotonic()-started,'active_json_and_sse':outcomes,
        'direct_egress_denied':True,'mediated_traffic_succeeded':True})
    kube('delete','deployment','qualified-rolling','--wait=true')


def main():
    arch = {'x86_64': 'amd64', 'aarch64': 'arm64'}.get(platform.machine())
    if platform.system() != 'Linux' or arch is None:
        raise RuntimeError('actual native Linux target required')
    stage('create-native-cluster')
    command('kind', 'create', 'cluster', '--name', CLUSTER, '--image', NODE_IMAGE, '--wait', '120s', timeout=300)
    command('kubectl', '--context', 'kind-' + CLUSTER, 'create', 'namespace', NS)
    command('docker', 'pull', HELPER_SOURCE)
    command('docker', 'tag', HELPER_SOURCE, HELPER)
    command('sh', 'scripts/build-image.sh', 'target/release/redact-secret-gateway', 'rsg-beta2-candidate:local', 'linux/' + arch)
    command('sh', 'scripts/build-image.sh', 'qualification/target/release/redact-secret-gateway-qualification', 'rsg-beta2-qualification:local', 'linux/' + arch)
    with tempfile.TemporaryDirectory() as work:
        Path(work, 'Dockerfile').write_text('FROM ' + NODE_IMAGE + '\n')
        command('docker', 'build', '--provenance=false', '--sbom=false', '-t', 'rsg-beta2-operator:local', work)
    for image in ['rsg-beta2-candidate:local', 'rsg-beta2-qualification:local', 'rsg-beta2-operator:local', HELPER]:
        command('kind', 'load', 'docker-image', '--name', CLUSTER, image, timeout=300)
    apply({'apiVersion':'v1', 'kind':'Secret', 'metadata':{'name':'local-token'},
           'stringData':{'token':'SYNTHETIC-BETA2-LOCAL-TOKEN-NOT-REAL-00000000'}})
    apply({'apiVersion':'v1','kind':'ConfigMap','metadata':{'name':'tools'},'data':{
        'actor.py':(ROOT/'qualification/sidecar/actor.py').read_text(),
        'install-owner-egress.sh':(ROOT/'scripts/kubernetes/install-owner-egress.sh').read_text()}})
    evidence('environment', {'recorded_utc':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),
        'source_commit':command('git','rev-parse','HEAD'),'platform':platform.uname()._asdict(),
        'host_load':os.getloadavg(),'quiet_host':False,'kind':command('kind','version'),
        'node_image':NODE_IMAGE,'helper_image':HELPER_SOURCE,'helper_image_id':command('docker','inspect','--format','{{.Id}}',HELPER),
        'nodes':json.loads(command('kubectl','--context','kind-'+CLUSTER,'get','nodes','-o','json')),
        'cni_pods':json.loads(command('kubectl','--context','kind-'+CLUSTER,'-n','kube-system','get','pods','-o','json')),
        'core_pin':'=0.1.0-beta.12','config_schema_version':1,
        'cargo_lock_sha256':hashlib.sha256((ROOT/'Cargo.lock').read_bytes()).hexdigest(),
        'candidate_sha256':hashlib.sha256((ROOT/'target/release/redact-secret-gateway').read_bytes()).hexdigest(),
        'toolchain':command('rustc','-vV'),'qualification_client':'Python stdlib http.client (pinned helper image)',
        'node_sdk_pin':json.loads((ROOT/'qualification/sdk/node/package.json').read_text())['dependencies']['openai'],
        'python_sdk_pin':'3.24.0 (reused Beta 1 SDK workflow, not the sidecar load client)',
        'candidate_image_id':command('docker','inspect','--format','{{.Id}}','rsg-beta2-candidate:local'),
        'qualification_image_id':command('docker','inspect','--format','{{.Id}}','rsg-beta2-qualification:local')})
    stage('candidate-startup-probes-security')
    apply(pod_spec('candidate','rsg-beta2-candidate:local'))
    candidate = wait_pod('candidate')
    evidence('candidate-probes-security', {'pod_status':candidate['status'],'app_identity':actor('candidate','identity'),
                                         'gateway_runtime':runtime_sample('candidate')})
    # Exact candidate: no accepted request or provider key. Auth refusal is local.
    refusal = json.loads(kube('exec','candidate','-c','app','--','python','-c',
        'import http.client,json;c=http.client.HTTPConnection("127.0.0.1",8787,timeout=3);c.request("POST","/v1/responses","{}",{"Content-Type":"application/json"});r=c.getresponse();print(json.dumps({"status":r.status,"body":json.loads(r.read())}))'))
    assert refusal['status'] == 401
    evidence('candidate-local-refusal', refusal)
    kube('delete','pod','candidate','--wait=true')
    stage('shipped-deployment-security')
    shipped_manifest()
    stage('failed-config-and-token-startup')
    refused=[]
    for failure in ['config','token']:
        name='invalid-'+failure
        apply(pod_spec(name,'rsg-beta2-candidate:local',failure=failure))
        deadline=time.monotonic()+120
        while time.monotonic()<deadline:
            data=json.loads(kube('get','pod',name,'-o','json'))
            statuses=data.get('status',{}).get('initContainerStatuses',[])
            last=next((s.get('lastState',{}).get('terminated',s.get('state',{}).get('terminated')) for s in statuses if s['name']=='gateway'),None)
            if last:
                assert last['exitCode']==1
                assert not any('running' in s.get('state',{}) for s in data.get('status',{}).get('containerStatuses',[]))
                refused.append({'failure':failure,'gateway_exit':1,'app_started':False})
                break
            time.sleep(1)
        else:
            raise RuntimeError('failed-start evidence deadline exceeded')
        kube('delete','pod',name,'--wait=true')
    evidence('failed-start',refused)
    stage('basic-residual-bypass-control')
    apply(pod_spec('basic','rsg-beta2-qualification:local',qualified=True))
    basic=wait_pod('basic')
    assert actor('basic','direct',basic['status']['podIP'])['direct_reachable'] is True
    evidence('basic-residual-bypass',{'direct_egress_reachable':True,'mandatory_claim':False})
    kube('delete','pod','basic','--wait=true')
    stage('candidate-resolver-and-tls-attacks')
    tls_cases()
    stage('memory-limit-termination-and-recovery')
    apply(pod_spec('oom','rsg-beta2-candidate:local',memory='4Mi'))
    deadline=time.monotonic()+120
    while time.monotonic()<deadline:
        data=json.loads(kube('get','pod','oom','-o','json'))
        statuses=data.get('status',{}).get('initContainerStatuses',[])
        last=next((s.get('lastState',{}).get('terminated',s.get('state',{}).get('terminated')) for s in statuses if s['name']=='gateway'),None)
        if last and last.get('reason')=='OOMKilled':
            assert not any('running' in s.get('state',{}) for s in data.get('status',{}).get('containerStatuses',[]))
            evidence('oom',{'gateway_exit':last['exitCode'],'reason':'OOMKilled','app_started':False,'cleanup_promised':False})
            break
        time.sleep(1)
    else: raise RuntimeError('OOM evidence deadline exceeded')
    kube('delete','pod','oom','--wait=true')
    apply(pod_spec('oom-recovery','rsg-beta2-qualification:local',qualified=True,enforced=True))
    recovered=wait_pod('oom-recovery')
    assert actor('oom-recovery','direct',recovered['status']['podIP'])['direct_reachable'] is False
    assert actor('oom-recovery','load',3,1,1024)['statuses'].get('200',0)>0
    evidence('oom-recovery',{'ready':True,'direct_egress_denied':True,'mediated_traffic_succeeded':True})
    stage('active-json-sse-rolling-replacement')
    rolling_replacement()
    all_rows = []
    egress_rows=[]
    for cpu in ['250m','500m','1']:
        stage('quota-load-'+cpu)
        name = 'quota-' + cpu
        apply(pod_spec(name,'rsg-beta2-qualification:local',qualified=True,enforced=True,cpu=cpu))
        pod = wait_pod(name)
        assert actor(name,'direct',pod['status']['podIP'])['direct_reachable'] is False
        ipv6_control=json.loads(kube('exec',name,'-c','synthetic-provider','--','python','-c','import socket,json;socket.create_connection(("fd00:be7a:2::1",9000),2).close();print(json.dumps({"ipv6_control_reachable":True}))'))
        assert ipv6_control['ipv6_control_reachable'] is True
        assert actor(name,'direct','fd00:be7a:2::1')['direct_reachable'] is False
        egress_rows.append({'cpu':cpu,'operator_completed':True,'direct_ipv4_denied':True,'direct_ipv6_denied':True,'ipv6_control_reachable':True})
        evidence('mandatory-egress',egress_rows)
        assert actor(name,'reject')['upstream_delivery_delta'] == 0
        for size in [1024,16384,65536]:
            for clients in [1,8,32]:
                before = {'runtime':runtime_sample(name),'metrics':actor(name,'snapshot')}
                load = sampled_load(name,10,clients,size)
                after = {'runtime':runtime_sample(name),'metrics':actor(name,'snapshot')}
                assert all(after['metrics']['provider']['body'][key]==0 for key in ['local_header_received','provider_key_mismatch','local_token_value_received'])
                deliveries=after['metrics']['provider']['body']['requests']-before['metrics']['provider']['body']['requests']
                attempts=after['metrics']['gateway']['body']['upstream_attempts']-before['metrics']['gateway']['body']['upstream_attempts']
                assert deliveries==attempts and attempts<=load['requests']
                row={'cpu_limit':cpu,'load':load,'before':before,'after':after,'provisional':True}
                all_rows.append(row)
                evidence('load',all_rows)
        for shape in ['findings','dense']:
            before={'runtime':runtime_sample(name),'metrics':actor(name,'snapshot')}
            load=sampled_load(name,10,8,16384,shape)
            after={'runtime':runtime_sample(name),'metrics':actor(name,'snapshot')}
            assert after['metrics']['provider']['body']['synthetic_plaintext_received']==0
            all_rows.append({'cpu_limit':cpu,'load':load,'before':before,'after':after,'provisional':True})
            evidence('load',all_rows)
        if cpu != '1':
            kube('delete','pod',name,'--wait=true')
    name='quota-1'
    stage('declared-soak')
    duration=int(os.environ.get('BETA2_SOAK_SECONDS','600'))
    stalls=actor(name,'stall',timeout=60)
    evidence('stalled-provider',stalls)
    assert stalls['json']['status']==504 and stalls['sse']['terminal'] is False
    time.sleep(20)
    warm=runtime_sample(name)
    cycles=[]
    started=time.monotonic()
    while time.monotonic()-started < duration:
        completions=actor(name,'complete-stream')
        assert all(result['status']==200 and result['terminal'] is True for result in completions.values())
        cycles.append({'completed_streams':completions,'load':sampled_load(name,10,8,16384),'cancel':actor(name,'cancel'),
                       'slow':actor(name,'slow'),'runtime':runtime_sample(name),'metrics':actor(name,'snapshot')})
        evidence('soak-progress',{'declared_seconds':duration,'cycles':cycles})
    time.sleep(3)
    recovered=runtime_sample(name)
    # Threshold derived from measured warm baseline, not a universal capacity.
    allowance=max(8192,int(warm['rss_kib']*0.20))
    assert recovered['rss_kib'] <= warm['rss_kib']+allowance
    assert recovered['fd_count'] <= warm['fd_count']+4
    assert recovered['threads'] == warm['threads']
    metrics=actor(name,'snapshot')['gateway']['body']
    assert all(metrics['admission'][key]==0 for key in ['receipt_in_use','inspection_in_use','upstream_in_use','stream_in_use','waiting','memory_units_in_use'])
    evidence('soak-verdict',{'declared_seconds':duration,'actual_seconds':time.monotonic()-started,
                            'warm':warm,'recovered':recovered,'rss_allowance_kib':allowance,'passed':True})
    stage('active-stream-term-and-kill-restart')
    restarts=[]
    watch=start_egress_watch(name)
    for signal in ['TERM','KILL']:
        stream=subprocess.Popen(['kubectl','--context','kind-'+CLUSTER,'-n',NS,'exec',name,'-c','app','--','python','/tools/actor.py','stream'],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        deadline=time.monotonic()+10
        while actor(name,'snapshot')['provider']['body']['active']==0:
            if time.monotonic()>deadline: raise RuntimeError('active stream start deadline')
            time.sleep(0.1)
        row=restart(name,signal)
        output,_=stream.communicate(timeout=30)
        result=json.loads(output)
        assert result['status']==200 and result['terminal'] is False
        row['active_stream']=result
        restarts.append(row)
    evidence('restart',{'cycles':restarts,'continuous_ipv4_ipv6_egress':stop_egress_watch(name,watch)})
    pod=wait_pod(name)
    assert actor(name,'direct',pod['status']['podIP'])['direct_reachable'] is False
    assert actor(name,'load',3,1,1024)['statuses'].get('200',0)>0
    kube('delete','pod',name,'--wait=true')
    apply(pod_spec('replacement','rsg-beta2-qualification:local',qualified=True,enforced=True))
    replacement=wait_pod('replacement')
    assert actor('replacement','direct',replacement['status']['podIP'])['direct_reachable'] is False
    assert actor('replacement','load',3,1,1024)['statuses'].get('200',0)>0
    evidence('replacement',{'direct_egress_denied':True,'mediated_traffic_succeeded':True})
    evidence('verdict',{'passed':True,'architecture':arch,'publication':False,
        'scope':'native sidecar startup/security, UID egress, cgroup load, bounded soak and restart/replacement',
        'remaining':'reconcile archived datasets, measured versus provisional support and final candidate handoff to #15'})


if __name__=='__main__':
    try:
        main()
    except Exception as error:
        import traceback
        evidence('failure',{'stage':CURRENT_STAGE,'error_type':type(error).__name__,'passed':False,
            'stack':[{'function':frame.name,'line':frame.lineno} for frame in traceback.extract_tb(error.__traceback__)]})
        try:
            pods=json.loads(kube('get','pods','-o','json',timeout=20))
            evidence('failed-pod-status',[{'name':pod['metadata']['name'],'status':pod.get('status',{})} for pod in pods['items']])
        except Exception:
            pass
        print('Beta 2 qualification failed; safe aggregate diagnostics are archived',file=sys.stderr)
        sys.exit(1)
