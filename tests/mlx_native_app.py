"""Owned normal-app HTTP qualification using only generated tiny MLX weights."""
import argparse,hashlib,json,os,shutil,sys,tempfile,time
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'tests'))
from profile_fixture import EncryptedApp
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary',required=True,type=Path)
parser.add_argument('--binary-sha256',required=True)
parser.add_argument('--runtime-bundle',required=True,type=Path)
parser.add_argument('--model-directory',required=True,type=Path)
parser.add_argument('--report',required=True,type=Path)
args=parser.parse_args()
if sys.platform!='darwin':raise SystemExit('Native Metal qualification requires macOS.')
binary=args.binary.resolve();bundle=args.runtime_bundle.resolve();model=args.model_directory.resolve();out=args.report.resolve()
EXPECTED_WORKER='9053f3cdba8385d6f0e9bcb784a27d643ea16b9b4e6fd5901c0fa84900c6470b'
EXPECTED_PACKAGE='a88bf93132f30b314ec84a0742acc5d00b1f6ef931006312a60d860405c815f8'
checks=[];app=None

def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def files(p):return {str(x.relative_to(p)):sha(x) for x in p.rglob('*') if x.is_file()}
def expected():return app.target()
def status():return app.api('/mlx/status')
def settle(operation):
    s=app.poll(status,lambda s:s['operation'] and s['operation']['id']==operation and s['operation']['status']!='running',seconds=190)
    return s

def prepare(capability,tokens=16):
    accepted=app.api('/mlx/prepare',{'worker':{'path':str(selected_worker),'source':'locally built Swift worker with owned resources','source_revision':'synthetic-http-qualification-v1'},'model_directory':str(selected_model),'model_id':'synthetic-constant-a','capability':capability,'sampling':{'max_tokens':tokens,'temperature':0.0,'top_p':1.0,'seed':0},'settings_revision':status()['settings']['revision'],'expected':expected()})
    s=settle(accepted['operation_id']);assert s['operation']['status']=='succeeded',s['operation'];assert s['runtime'] is None
    return s['prepared']
def load(p):
    review={'review_id':p['review_id']} if 'review_id' in p else {}
    accepted=app.api('/mlx/load',{'plan_id':p['plan']['id'],**review,'settings_revision':p['settings_revision'],'confirm':True,'expected':expected()});s=settle(accepted['operation_id']);assert s['operation']['status']=='succeeded',s['operation'];assert s['runtime']['phase']=='ready';return s

def unload():
    s=status();accepted=app.api('/mlx/unload',{'session_id':s['runtime']['session_id'],'runtime_revision':s['runtime']['revision'],'expected':expected()});s=settle(accepted['operation_id']);assert s['operation']['status']=='succeeded';assert s['runtime'] is None

def chat(content):
    c=app.api('/chats',{'title':'Synthetic MLX qualification',**expected()});accepted=app.api('/chats/'+c['id']+'/messages',{'content':content,'version':c['version'],**expected()});return c,accepted

def assistant(chat_id):
    return app.api('/chats/'+chat_id)['active_messages'][-1]

