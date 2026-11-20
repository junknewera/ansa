"""Owned synthetic v1 stores, crash boundaries and cross-binary recovery."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import stat
import tempfile

import auth_cases as vectors
import protocol

FILES = ('registration.json', 'credentials.json', 'signed-out', 'lock')
CORRUPT = ['directory-mode', 'credentials-mode', 'credentials-symlink', 'credentials-hardlink',
           'credentials-fifo', 'credentials-empty', 'credentials-oversize', 'credentials-json',
           'lock-symlink', 'lock-hardlink', 'lock-fifo', 'marker-symlink', 'marker-hardlink', 'marker-fifo',
           'pending-symlink', 'pending-hardlink', 'pending-fifo', 'pending-cleanup-control',
           'log-symlink', 'log-hardlink', 'log-fifo', 'version', 'binding-client', 'binding-subject',
           'binding-host', 'refresh-pending', 'scope', 'refresh-not-ready', 'root-nul']
CRASH = [
    # name, action, initial login, point, file, matching occurrence
    ('login-host-temp','login',False,'temporary_synced','registration.json',1),
    ('login-host-replaced','login',False,'replaced','registration.json',1),
    ('login-intent-temp','login',False,'temporary_synced','registration.json',2),
    ('login-intent-replaced','login',False,'replaced','registration.json',2),
    ('login-identity-temp','login',False,'temporary_synced','registration.json',3),
    ('login-identity-replaced','login',False,'replaced','registration.json',3),
    ('login-tokens-temp','login',False,'temporary_synced','credentials.json',1),
    ('login-tokens-replaced','login',False,'replaced','credentials.json',1),
    ('login-final-temp','login',False,'temporary_synced','registration.json',4),
    ('login-final-replaced','login',False,'replaced','registration.json',4),
    ('reauth-tokens-temp','login',True,'temporary_synced','credentials.json',1),
    ('refresh-intent-temp','refresh',True,'temporary_synced','credentials.json',1),
    ('refresh-intent-replaced','refresh',True,'replaced','credentials.json',1),
    ('refresh-result-temp','refresh',True,'temporary_synced','credentials.json',2),
    ('refresh-result-replaced','refresh',True,'replaced','credentials.json',2),
    ('logout-marker','logout',True,'logout_marked','signed-out',1),
    ('logout-intent-temp','logout',True,'temporary_synced','registration.json',1),
    ('logout-intent-replaced','logout',True,'replaced','registration.json',1),
    ('logout-tokens-removed','logout',True,'tokens_removed','credentials.json',1),
    ('logout-result-temp','logout',True,'temporary_synced','registration.json',2),
    ('logout-result-replaced','logout',True,'replaced','registration.json',2),
]


def check(condition, message):
    if not condition:
        raise ValueError(message)


def uint(value):
    if isinstance(value, protocol.NumberToken):
        check(value.integer and value.lexeme.isascii() and value.lexeme.isdecimal(), 'unsigned metadata')
        value = int(value.lexeme)
    check(type(value) is int and 0 <= value <= 2**64-1, 'unsigned metadata')
    return value


@contextmanager
def fixture_root():
    with tempfile.TemporaryDirectory(prefix='ansa-m3-fixture-') as directory:
        root = Path(directory)
        os.chmod(root, 0o700)
        fd = os.open(root/'fixture-owner', os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd,'wb') as stream:
            stream.write(b'ansa-m3-synthetic-v1\n')
            stream.flush(); os.fsync(stream.fileno())
        yield root


def snapshot(directory, label, root):
    """Read only descriptor-checked, bounded files in our newly minted root."""
    root_fd = os.open(root,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
    result = {'label':label,'entries':sorted(os.listdir(root_fd)),'files':{},'pending':[]}
    def observe(parent, name, prefix, maximum=256*1024):
        try:
            meta=os.stat(name,dir_fd=parent,follow_symlinks=False)
        except FileNotFoundError:
            return {'kind':'missing'}
        kind='file' if stat.S_ISREG(meta.st_mode) else 'directory' if stat.S_ISDIR(meta.st_mode) else 'symlink' if stat.S_ISLNK(meta.st_mode) else 'fifo' if stat.S_ISFIFO(meta.st_mode) else 'other'
        item={'kind':kind,'mode':stat.S_IMODE(meta.st_mode),'nlink':meta.st_nlink,
              'owner':meta.st_uid==os.geteuid(),'size':meta.st_size}
        if kind!='file' or not item['owner'] or item['mode']&0o077 or item['nlink']!=1 or item['size']>maximum:
            return item
        fd=os.open(name,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC,dir_fd=parent)
        with os.fdopen(fd,'rb') as stream:
            opened=os.fstat(stream.fileno())
            check((opened.st_dev,opened.st_ino)==(meta.st_dev,meta.st_ino),'snapshot identity changed')
            data=stream.read(maximum+1)
            check(len(data)==item['size'] and len(data)<=maximum,'snapshot read limit')
        path=directory/(label+'.'+prefix)
        path.write_bytes(data)
        item['raw']={'file':path.name,'sha256':hashlib.sha256(data).hexdigest(),'size':len(data)}
        return item
    try:
        result['marker']=observe(root_fd,'fixture-owner','fixture-owner',64)
        result['log']=observe(root_fd,'exchanges.log','exchanges.log',1024*1024)
        try:
            auth_fd=os.open('auth',os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=root_fd)
        except OSError:
            result['directory']='missing-or-unsafe'
            result['files']={name:{'kind':'missing-or-unsafe'} for name in FILES}
        else:
            try:
                meta=os.fstat(auth_fd)
                if meta.st_uid!=os.geteuid() or meta.st_mode&0o077:
                    result['directory']='unsafe'
                    result['files']={name:{'kind':'missing-or-unsafe'} for name in FILES}
                else:
                    result['directory']='private'
                    result['files']={name:observe(auth_fd,name,name) for name in FILES}
                    for name in sorted(os.listdir(auth_fd)):
                        if name.startswith('pending-'):
                            check(len(name)<=96 and '/' not in name,'pending name')
                            result['pending'].append({'name':name,'entry':observe(auth_fd,name,name)})
            finally:
                os.close(auth_fd)
    finally:
        os.close(root_fd)
    return result


def read_snapshot(directory, value, label):
    check(isinstance(value,dict) and value.get('label')==label,'snapshot label')
    check(set(value.get('files',{}))==set(FILES),'snapshot file inventory')
    def read(entry,name,maximum):
        check(isinstance(entry,dict),'snapshot entry')
        if 'raw' not in entry:
            return None
        check(entry.get('kind')=='file' and entry.get('owner') is True and uint(entry['nlink'])==1
              and uint(entry['mode'])&0o077==0 and uint(entry['size'])<=maximum,'snapshot unsafe file')
        data=protocol.retained_bytes(directory,entry['raw'],label+'.'+name,maximum)
        check(len(data)==uint(entry['size']),'snapshot file size')
        return data
    records={}; invalid=[]
    for name in FILES:
        data=read(value['files'][name],name,256*1024)
        if data is not None and name.endswith('.json'):
            try: records[name]=protocol.strict_loads(data)
            except ValueError: records[name]=None; invalid.append(name)
        else: records[name]=data
    log=read(value.get('log'),'exchanges.log',1024*1024)
    events=log.decode().splitlines() if log is not None else ([] if value['log'].get('kind')=='missing' else None)
    if events is not None:
        check(all(event in ('code','refresh','revoke','models','inference') for event in events),'unexpected fake exchange')
    check(read(value.get('marker'),'fixture-owner',64)==b'ansa-m3-synthetic-v1\n','fixture ownership marker changed')
    pending=value.get('pending')
    check(isinstance(pending,list) and len(pending)<=32,'snapshot pending bound')
    names=[]
    for item in pending:
        name=item.get('name')
        check(isinstance(name,str) and name.startswith('pending-') and len(name)<=96
              and all(c.isascii() and (c.isalnum() or c in '-_') for c in name),'pending name')
        names.append(name); read(item.get('entry'),name,256*1024)
    check(len(names)==len(set(names)),'duplicate pending evidence')
    return {'records':records,'events':events,'pending':pending,'raw':value,'invalid_json':invalid}


def account(record):
    return hashlib.sha256(protocol.exact_dumps([vectors.ISSUER,record['client_id'],record['subject']]).encode()).hexdigest()


def status(state, now):
    record=state['records']['credentials.json']
    if record is None:
        return {'logged_in':False,'refresh_pending':False,'plan_enabled':False,'account':'','expires_in':0,'refresh_wait':0,'session':''}
    pending=record['refresh_pending'] is True
    return {'logged_in':not pending and state['raw']['files']['signed-out']['kind']=='missing',
            'refresh_pending':pending,'plan_enabled':all(scope in record['scope'].split() for scope in ('offline_access','resource.invoke','chatgpt.tokens.use.direct')),
            'account':account(record),'expires_in':max(0,uint(record['expires_at'])-now),
            'refresh_wait':max(0,uint(record['earliest_refresh_at'])-now),'session':record['session']}


def response_matches(actual, state, now, action, confirmed=None):
    wanted={'accepted':True,'status':status(state,now)}
    if action in ('access','refresh'):
        record=state['records']['credentials.json']
        check(record is not None,'successful access without credentials')
        wanted.update(account=account(record),session=record['session'],expires_at=record['expires_at'],bearer_present=True)
    elif action=='logout':
        wanted['confirmed']=confirmed
    check(protocol.structural(actual)==protocol.structural(wanted),'auth status or lease differs from stored v1 state')


def canonical_equal(a,b):
    for name in ('registration.json','credentials.json','signed-out'):
        left=a['raw']['files'][name]; right=b['raw']['files'][name]
        check(left.get('kind')==right.get('kind') and left.get('raw',{}).get('sha256')==right.get('raw',{}).get('sha256'),'read changed canonical store bytes')


def _step(r,root,adapter,label,action,now=vectors.NOW,**options):
    request={'op':'store','root':str(root),'action':action,'now':now,**options}
    command=r.call(label,adapter,request,cwd=root)
    return {'label':label,'adapter':adapter,'command':command,'snapshot':snapshot(r.directory,label,root)}


def _view(r,step,root,action,now=vectors.NOW,exit_code=0,**options):
    expected={'op':'store','root':root,'action':action,'now':now,**options}
    actual=r.read_call(step['command'],step['label'],step['adapter'],expected,exit_code=exit_code,cwd=root)
    return actual,read_snapshot(r.directory,step['snapshot'],step['label'])


def compat(r,writer,reader):
    case_id='compat-'+writer+'-to-'+reader
    with fixture_root() as root:
        steps=[]
        sequence=[(writer,'login',vectors.NOW,{}),(reader,'status',vectors.NOW,{}),
                  (reader,'refresh',vectors.NOW+3550,{}),(writer,'status',vectors.NOW+3550,{}),
                  (writer,'refresh',vectors.NOW+7100,{}),(reader,'access',vectors.NOW+7100,{}),
                  (reader,'logout',vectors.NOW+7100,{}),(writer,'status',vectors.NOW+7100,{}),
                  (writer,'access',vectors.NOW+7100,{}),(writer,'login',vectors.NOW+7100,{}),
                  (reader,'access',vectors.NOW+7100,{}),(reader,'logout',vectors.NOW+7100,{'revoke_failure':True}),
                  (writer,'logout',vectors.NOW+7100,{})]
        for index,(adapter,action,now,options) in enumerate(sequence):
            steps.append(_step(r,root,adapter,case_id+'-'+str(index),action,now,**options))
        return {'id':case_id,'root':str(root),'steps':steps}


def validate_compat(r,row):
    writer='cpp' if row['id']=='compat-cpp-to-rust' else 'rust'; reader='rust' if writer=='cpp' else 'cpp'
    sequence=[(writer,'login',vectors.NOW,{}),(reader,'status',vectors.NOW,{}),(reader,'refresh',vectors.NOW+3550,{}),
              (writer,'status',vectors.NOW+3550,{}),(writer,'refresh',vectors.NOW+7100,{}),(reader,'access',vectors.NOW+7100,{}),
              (reader,'logout',vectors.NOW+7100,{}),(writer,'status',vectors.NOW+7100,{}),(writer,'access',vectors.NOW+7100,{}),
              (writer,'login',vectors.NOW+7100,{}),(reader,'access',vectors.NOW+7100,{}),
              (reader,'logout',vectors.NOW+7100,{'revoke_failure':True}),(writer,'logout',vectors.NOW+7100,{})]
    check(len(row['steps'])==len(sequence),'compat step inventory')
    states=[]
    for index,(step,(adapter,action,now,options)) in enumerate(zip(row['steps'],sequence)):
        check(step['label']==row['id']+'-'+str(index) and step['adapter']==adapter,'compat step identity')
        actual,state=_view(r,step,row['root'],action,now,**options); states.append(state)
        if index==8:
            check(isinstance(actual,dict) and actual.get('accepted') is False,'logged-out state allowed access')
        else:
            response_matches(actual,state,now,action,confirmed=index==6 if action=='logout' else None)
        check(not state['pending'],'compat retained pending copies')
    first=states[0]['records']['credentials.json']; registration=states[0]['records']['registration.json']
    check(first is not None and uint(first['version'])==1 and uint(registration['version'])==1,'v1 registration/credential version')
    check(first['client_id']=='fixture_client' and first['subject']=='fixture_subject'
          and first['host_id']==registration['host_id'] and first['session'],'fixture identity not bound')
    for index in range(6):
        record=states[index]['records']['credentials.json']
        check(account(record)==account(first) and record['session']==first['session'] and record['nonce']==first['nonce']
              and record['host_id']==first['host_id'] and record['refresh_pending'] is False,'refresh changed identity/session/nonce')
        check(states[index]['records']['registration.json']==registration,'refresh changed registration')
    for previous,index in ((0,1),(2,3),(4,5),(6,7),(7,8),(9,10),(11,12)):
        canonical_equal(states[previous],states[index]); check(states[previous]['events']==states[index]['events'],'read/repeated logout performed network')
    for previous,index in ((1,2),(3,4)):
        check(states[index]['records']['credentials.json']['refresh_token']!=states[previous]['records']['credentials.json']['refresh_token'],'fixture did not rotate token')
    check(states[5]['events']==['code','refresh','refresh'],'cross-binary exchanges differ')
    check(states[6]['events']==['code','refresh','refresh','revoke'],'logout exchange differs')
    replacement=states[9]['records']['credentials.json']
    check(account(replacement)==account(first) and replacement['host_id']==first['host_id'] and replacement['session']!=first['session'],'reauthorization changed account/host or retained session')
    check(states[12]['records']['registration.json']['last_revocation_confirmed'] is False,'failed revocation became confirmed')


def crash_case(r,spec):
    name,action,seed,point,file,nth=spec; case_id='crash-'+name; actors=[]
    for adapter in ('cpp','rust'):
        with fixture_root() as root:
            steps=[]
            if seed: steps.append(_step(r,root,adapter,case_id+'-'+adapter+'-seed','login'))
            before=snapshot(r.directory,case_id+'-'+adapter+'-before',root)
            now=vectors.NOW+(3550 if action=='refresh' else 100 if seed and action=='login' else 0)
            fault={'point':point,'name':file,'nth':nth}
            steps.append(_step(r,root,adapter,case_id+'-'+adapter+'-fault',action,now,fault=fault))
            other='rust' if adapter=='cpp' else 'cpp'
            steps.append(_step(r,root,other,case_id+'-'+adapter+'-status','status',now))
            steps.append(_step(r,root,other,case_id+'-'+adapter+'-access','access',now))
            actors.append({'adapter':adapter,'root':str(root),'before':before,'steps':steps})
    return {'id':case_id,'actors':actors}


def validate_crash(r,row,spec):
    name,action,seed,point,file,nth=spec
    check([a.get('adapter') for a in row['actors']]==['cpp','rust'],'crash actor inventory')
    for actor in row['actors']:
        adapter=actor['adapter']; other='rust' if adapter=='cpp' else 'cpp'; prefix=row['id']+'-'+adapter
        steps=actor['steps']; check(len(steps)==3+int(seed),'crash step inventory')
        if seed:
            check(steps[0]['label']==prefix+'-seed' and steps[0]['adapter']==adapter,'crash seed identity')
            actual,state=_view(r,steps[0],actor['root'],'login')
            response_matches(actual,state,vectors.NOW,'login')
        offset=int(seed); now=vectors.NOW+(3550 if action=='refresh' else 100 if seed and action=='login' else 0)
        fault={'point':point,'name':file,'nth':nth}
        step=steps[offset]; check(step['label']==prefix+'-fault' and step['adapter']==adapter,'crash invocation identity')
        _,after=_view(r,step,actor['root'],action,now,exit_code=71,fault=fault)
        before=read_snapshot(r.directory,actor['before'],prefix+'-before')
        pending=point=='temporary_synced'
        check(len(after['pending'])==int(pending),'crash pending copy state')
        if action=='refresh':
            record=after['records']['credentials.json']; check(record is not None,'refresh crash lost credentials')
            wanted_pending=name in ('refresh-intent-replaced','refresh-result-temp')
            check(record['refresh_pending'] is wanted_pending,'refresh crash marker differs')
            check(after['events']==['code']+(['refresh'] if name.startswith('refresh-result') else []),'refresh happened before durable pending or was skipped')
            check(record['session']==before['records']['credentials.json']['session'],'crash changed refresh session')
        elif action=='logout':
            removed=name in ('logout-tokens-removed','logout-result-temp','logout-result-replaced')
            check((after['records']['credentials.json'] is None)==removed,'logout token deletion ordering')
            check(after['raw']['files']['signed-out']['kind']=='file','logout marker not durable')
            check(after['events']==['code']+(['revoke'] if name.startswith('logout-result') else []),'logout revoke ordering')
        else:
            published=name in ('login-tokens-replaced','login-final-temp','login-final-replaced') or seed
            check((after['records']['credentials.json'] is not None)==published,'login publication ordering')
            posted=name.startswith(('login-identity','login-tokens','login-final','reauth-'))
            check(after['events']==(['code'] if seed else [])+(['code'] if posted else []),'code exchange ordering')
            if name=='reauth-tokens-temp':
                check(after['records']['registration.json']['revocation_uncertain'] is True,'unknown prior grant lost uncertainty')
        status_step=steps[offset+1]; access_step=steps[offset+2]
        check(status_step['label']==prefix+'-status' and access_step['label']==prefix+'-access'
              and status_step['adapter']==other and access_step['adapter']==other,'cross-binary recovery identity')
        actual,recovered=_view(r,status_step,actor['root'],'status',now); response_matches(actual,recovered,now,'status')
        canonical_equal(after,recovered)
        check(not recovered['pending'] and recovered['events']==after['events'],'recovery retried exchange or retained copies')
        actual,final=_view(r,access_step,actor['root'],'access',now)
        expected_access=status(recovered,now)['logged_in']
        if expected_access:
            response_matches(actual,final,now,'access')
            additional=['refresh'] if name=='refresh-intent-temp' else []
            check(final['events']==recovered['events']+additional,'recovery repeated ambiguous exchange')
        else:
            check(isinstance(actual,dict) and actual.get('accepted') is False,'pending/signed-out recovery dispatched')
            check(final['events']==recovered['events'],'rejected recovery performed network')


def _write(path,data):
    fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_TRUNC|os.O_NOFOLLOW|os.O_CLOEXEC,0o600)
    with os.fdopen(fd,'wb') as stream:
        stream.write(data); stream.flush(); os.fsync(stream.fileno())


def _mutate(root,name):
    auth=root/'auth'; credentials=auth/'credentials.json'; action='access'
    if name=='root-nul': return 'status'
    if name=='directory-mode': os.chmod(auth,0o777); return 'status'
    targets={'credentials':credentials,'lock':auth/'lock','marker':auth/'signed-out',
             'pending':auth/'pending-guard','log':root/'exchanges.log'}
    family,_,kind=name.partition('-')
    if family in targets and kind in ('symlink','hardlink','fifo'):
        target=targets[family]
        if target.exists(): target.unlink()
        if kind=='symlink': target.symlink_to(root/'sentinel')
        elif kind=='hardlink': target.hardlink_to(root/'sentinel')
        else: os.mkfifo(target,0o600)
        return 'logout' if family=='marker' else 'refresh' if family=='log' else 'status'
    if name=='credentials-mode': os.chmod(credentials,0o644); return 'status'
    if name=='credentials-empty': _write(credentials,b''); return 'status'
    if name=='credentials-oversize': _write(credentials,b'x'*(256*1024+1)); return 'status'
    if name=='credentials-json': _write(credentials,b'{malformed'); return 'status'
    if name=='pending-cleanup-control': _write(auth/'pending-stale',b'owned residue'); return 'status'
    record=json_bytes_file(credentials)
    key={'version':'version','binding-client':'client_id','binding-subject':'subject','binding-host':'host_id',
         'refresh-pending':'refresh_pending','scope':'scope','refresh-not-ready':'earliest_refresh_at'}[name]
    record[key]=2 if name=='version' else True if name=='refresh-pending' else 'openid' if name=='scope' else vectors.NOW+3600 if name=='refresh-not-ready' else 'other'
    _write(credentials,vectors.compact(record).encode())
    return 'refresh' if name=='refresh-not-ready' else 'access'


def json_bytes_file(path):
    # This path is a canonical file we just created with the real login flow.
    data=protocol.owned_bytes(path.resolve(),256*1024)
    return json.loads(data,object_pairs_hook=protocol._pairs)


def corruption(r,name):
    case_id='store-'+name; actors=[]
    for adapter in ('cpp','rust'):
        with fixture_root() as root:
            steps=[]
            if name!='root-nul': steps.append(_step(r,root,adapter,case_id+'-'+adapter+'-seed','login'))
            _write(root/'sentinel',b'owned sentinel\n')
            sentinel=os.open(root/'sentinel',os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
            try:
                action=_mutate(root,name)
                prefix=case_id+'-'+adapter
                before=snapshot(r.directory,prefix+'-before',root)
                request={'op':'store','root':str(root)+('\0alias' if name=='root-nul' else ''),'action':action,'now':vectors.NOW}
                label=prefix+'-probe'; r.call(label,adapter,request,cwd=root)
                steps.append({'label':label,'adapter':adapter,'command':label,'snapshot':snapshot(r.directory,label,root)})
                preserved=os.pread(sentinel,1024,0)
                path=r.directory/(prefix+'-sentinel.stdout'); path.write_bytes(preserved)
                actors.append({'adapter':adapter,'root':str(root),'steps':steps,'before':before,
                               'sentinel':{'file':path.name,'size':len(preserved),'sha256':hashlib.sha256(preserved).hexdigest()}})
            finally:
                os.close(sentinel)
    return {'id':case_id,'actors':actors}


def validate_corruption(r,row,name):
    check([a.get('adapter') for a in row['actors']]==['cpp','rust'],'store guard actor inventory')
    actions={'directory-mode':'status','root-nul':'status','credentials-mode':'status','credentials-empty':'status',
             'credentials-oversize':'status','credentials-json':'status','pending-cleanup-control':'status',
             'refresh-not-ready':'refresh'}
    for actor in row['actors']:
        adapter=actor['adapter']; prefix=row['id']+'-'+adapter; steps=actor['steps']
        seed=name!='root-nul'; check(len(steps)==1+int(seed),'store guard step inventory')
        if seed:
            check(steps[0]['label']==prefix+'-seed' and steps[0]['adapter']==adapter,'store guard seed identity')
            actual,state=_view(r,steps[0],actor['root'],'login'); response_matches(actual,state,vectors.NOW,'login')
        family,_,kind=name.partition('-')
        action=actions.get(name,'logout' if family=='marker' else 'refresh' if family=='log' else 'status' if family in ('credentials','lock','pending') else 'access')
        step=steps[-1]; check(step['label']==prefix+'-probe' and step['adapter']==adapter,'store guard probe identity')
        request={'op':'store','root':actor['root']+('\0alias' if name=='root-nul' else ''),'action':action,'now':vectors.NOW}
        actual=r.read_call(step['command'],step['label'],adapter,request,cwd=actor['root'])
        before=read_snapshot(r.directory,actor['before'],prefix+'-before')
        after=read_snapshot(r.directory,step['snapshot'],prefix+'-probe')
        check(protocol.retained_bytes(r.directory,actor.get('sentinel'),prefix+'-sentinel.stdout',1024)==b'owned sentinel\n','fixture escaped through a link')
        if name=='pending-cleanup-control':
            response_matches(actual,after,vectors.NOW,'status'); check(len(before['pending'])==1 and not after['pending'],'stale pending not removed')
        else:
            check(isinstance(actual,dict) and actual.get('accepted') is False and 'adapter_error' not in actual,'unsafe store admitted')
        if family=='log':
            check(after['records']['credentials.json']['refresh_pending'] is True,'failed fake exchange lost pending intent')
        else:
            check(after['events']==before['events'],'invalid store performed network')
            canonical_equal(before,after)
        if name=='root-nul': check(after['raw']['entries']==before['raw']['entries'],'NUL path changed fixture root')


def race(r):
    case_id='cross-process-cpp-rust-refresh'
    with fixture_root() as root:
        seed=_step(r,root,'cpp',case_id+'-seed','login')
        before=snapshot(r.directory,case_id+'-before',root)
        now=vectors.NOW+3550
        def call(adapter):
            label=case_id+'-'+adapter
            r.call(label,adapter,{'op':'store','root':str(root),'action':'access','now':now},cwd=root)
            return {'label':label,'adapter':adapter,'command':label}
        with ThreadPoolExecutor(max_workers=2) as pool:
            jobs=[pool.submit(call,adapter) for adapter in ('cpp','rust')]
            actors=[job.result() for job in jobs]
        return {'id':case_id,'root':str(root),'seed':seed,'before':before,'actors':actors,
                'after':snapshot(r.directory,case_id+'-after',root)}


def validate_race(r,row):
    case_id=row['id']; seed=row['seed']
    check(seed['label']==case_id+'-seed' and seed['adapter']=='cpp','cross process seed')
    actual,initial=_view(r,seed,row['root'],'login'); response_matches(actual,initial,vectors.NOW,'login')
    before=read_snapshot(r.directory,row['before'],case_id+'-before'); canonical_equal(initial,before)
    final=read_snapshot(r.directory,row['after'],case_id+'-after'); now=vectors.NOW+3550
    check([a.get('adapter') for a in row['actors']]==['cpp','rust'],'cross process inventory')
    for actor in row['actors']:
        check(actor['label']==case_id+'-'+actor['adapter'],'cross process label')
        actual=r.read_call(actor['command'],actor['label'],actor['adapter'],{'op':'store','root':row['root'],'action':'access','now':now},cwd=row['root'])
        response_matches(actual,final,now,'access')
    record=final['records']['credentials.json']; original=before['records']['credentials.json']
    check(final['events']==['code','refresh'] and record['refresh_pending'] is False and record['session']==original['session']
          and account(record)==account(original) and uint(record['expires_at'])==now+3600,'C++/Rust raced a rotating token')


def framing(r,kind):
    case_id='adapter-'+kind; actors=[]
    for adapter in ('cpp','rust'):
        with fixture_root() as root:
            request={'op':'store','root':str(root),'action':'login','now':vectors.NOW}
            if kind=='oversize': request['padding']='x'*(4*1024*1024)
            payload=vectors.compact(request).encode()+(b'\n' if kind=='oversize' else b'')
            label=case_id+'-'+adapter; before=snapshot(r.directory,label+'-before',root)
            r.call(label,adapter,None,cwd=root,raw_input=payload)
            actors.append({'adapter':adapter,'root':str(root),'label':label,'command':label,'before':before,
                           'after':snapshot(r.directory,label+'-after',root)})
    return {'id':case_id,'actors':actors}


def validate_framing(r,row,kind):
    check([a.get('adapter') for a in row['actors']]==['cpp','rust'],'framing actor inventory')
    for actor in row['actors']:
        label=row['id']+'-'+actor['adapter']; check(actor['label']==label,'framing label')
        request={'op':'store','root':actor['root'],'action':'login','now':vectors.NOW}
        if kind=='oversize': request['padding']='x'*(4*1024*1024)
        payload=vectors.compact(request).encode()+(b'\n' if kind=='oversize' else b'')
        r.read_call(actor['command'],label,actor['adapter'],None,exit_code=2,cwd=actor['root'],raw_input=payload)
        before=read_snapshot(r.directory,actor['before'],label+'-before'); after=read_snapshot(r.directory,actor['after'],label+'-after')
        check(before['raw']['entries']==after['raw']['entries'] and before['events']==after['events']==[],'rejected framing executed an auth operation')


def case_ids(mode):
    result=['compat-cpp-to-rust','compat-rust-to-cpp']
    if mode=='fault':
        result += ['crash-'+row[0] for row in CRASH]
        result += ['store-'+name for name in CORRUPT]+['cross-process-cpp-rust-refresh','adapter-oversize','adapter-unterminated']
    return result


def capture(r,mode):
    rows=[compat(r,'cpp','rust'),compat(r,'rust','cpp')]
    if mode=='fault':
        rows.extend(crash_case(r,spec) for spec in CRASH)
        rows.extend(corruption(r,name) for name in CORRUPT)
        rows.extend([race(r),framing(r,'oversize'),framing(r,'unterminated')])
    return rows


def validate(r,row):
    if row['id'] in ('compat-cpp-to-rust','compat-rust-to-cpp'): return validate_compat(r,row)
    if row['id']=='cross-process-cpp-rust-refresh': return validate_race(r,row)
    if row['id'] in ('adapter-oversize','adapter-unterminated'): return validate_framing(r,row,row['id'][8:])
    if row['id'].startswith('store-') and row['id'][6:] in CORRUPT: return validate_corruption(r,row,row['id'][6:])
    spec=next((s for s in CRASH if row['id']=='crash-'+s[0]),None)
    check(spec is not None,'unknown store case')
    return validate_crash(r,row,spec)
