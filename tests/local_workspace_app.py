"""Default local workspace acceptance; no account/password startup, synthetic only."""
import hashlib,json,os,re,secrets,signal,subprocess,sys,tempfile,time,socket
from pathlib import Path
from profile_fixture import EncryptedApp
ROOT=Path(__file__).resolve().parent.parent
CURRENT_SCHEMA=int(re.search(r'pub const SCHEMA_VERSION: i64 = (\d+);',(ROOT/'crates/smith-profile-storage/src/lib.rs').read_text()).group(1))
class Host(EncryptedApp):
    def start(self):
        env={**os.environ,'SMITH_NEXT_DATA_DIR':str(self.root),'SMITH_NEXT_PORT':str(self.port)};env.pop('SMITH_NEXT_SYNTHETIC_PROFILE',None);env.pop('SMITH_NEXT_MANUAL_PROFILE',None)
        self.process=subprocess.Popen([self.binary],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
        for _ in range(300):
            if self.process.poll() is not None:raise AssertionError('Owned automatic app exited during startup')
            try:
                state=self.status();assert state['automatic'] and state['state']=='unlocked';self.session=state['session'];return self
            except OSError:time.sleep(.025)
        raise AssertionError('Owned automatic app startup deadline')
    def operation(self,kind,body,outcome='succeeded'):
        accepted=self.api('/profile/'+kind,{'revision':self.status()['revision'],**body},want=202)
        state=self.poll(self.status,lambda s:s['operation'] and s['operation']['id']==accepted['operation_id'] and s['operation']['status']!='running',seconds=30)
        assert state['operation']['status']==outcome,(kind,state['operation'].get('error'));assert state['state']=='unlocked';self.session=state['session'];return state

def inventory(root):return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in root.rglob('*') if p.is_file()}
def main():
    expected_schema=int(sys.argv[2]) if len(sys.argv)>2 else CURRENT_SCHEMA
    binary=Path(sys.argv[1] if len(sys.argv)>1 else ROOT/'target/debug/smith-next').resolve();fixture=ROOT/'crates/smith-profile-storage/target/debug/examples/legacy_fixture';master=secrets.token_hex(32);checks=[];host=None;temporary=tempfile.TemporaryDirectory(prefix='smith-local-acceptance-');tmp=Path(temporary.name).resolve()
    try:
        hostroot=tmp/'host';hostroot.mkdir();(hostroot/'profile.sqlite3').write_bytes(b'Untouched synthetic manual profile');(hostroot/'profile-selection.json').write_bytes(b'Untouched synthetic manual selection');old={p.name:p.read_bytes() for p in hostroot.iterdir()}
        host=Host(binary,hostroot).start();assert host.status()['retained_manual_profile'];assert all((hostroot/name).read_bytes()==value for name,value in old.items());local=hostroot/'local-workspace';assert (local/'managed.key').stat().st_mode&0o777==0o600 and local.stat().st_mode&0o777==0o700
        key_before=(local/'managed.key').read_bytes();host.api('/drafts/new',{'version':0,'text':'Existing local workspace data stays','attachments':[],'media':[]},'PUT');host.stop();host.start();assert host.api('/drafts/new')['text']=='Existing local workspace data stays';assert (local/'managed.key').read_bytes()==key_before
        host.api('/profile/lock',{'revision':host.status()['revision'],'session':host.session},want=404,raw=True)
        checks.append('automatic-create-restart-no-password-stable-private-key-retained-manual-files-and-local-data')
        source=tmp/'original-copy';subprocess.run([fixture,source],input=master+'\n',text=True,capture_output=True,check=True);before=inventory(source);fields={'source_copy':str(source),'managed_copy':str(source/'artifacts'),'offline_copy_attested':True,'confirm':True};key={'format':'master','master_hex':master}
        host.operation('legacy-preflight',{**fields,'original_key':{'format':'master','master_hex':'00'*32}},'failed');assert not (local/'imported').exists();assert inventory(source)==before
        old_operation=host.status().get('operation');payload=json.dumps({'revision':host.status()['revision'],**fields,'original_key':key}).encode()
        with socket.create_connection(('127.0.0.1',host.port),timeout=5) as client:
            client.sendall(('POST /api/profile/legacy-preflight HTTP/1.1\r\nHost: 127.0.0.1:'+str(host.port)+'\r\nOrigin: '+host.base+'\r\nContent-Type: application/json\r\nContent-Length: '+str(len(payload))+'\r\nConnection: close\r\n\r\n').encode()+payload)
            assert b'202' in client.recv(64)
        settled=host.poll(host.status,lambda s:s['operation'] and s['operation'].get('id')!=(old_operation or {}).get('id') and s['operation']['status']!='running',seconds=30);assert settled['operation']['status']=='succeeded' and settled['state']=='unlocked';host.session=settled['session'];assert not (local/'imported').exists()
        checks.append('accepted-preflight-finishes-and-reopens-after-requesting-client-disconnects')
        report=host.operation('legacy-import',{**fields,'original_key':key})['operation']['result'];assert report['target_schema']==expected_schema and host.status()['selection']=='imported';assert inventory(source)==before
        chat=host.api('/chats/session');assert len(chat['messages'])==6 and chat['active_messages'][-1]['id']=='latest';assert host.api('/legacy-import/messages/latest')['reasoning']=='Original private reasoning retained.';assert not host.api('/legacy-import/tasks')['tasks'][0]['resumable']
        blob='/legacy-import/artifacts/11223344-5566-7788-99aa-bbccddeeff00/bytes';assert host.api(blob,raw=True)==b'Synthetic original managed artifact. Exact retained bytes.\n'
        session=host.session;host.session=None;assert host.api(blob+'?profile_session='+session,raw=True)==b'Synthetic original managed artifact. Exact retained bytes.\n';host.api('/legacy-import/tasks?profile_session='+session,want=409);host.session=session
        other=subprocess.run([binary],env={**os.environ,'SMITH_NEXT_DATA_DIR':str(local/'imported'),'SMITH_NEXT_PORT':'0'},capture_output=True,timeout=5);assert other.returncode!=0
        checks.append('original-key-only-import-auto-opens-verified-chat-details-dormant-tasks-and-exact-download')
        host.operation('legacy-rollback',fields);assert host.status()['selection']=='default' and host.api('/drafts/new')['text']=='Existing local workspace data stays';assert (local/'imported'/'profile.sqlite3').exists() and (local/'imported'/'import.db').exists();assert inventory(source)==before;host.stop()
        checks.append('unchanged-import-deactivation-returns-prior-local-data-retains-every-copy')
        host=Host(binary,tmp/'later').start();host.operation('legacy-import',{**fields,'original_key':key});draft=host.api('/drafts/session');host.api('/drafts/session',{'version':draft['version'],'text':'Later imported write','attachments':[],'media':[]},'PUT');host.operation('legacy-rollback',fields,'failed');assert host.status()['selection']=='imported' and host.api('/drafts/session')['text']=='Later imported write';host.stop();host.start();assert host.status()['selection']=='imported' and host.api('/drafts/session')['text']=='Later imported write';host.stop();assert inventory(source)==before
        checks.append('later-committed-write-refuses-deactivation-reopens-import-and-survives-automatic-restart')
        local=host.root/'local-workspace';database_before=(local/'profile.sqlite3').read_bytes();(local/'managed.key').unlink();env={**os.environ,'SMITH_NEXT_DATA_DIR':str(host.root),'SMITH_NEXT_PORT':'0'};result=subprocess.run([binary],env=env,capture_output=True,timeout=10);assert result.returncode!=0 and not(local/'managed.key').exists() and (local/'profile.sqlite3').read_bytes()==database_before
        checks.append('lost-local-key-fails-without-regeneration-or-profile-overwrite')
        print(json.dumps({'passed':True,'syntheticOnly':True,'expected_schema':expected_schema,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'groups':len(checks),'checks':checks},indent=2))
    finally:
        if host:host.stop()
        temporary.cleanup()
if __name__=='__main__':main()