assert sha(binary)==args.binary_sha256,'The immutable app binary differs from the reviewed SHA-256.'
assert sha(bundle/'smith-mlx-worker')==EXPECTED_WORKER,'Use the reviewed synthetic-qualification worker.'
# Set only this test process and its owned children to the conventional umask.
# The supervisor must explicitly create its scratch0700; umask077 would hide the regression.
os.umask(0o022)
with tempfile.TemporaryDirectory(prefix='smith-mlx-app-qualification-') as temporary:
    root=Path(temporary).resolve();selected_bundle=root/'runtime';shutil.copytree(bundle,selected_bundle);selected_worker=selected_bundle/'smith-mlx-worker';selected_model=root/'model';shutil.copytree(model,selected_model)
    source_before=files(selected_bundle),files(selected_model)
    try:
        app=EncryptedApp(binary,root/'profile').start();assert app.status()['automatic'];assert status()['runtime'] is None and status()['settings']['revision']==0
        checks.append('normal-automatic-profile-opens-with-no-login-and-no-runtime')
        p=prepare('chat');assert p['plan']['model']['package_sha256']==EXPECTED_PACKAGE;assert len(p['plan']['runtime_files'])==7
        review={'review_id':p['review_id']} if 'review_id' in p else {}
        refusals=[({'plan_id':'0'*64},409),({'confirm':False},400),({'settings_revision':1},409)]
        if review:refusals.append(({'review_id':'0'*64},409))
        for change,want in refusals:
            body={'plan_id':p['plan']['id'],**review,'settings_revision':0,'confirm':True,'expected':expected()};body.update(change);app.api('/mlx/load',body,want=want)
        assert status()['runtime'] is None
        checks.append('hash-only-plan-includes-worker-six-resources-and-package-exact-rejections-launch-nothing')
        s=load(p);assert s['runtime']['backend']=='mlx_swift_metal_only';assert s['runtime']['manifest']['template']=='simple-chatml-v1';assert s['settings']['revision']==1
        assert app.api('/models?revision='+expected()['revision'])['models'][0]['id']=='synthetic-constant-a'
        c,accepted=chat('Synthetic deterministic local test.')
        done=app.poll(lambda:assistant(c['id']),lambda m:m['status']!='streaming',seconds=30)
        assert done['status']=='limited',done;assert done['content']=='A'*16,done;assert done['transport_incomplete'] is False
        captured_provenance=done.get('model_provenance')
        if review:
            assert captured_provenance['thinking_mode']=='model_default'
            assert captured_provenance['review_id']==p['review_id'] and captured_provenance['settings_revision']==1
            assert captured_provenance['worker_sha256']==EXPECTED_WORKER and captured_provenance['package_sha256']==EXPECTED_PACKAGE
        checks.append('native-metal-chat-through-normal-router-persists-exact-deterministic-length-result')
        s=status();app.api('/mlx/completions',{'session_id':s['runtime']['session_id'],'runtime_revision':s['runtime']['revision'],'prompt':'no implicit raw bypass','sampling':{'max_tokens':8,'temperature':0.0,'top_p':1.0},'confirm':True,'expected':expected()},want=400)
        unload();assert status()['settings']['revision']==1
        p=prepare('raw_completion');s=load(p);assert s['settings']['revision']==2
        # A raw-only selected mode cannot enter ordinary chat even when its package admits a template.
        c2=app.api('/chats',{'title':'Synthetic raw isolation',**expected()});app.api('/chats/'+c2['id']+'/messages',{'content':'must not send as chat','version':c2['version'],**expected()},want=409)
        s=status();accepted=app.api('/mlx/completions',{'session_id':s['runtime']['session_id'],'runtime_revision':s['runtime']['revision'],'prompt':'Exact synthetic raw input','sampling':{'max_tokens':8,'temperature':0.0,'top_p':1.0,'seed':0},'confirm':True,'expected':expected()});s=settle(accepted['operation_id']);assert s['operation']['status']=='succeeded',s['operation'];assert s['operation']['completion']['text']=='A'*8;assert not s['operation']['persisted'];assert not app.api('/chats/'+c2['id'])['messages']
        checks.append('explicit-raw-selection-cannot-chat-and-raw-result-never-fabricates-chat-rows')
        s=status();accepted=app.api('/mlx/completions',{'session_id':s['runtime']['session_id'],'runtime_revision':s['runtime']['revision'],'prompt':'Cancel this synthetic raw response','sampling':{'max_tokens':2048,'temperature':0.0,'top_p':1.0,'seed':0},'confirm':True,'expected':expected()})
        progressing=app.poll(status,lambda s:s['operation']['id']==accepted['operation_id'] and (s['operation'].get('progress') or {}).get('text','')!='',seconds=20);assert progressing['operation']['status']=='running','synthetic generation completed before cancellation witness'
        app.api('/mlx/cancel',{'operation_id':accepted['operation_id'],'expected':expected()});s=settle(accepted['operation_id']);assert s['operation']['status']=='cancelled',s['operation'];assert s['operation']['completion']['incomplete'];assert s['operation']['completion']['text'];assert s['runtime']['spawned']==s['runtime']['reaped'] and s['runtime']['pid'] is None
        checks.append('active-native-cancel-preserves-partial-incomplete-output-and-reaps-owned-worker')
        settings=status()['settings'];app.stop();app.start();assert status()['settings']==settings and status()['runtime'] is None;assert assistant(c['id'])['content']=='A'*16
        assert assistant(c['id']).get('model_provenance')==captured_provenance
        checks.append('restart-retains-encrypted-chat-and-sampling-without-automatic-execution')
        p=prepare('chat',8);s=load(p);c3,accepted=chat('Synthetic reload after restart');done=app.poll(lambda:assistant(c3['id']),lambda m:m['status']!='streaming',seconds=30);assert done['content']=='A'*8 and done['status']=='limited';unload();app.stop()
        assert files(selected_bundle)==source_before[0] and files(selected_model)==source_before[1]
        db=root/'profile/local-workspace/profile.sqlite3';assert not db.read_bytes().startswith(b'SQLite format 3');assert b'Synthetic deterministic local test.' not in db.read_bytes()
        checks.append('fresh-review-reload-after-restart-source-bundles-unchanged-encrypted-storage')
        assert sha(binary)==args.binary_sha256,'The tested app binary changed during qualification.'
        report={'executed_at_utc':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),'execution_umask':'022','passed':True,'synthetic_only':True,'native_metal':True,'app_sha256':sha(binary),'worker_sha256':sha(selected_worker),'runtime_bundle_sha256':p['plan']['runtime_bundle_sha256'],'model_package_sha256':p['plan']['model']['package_sha256'],'groups':len(checks),'checks':checks,'public_models_used':False,'real_profiles_accessed':False}
        out.parent.mkdir(parents=True,exist_ok=True);out.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
    finally:
        if app:app.stop()
