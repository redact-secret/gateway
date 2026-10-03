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
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
NODE_IMAGE = 'kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a'
HELPER = 'python:3.13-alpine@sha256:2dd78ad5cf13a0b68f5134dc49aa9950203a8cf4b7463431b9f3b398287c5059'
CLUSTER = 'gateway-beta2'
NS = 'gateway-beta2'
OUT = ROOT / 'qualification/evidence/beta2'
OUT.mkdir(parents=True, exist_ok=True)


def command(*args, data=None, timeout=180):
    result = subprocess.run(args, input=data, capture_output=True, text=True, timeout=timeout, cwd=ROOT)
    if result.returncode:
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
    config_name = name + '-config'
    apply({'apiVersion': 'v1', 'kind': 'ConfigMap', 'metadata': {'name': config_name},
           'data': {'config.json': json.dumps(config)}})
    context = {'runAsNonRoot': True, 'runAsUser': 65532, 'runAsGroup': 65532,
               'allowPrivilegeEscalation': False, 'readOnlyRootFilesystem': True, 'capabilities': {'drop': ['ALL']}}
    probe = {'exec': {'command': ['/usr/local/bin/redact-secret-gateway', 'probe', 'ready', '127.0.0.1:8787']},
             'timeoutSeconds': 3, 'periodSeconds': 2, 'failureThreshold': 60}
    gateway = {'name': 'gateway', 'image': image, 'imagePullPolicy': 'Never', 'restartPolicy': 'Always',
               'args': ['serve-observed', '/etc/gateway/config.json'], 'securityContext': context,
               'resources': {'requests': {'cpu': '250m', 'memory': '128Mi'}, 'limits': {'cpu': cpu, 'memory': memory}},
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
        init.append({'name': 'synthetic-provider', 'image': HELPER, 'restartPolicy': 'Always',
                     'command': ['python', '/tools/actor.py', 'provider'], 'securityContext': {**context, 'runAsUser': 20001, 'runAsGroup': 20001},
                     'startupProbe': {'exec': {'command': ['python', '-c', 'import socket;socket.create_connection(("127.0.0.1",9000),2).close()']}, 'periodSeconds': 1},
                     'volumeMounts': [{'name': 'tools', 'mountPath': '/tools', 'readOnly': True}]})
        gateway['args'] += ['--fake-provider', '127.0.0.1:9000']
    init.append(gateway)
    return {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': name}, 'spec': {
        'automountServiceAccountToken': False, 'enableServiceLinks': False, 'terminationGracePeriodSeconds': 15,
        'securityContext': {'runAsNonRoot': True, 'fsGroup': 65532, 'seccompProfile': {'type': 'RuntimeDefault'}},
        'initContainers': init,
        'containers': [{'name': 'app', 'image': HELPER, 'command': ['python', '/tools/actor.py', 'idle'],
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
'fd_count':len(list(Path(f'/proc/{pid}/fd').iterdir())), 'identity':{k:status[k].strip() for k in ['Uid','Gid','CapEff','NoNewPrivs','Seccomp']}, 'cgroup':values}))
'''
    # kind nodes have no Python assumption: read host-visible proc/cgroup through
    # a transient pinned helper sharing node PID/cgroup namespaces, read-only.
    raw = command('docker', 'run', '--rm', '--pid', 'container:' + node, '--cgroupns', 'host',
                  '--network', 'none', '--read-only', '--cap-drop', 'ALL', '--cap-add', 'SYS_PTRACE', '--security-opt', 'no-new-privileges',
                  '-e', f'GATEWAY_PID={pid}', HELPER, 'python', '-c', script)
    return json.loads(raw)


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


def main():
    arch = {'x86_64': 'amd64', 'aarch64': 'arm64'}.get(platform.machine())
    if platform.system() != 'Linux' or arch is None:
        raise RuntimeError('actual native Linux target required')
    command('kind', 'create', 'cluster', '--name', CLUSTER, '--image', NODE_IMAGE, '--wait', '120s', timeout=300)
    command('kubectl', '--context', 'kind-' + CLUSTER, 'create', 'namespace', NS)
    command('docker', 'pull', HELPER)
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
        'node_image':NODE_IMAGE,'helper_image':HELPER,
        'nodes':json.loads(command('kubectl','--context','kind-'+CLUSTER,'get','nodes','-o','json')),
        'cni_pods':json.loads(command('kubectl','--context','kind-'+CLUSTER,'-n','kube-system','get','pods','-o','json')),
        'core_pin':'=0.1.0-beta.12','config_schema_version':1,
        'cargo_lock_sha256':hashlib.sha256((ROOT/'Cargo.lock').read_bytes()).hexdigest(),
        'candidate_sha256':hashlib.sha256((ROOT/'target/release/redact-secret-gateway').read_bytes()).hexdigest()})
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
    apply(pod_spec('basic','rsg-beta2-qualification:local',qualified=True))
    basic=wait_pod('basic')
    assert actor('basic','direct',basic['status']['podIP'])['direct_reachable'] is True
    evidence('basic-residual-bypass',{'direct_egress_reachable':True,'mandatory_claim':False})
    kube('delete','pod','basic','--wait=true')
    all_rows = []
    for cpu in ['250m','500m','1']:
        name = 'quota-' + cpu
        apply(pod_spec(name,'rsg-beta2-qualification:local',qualified=True,enforced=True,cpu=cpu))
        pod = wait_pod(name)
        assert actor(name,'direct',pod['status']['podIP'])['direct_reachable'] is False
        assert actor(name,'direct','fd00:be7a:2::1')['direct_reachable'] is False
        assert actor(name,'reject')['upstream_delivery_delta'] == 0
        for size in [1024,16384,65536]:
            for clients in [1,8,32]:
                before = {'runtime':runtime_sample(name),'metrics':actor(name,'snapshot')}
                load = actor(name,'load',10,clients,size,timeout=60)
                after = {'runtime':runtime_sample(name),'metrics':actor(name,'snapshot')}
                assert after['metrics']['provider']['body']['local_header_received'] == 0
                row={'cpu_limit':cpu,'load':load,'before':before,'after':after,'provisional':True}
                all_rows.append(row)
                evidence('load',all_rows)
        if cpu != '1':
            kube('delete','pod',name,'--wait=true')
    name='quota-1'
    duration=int(os.environ.get('BETA2_SOAK_SECONDS','600'))
    warm=runtime_sample(name)
    cycles=[]
    started=time.monotonic()
    while time.monotonic()-started < duration:
        cycles.append({'load':actor(name,'load',10,8,16384,timeout=60),'cancel':actor(name,'cancel'),
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
    restarts=[]
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
    evidence('restart',restarts)
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
        'remaining':'TLS/resolver attack matrix and explicit OOM/active-stream shutdown evidence require reconciliation'})


if __name__=='__main__':
    try:
        main()
    except Exception as error:
        evidence('failure',{'stage':'qualification','error_type':type(error).__name__,'passed':False})
        raise
