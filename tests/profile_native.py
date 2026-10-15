#!/usr/bin/env python3
"""Owned fixture native process is reaped before encrypted application close completes."""
import argparse,hashlib,json,os,subprocess,tempfile,time
from pathlib import Path
from profile_fixture import EncryptedApp

def main():
    root=Path(__file__).resolve().parents[1];parser=argparse.ArgumentParser();parser.add_argument('--binary',default=str(root/'target/debug/smith-next'));args=parser.parse_args();checks=[]
    with tempfile.TemporaryDirectory(prefix='smith-keyed-native-') as tmp:
        tmp=Path(tmp).resolve();fixture=tmp/'owned-llama-fixture'
        subprocess.run(['rustc','--edition=2021',str(root/'crates/smith-managed-runtime/tests/fixtures/llama_compatible.rs'),'-o',str(fixture)],check=True)
        app=EncryptedApp(args.binary,tmp/'profile')
        def artifact(path):return {'path':str(path),'sha256':hashlib.sha256(path.read_bytes()).hexdigest(),'source':'owned synthetic encrypted-profile test','source_revision':'fixture-v1'}
        def prepare(mode):
            model=tmp/(mode+'.gguf');model.write_bytes(('GGUF'+mode).encode())
            accepted=app.api('/native/plans',{**app.target(),'executable':artifact(fixture),'model':artifact(model),'model_id':mode,'execution_kind':'compatible_fixture','context_tokens':128,'threads':1,'startup_seconds':30,'generation_seconds':30})
            state=app.poll(lambda:app.api('/native/status'),lambda s:s['operation']['id']==accepted['operation_id'] and s['operation']['status']!='running')
            assert state['operation']['status']=='succeeded',state
            return state['prepared']
        def load(plan):return app.api('/native/load',{**app.target(),'plan_id':plan['plan']['id'],'runtime_revision':plan['runtime_revision'],'confirm':True})
        def gone(pid):
            try:os.kill(pid,0);return False
            except ProcessLookupError:return True
        try:
            app.start(True);app.api('/config',{'provider':'managed','endpoint':'','model':''},'PUT');plan=prepare('generation_stall');accepted=load(plan)
            ready=app.poll(lambda:app.api('/native/status'),lambda s:s['operation']['id']==accepted['operation_id'] and s['operation']['status']=='succeeded');pid=ready['runtime']['pid'];assert pid
            chat=app.api('/chats',{'title':'Encrypted native cancellation',**app.target()});route='/chats/'+chat['id']
            run=app.api(route+'/messages',{'version':0,'content':'Synthetic owned fixture prompt',**app.target()})
            app.poll(lambda:app.api('/native/status'),lambda s:s['runtime']['active_leases']==1)
            app.stop();assert gone(pid),'application close completed before owned native process was reaped'
            app.start();assert app.api('/native/status')['runtime'] is None
            message=next(m for m in app.api(route)['messages'] if m['id']==run['message_id']);assert message['status']=='cancelled',message
            checks.append('active-native-generation-cancelled-persisted-and-owned-child-reaped-before-keyed-close')
            plan=prepare('health_stall');load(plan)
            loading=app.poll(lambda:app.api('/native/status'),lambda s:s['runtime'] and s['runtime']['pid']);pid=loading['runtime']['pid']
            app.stop();assert gone(pid),'shutdown retained owned loading child'
            app.start();assert app.api('/native/status')['runtime'] is None;app.stop()
            checks.append('shutdown-during-native-load-drains-tracked-operation-and-restart-never-autoloads')
        finally:app.stop()
    print(json.dumps({'groups':len(checks),'passed':checks,'native':'owned compatible fixture only; no real model'},indent=2))
if __name__=='__main__':main()
