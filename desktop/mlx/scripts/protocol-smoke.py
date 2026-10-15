#!/usr/bin/env python3
"""Exercise the actual native child with synthetic bytes only; never request a valid model load."""
import hashlib,json,os,pathlib,select,shutil,struct,subprocess,sys,tempfile,time
binary=pathlib.Path(sys.argv[1]).resolve()
checks=[]
with tempfile.TemporaryDirectory(prefix='smith-mlx-smoke-') as tmp:
    base=pathlib.Path(tmp).resolve(); scratch=base/'scratch';scratch.mkdir(mode=0o700)
    model=base/'model';model.mkdir()
    config={'model_type':'llama','hidden_size':64,'num_attention_heads':4,'num_hidden_layers':1,'intermediate_size':128,'vocab_size':256,'max_position_embeddings':8192}
    for name,value in [('config.json',config),('tokenizer_config.json',{'tokenizer_class':'PreTrainedTokenizerFast'}),('tokenizer.json',{'model':{'type':'BPE','vocab':{'a':0},'merges':[]}})]:
        (model/name).write_text(json.dumps(value))
    header=json.dumps({'synthetic.weight':{'dtype':'F32','shape':[1],'data_offsets':[0,4]}},separators=(',',':')).encode()
    (model/'model.safetensors').write_bytes(struct.pack('<Q',len(header))+header+b'\0'*4)
    def start(executable=binary):
        return subprocess.Popen([str(executable),'--scratch-root',str(scratch)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,env={'PATH':'/usr/bin:/bin','TMPDIR':str(base)},bufsize=0)
    def event(p):
        line=b'';deadline=time.monotonic()+20
        while not line.endswith(b'\n'):
            assert time.monotonic()<deadline,'native child response timeout'
            ready,_,_=select.select([p.stdout],[],[],0.2)
            if ready:
                byte=os.read(p.stdout.fileno(),1)
                assert byte,'unexpected EOF'
                line+=byte
                assert len(line)<=131073,'oversized response'
        return json.loads(line)
    def request(p,id,op,**fields):
        p.stdin.write((json.dumps({'v':1,'id':id,'op':op,**fields})+'\n').encode());p.stdin.flush()
        events=[]
        while True:
            e=event(p);assert e['id']==id,e;events.append(e)
            if e['event'] in ['done','error']:return events
    p=start()
    try:
        e=request(p,'hello','hello')[-1];assert e['backend']=='mlx-swift-metal' and e['protocol']==1
        checks.append('actual-native-hello-no-model')
        e=request(p,'inspect','inspect',model_root=str(model))[-1]
        assert e['event']=='done',e
        manifest=e['manifest'];assert manifest['model_type']=='llama' and len(manifest['files'])==4
        assert e['capabilities']==['raw_completion']
        for f in manifest['files']:assert hashlib.sha256((model/f['name']).read_bytes()).hexdigest()==f['sha256']
        checks.append('synthetic-safetensors-inspection-manifest-hashes')
        e=request(p,'wrong-hash','load',model_root=str(model),expected_package_sha256='0'*64)[-1]
        assert e['code']=='package_changed',e
        checks.append('changed-package-rejected-before-gpu-loader')
        e=request(p,'no-model','generate',mode='raw',prompt='synthetic',max_tokens=8,temperature=0,top_p=1)[-1]
        assert e['code']=='no_model',e
        checks.append('no-fallback-generation-without-model')
        e=request(p,'cancel-unknown','cancel',target_id='absent')[-1];assert e['code']=='not_active'
        checks.append('exact-cancel-target')
        e=request(p,'shutdown','shutdown')[-1];assert e['event']=='done';p.stdin.close();assert p.wait(timeout=10)==0
        assert list(scratch.iterdir())==[]
        checks.append('owned-snapshot-cleanup-and-shutdown')
    finally:
        if p.poll() is None:p.kill();p.wait()
    isolated=base/'isolated-worker';isolated.mkdir();shutil.copy2(binary,isolated/'smith-mlx-worker')
    p=start(isolated/'smith-mlx-worker')
    try:
        e=request(p,'missing-resources','load',model_root=str(model),expected_package_sha256=manifest['package_sha256'])[-1]
        assert e['code']=='invalid_runtime_resources',e
        assert request(p,'close','shutdown')[-1]['event']=='done';p.stdin.close();assert p.wait(timeout=10)==0
        assert list(scratch.iterdir())==[]
        checks.append('copied-child-refuses-missing-colocated-runtime-resources-before-gpu')
    finally:
        if p.poll() is None:p.kill();p.wait()
    for label,data,code in [('partial-eof',b'{"v":1', 'truncated_frame'),('oversized-frame',b' '*131073+b'\n','frame_too_large'),('duplicate-key',b'{"v":1,"v":1,"id":"x","op":"hello"}\n','duplicate_json_key')]:
        p=start()
        try:
            p.stdin.write(data);p.stdin.close();e=event(p);assert e['event']=='error' and e['id'] is None and e['code']==code,e;assert p.wait(timeout=10)==0
            checks.append(label)
        finally:
            if p.poll() is None:p.kill();p.wait()
report={'schema':1,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'native_process':True,'gpu_inference':False,'models_downloaded':False,'checks':checks,'passed':len(checks)}
print(json.dumps(report,indent=2))
