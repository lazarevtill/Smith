"""Disposable normal encrypted app fixture shared by integration checks."""
import json, os, signal, socket, subprocess, time, urllib.error, urllib.request
from pathlib import Path

class EncryptedApp:
    password='Synthetic fixture key, not a real user secret'
    def __init__(self,binary,root):
        self.binary=str(Path(binary).resolve());self.root=Path(root).resolve();self.session=None;self.process=None
        with socket.socket() as s:s.bind(('127.0.0.1',0));self.port=s.getsockname()[1]
        self.base=f'http://127.0.0.1:{self.port}'
    def api(self,path,data=None,method=None,want=200,raw=False,content_type='application/json'):
        headers={'Origin':self.base,'Content-Type':content_type}
        if self.session:headers['X-Smith-Profile-Session']=self.session
        if data is not None and not isinstance(data,bytes):data=json.dumps(data).encode()
        request=urllib.request.Request(self.base+'/api'+path,data=data,method=method,headers=headers)
        try:
            with urllib.request.urlopen(request,timeout=20) as response:status,body=response.status,response.read()
        except urllib.error.HTTPError as error:status,body=error.code,error.read()
        assert status==want,(path,status,body)
        return body if raw else json.loads(body)
    def poll(self,read,predicate,seconds=20):
        end=time.monotonic()+seconds
        while True:
            value=read()
            if predicate(value):return value
            assert time.monotonic()<end,value
            time.sleep(.02)
    def status(self):return self.api('/profile/status')
    def profile_operation(self,kind,body):
        accepted=self.api('/profile/'+kind,{'revision':self.status()['revision'],**body},want=202)
        value=self.poll(self.status,lambda s:s['operation'] and s['operation']['id']==accepted['operation_id'] and s['operation']['status']!='running')
        assert value['operation']['status']=='succeeded',value
        return value
    def unlock(self,create=False):
        value=self.profile_operation('create' if create else 'unlock',{'passphrase':self.password});self.session=value['session'];return value
    def lock(self):
        value=self.profile_operation('lock',{'session':self.session});self.session=None;assert value['state']=='locked';return value
    def start(self,create=False):
        env={**os.environ,'SMITH_NEXT_DATA_DIR':str(self.root),'SMITH_NEXT_PORT':str(self.port)};env.pop('SMITH_NEXT_SYNTHETIC_PROFILE',None);env.pop('SMITH_NEXT_MANUAL_PROFILE',None)
        self.process=subprocess.Popen([self.binary],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
        for _ in range(200):
            if self.process.poll() is not None:raise AssertionError(self.process.stdout.read())
            try:self.status();break
            except OSError:time.sleep(.025)
        else:raise AssertionError('owned app startup timeout')
        state=self.status()
        if state.get('automatic'):
            assert state['state']=='unlocked';self.session=state['session']
        else:self.unlock(create)
        return self
    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
            try:self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:self.process.kill();self.process.wait();raise AssertionError('owned app shutdown timed out')
            assert self.process.returncode==0,self.process.stdout.read()
        self.session=None
    def target(self):return {key:self.api('/config')[key] for key in ('revision','provider','endpoint')}
