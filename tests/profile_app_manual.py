#!/usr/bin/env python3
"""Historical manual-profile HTTP lifecycle. Requires synthetic-profile build; never default product UX."""
import argparse, concurrent.futures, hashlib, json, os, re, signal, socket, sqlite3, subprocess, tempfile, threading, time, urllib.error, urllib.request
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class SlowProvider(BaseHTTPRequestHandler):
    started=threading.Event()
    release=threading.Event()
    def log_message(self,*args): pass
    def do_GET(self):
        self.started.set(); self.release.wait(10)
        try:
            self.send_response(200);self.end_headers();self.wfile.write(b'{"data":[{"id":"owned-fixture"}]}')
        except (BrokenPipeError,ConnectionResetError): pass

def main():
    root=Path(__file__).resolve().parents[1]
    current_schema=int(re.search(r'pub const SCHEMA_VERSION: i64 = (\d+);',(root/'crates/smith-profile-storage/src/lib.rs').read_text()).group(1))
    parser=argparse.ArgumentParser();parser.add_argument('--binary',default=str(root/'target/debug/smith-next'));parser.add_argument('--expected-schema',type=int,default=current_schema);args=parser.parse_args()
    binary=str(Path(args.binary).resolve());checks=[]
    with tempfile.TemporaryDirectory(prefix='smith-encrypted-app-') as tmp:
        tmp=Path(tmp).resolve();profile=tmp/'profile';process=None;session=None;password='Synthetic only, never a real key: 🔐 93'
        with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
        base=f'http://127.0.0.1:{port}'
        def api(path,data=None,method=None,want=200,epoch='current',origin=None,raw=False):
            token=session if epoch=='current' else epoch
            headers={'Origin':origin or base,'Content-Type':'application/json'}
            if token is not None:headers['X-Smith-Profile-Session']=token
            req=urllib.request.Request(base+'/api'+path,data=None if data is None else json.dumps(data).encode(),method=method,headers=headers)
            try:
                with urllib.request.urlopen(req,timeout=20) as r:status,body=r.status,r.read()
            except urllib.error.HTTPError as e:status,body=e.code,e.read()
            assert status==want,(path,status,body)
            return body if raw else json.loads(body)
        def poll(fn,pred,seconds=20):
            end=time.monotonic()+seconds
            while True:
                value=fn()
                if pred(value):return value
                assert time.monotonic()<end,value
                time.sleep(.02)
        def state():return api('/profile/status',epoch=None)
        def start(directory):
            nonlocal process,profile,session
            profile=directory;session=None
            env={**os.environ,'SMITH_NEXT_MANUAL_PROFILE':'1','SMITH_NEXT_DATA_DIR':str(profile),'SMITH_NEXT_PORT':str(port)};env.pop('SMITH_NEXT_SYNTHETIC_PROFILE',None)
            process=subprocess.Popen([binary],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            for _ in range(200):
                if process.poll() is not None:raise AssertionError(process.stdout.read())
                try:return state()
                except OSError:time.sleep(.025)
            raise AssertionError('owned app startup timeout')
        def stop():
            nonlocal process
            if process and process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:process.wait(timeout=20)
                except subprocess.TimeoutExpired:process.kill();process.wait();raise AssertionError('shutdown did not close encrypted profile')
                assert process.returncode==0,process.stdout.read()
            process=None
        def operation(route,fields,outcome='succeeded'):
            accepted=api('/profile/'+route,{'revision':state()['revision'],**fields},want=202)
            result=poll(state,lambda s:s['operation'] and s['operation']['id']==accepted['operation_id'] and s['operation']['status']!='running')
            assert result['operation']['status']==outcome,result
            return result
        def unlock(create=False,key=password):
            nonlocal session
            result=operation('create' if create else 'unlock',{'passphrase':key})
            assert result['state']=='unlocked' and result['encrypted'] is True,result
            session=result['session'];return result
        def lock():
            nonlocal session
            old=session;result=operation('lock',{'session':session});assert result['state']=='locked',result
            session=None;return old
        def target():return {k:api('/config')[k] for k in ('revision','provider','endpoint')}
        def copied_source(path):
            path.mkdir();db=sqlite3.connect(path/'smith-next.sqlite3')
            db.executescript("""CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE chats(id TEXT PRIMARY KEY,title TEXT NOT NULL,created_at INTEGER NOT NULL);CREATE TABLE messages(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL,error TEXT,created_at INTEGER NOT NULL,run_id TEXT);CREATE TABLE artifacts(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),name TEXT NOT NULL,content TEXT NOT NULL,created_at INTEGER NOT NULL);PRAGMA user_version=1;INSERT INTO chats VALUES('legacy-chat','Retained copied conversation',1);INSERT INTO messages VALUES('legacy-user','legacy-chat','user','COPIED PRIVATE PROMPT','completed',NULL,1,NULL);INSERT INTO artifacts VALUES('legacy-artifact','legacy-chat','retained.txt','COPIED PRIVATE ARTIFACT',1);""");db.commit();db.close();(path/'source-note.txt').write_text('offline source ancillary retained')
        try:
            s=start(profile);assert s['state']=='needs_creation' and not (profile/'smith-next.sqlite3').exists()
            for p in ['/config','/chats','/drafts/new','/native/status','/agent/workspaces','/media/artifacts','/runs/unknown/events','/artifacts/unknown']:api(p,want=423)
            api('/profile/status',origin='https://untrusted.invalid',want=403,raw=True)
            checks.append('new-profile-locked-all-app-APIs-and-preunlock-origin-guard')
            unlock(True);old=session;api('/config',epoch=None,want=409);api('/config?profile_session='+session,epoch=None,want=409)
            draft=api('/drafts/new',{'version':0,'text':'ENCRYPTED DRAFT PRIVATE CANARY','attachments':[],'media':[]},'PUT');assert draft['version']==1
            api('/drafts/new',{'version':0,'text':'stale','attachments':[]},'PUT',want=409)
            chat=api('/chats',{'title':'Encrypted test chat',**target()});route='/chats/'+chat['id']
            run=api(route+'/messages',{'version':0,'content':'SAVED PRIVATE PROMPT','attachments':[],'media':[],**target()})
            old=lock();api(route,epoch=old,want=423)
            encrypted=profile/'profile.sqlite3';before=hashlib.sha256(encrypted.read_bytes()).hexdigest()
            failed=operation('unlock',{'passphrase':'wrong synthetic key'},'failed');assert failed['state']=='locked';assert hashlib.sha256(encrypted.read_bytes()).hexdigest()==before
            unlock();assert session!=old;api(route,epoch=old,want=409)
            assert api('/drafts/new')['text']=='ENCRYPTED DRAFT PRIVATE CANARY'
            messages=api(route)['messages'];assert messages[-1]['status'] in ['cancelled','completed'];assert messages[0]['content']=='SAVED PRIVATE PROMPT'
            checks.append('encrypted-create-draft-CAS-lock-cancel-wrong-key-preserves-bytes-fresh-session')
            api('/drafts/new',{'version':1},'DELETE');api('/drafts/new',{'version':1,'text':'ABA','attachments':[]},'PUT',want=409)
            lock();stop();assert start(profile)['state']=='locked';unlock();assert api('/drafts/new')['version']==2
            assert api(route)['messages'][0]['content']=='SAVED PRIVATE PROMPT'
            checks.append('reopen-never-autounlocks-and-draft-tombstone-persists')
            # A hanging admitted provider discovery is revoked without repopulating caches.
            server=ThreadingHTTPServer(('127.0.0.1',0),SlowProvider);threading.Thread(target=server.serve_forever,daemon=True).start()
            api('/config',{'provider':'openai','endpoint':f'http://127.0.0.1:{server.server_port}/v1','model':'owned-fixture'},'PUT')
            with concurrent.futures.ThreadPoolExecutor() as pool:
                pending=pool.submit(api,'/connection/test',target(),None,423)
                assert SlowProvider.started.wait(5);started=time.monotonic();lock();assert time.monotonic()-started<5
                pending.result(5);SlowProvider.release.set()
            server.shutdown();server.server_close();unlock();assert api('/config')['tested'] is False
            checks.append('lock-revokes-hanging-provider-request-and-does-not-publish-late-cache')
            lock();stop()
            for file in profile.rglob('*'):
                if file.is_file():
                    data=file.read_bytes()
                    for canary in [password.encode(),'ENCRYPTED DRAFT PRIVATE CANARY'.encode(),b'SAVED PRIVATE PROMPT']:assert canary not in data,(file,'plaintext canary')
            assert not encrypted.read_bytes().startswith(b'SQLite format 3')
            checks.append('normal-profile-disk-contains-no-plaintext-canaries-or-passphrase')
            source=tmp/'offline-copy';copied_source(source);source_before={p.name:p.read_bytes() for p in source.iterdir()}
            migrated=tmp/'migration-host';migrated.mkdir();(migrated/'smith-next.sqlite3').write_bytes(source_before['smith-next.sqlite3'])
            assert start(migrated)['state']=='migration_required';api('/config',want=423)
            fields={'source_copy':str(source),'passphrase':password,'offline_copy_attested':True,'confirm':True}
            result=operation('migrate',fields);assert result['state']=='locked';assert result['operation']['result']['target_schema']==args.expected_schema
            # Another app pointing directly at the selected slot must fail its ownership lock.
            other=subprocess.run([binary],env={**os.environ,'SMITH_NEXT_MANUAL_PROFILE':'1','SMITH_NEXT_DATA_DIR':str(migrated/'migrated'),'SMITH_NEXT_PORT':str(port+1)},capture_output=True,text=True,timeout=5)
            assert other.returncode!=0 and 'already open' in other.stderr,other
            result=operation('rollback',fields);assert result['state']=='migration_required' and result['operation']['result']['deactivated']
            assert (migrated/'migrated'/'profile.sqlite3').exists();assert {p.name:p.read_bytes() for p in source.iterdir()}==source_before
            checks.append('offline-copy-migration-locked-activation-owned-slot-and-nondestructive-rollback')
            stop();second=tmp/'migration-with-write';assert start(second)['state']=='needs_creation';operation('migrate',fields);unlock()
            assert api('/chats/legacy-chat')['messages'][0]['content']=='COPIED PRIVATE PROMPT'
            assert api('/artifacts/legacy-artifact',raw=True)==b'COPIED PRIVATE ARTIFACT'
            api('/drafts/legacy-chat',{'version':0,'text':'later committed data','attachments':[]},'PUT');lock()
            result=operation('rollback',fields,'failed');assert result['state']=='locked';assert (second/'migrated'/'profile.sqlite3').exists()
            unlock();assert api('/drafts/legacy-chat')['text']=='later committed data';lock()
            checks.append('migrated-history-artifact-restored-and-later-write-refuses-rollback')
        finally:stop()
    print(json.dumps({'passed':checks,'groups':len(checks),'expected_schema':args.expected_schema,'binary_sha256':hashlib.sha256(Path(binary).read_bytes()).hexdigest(),'mode':'normal encrypted HTTP; owned synthetic profiles only'},indent=2))
if __name__=='__main__':main()
