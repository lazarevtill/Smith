#!/usr/bin/env python3
"""Explicit connected ASR qualification for generated WAV/AIFF/AAC/ALAC only.
No downloads, microphone, playback, user files, remote providers or model discovery.
"""
import argparse,base64,hashlib,io,json,os,struct,tempfile,time,uuid,wave
from pathlib import Path
from local_transcription_acceptance import MODEL_SHA256,digest,owned_transcriber,repeated_generated_wave
from profile_fixture import EncryptedApp
SPEECH_SHA256='4dc819f193a02aea2042cba9bba019916907f465cd18d24834931ff0b8948c01'
def sources(original):
    with wave.open(io.BytesIO(original),'rb') as f: samples=f.readframes(f.getnframes())
    out=io.BytesIO()
    with wave.open(out,'wb') as f:
        f.setnchannels(2);f.setsampwidth(2);f.setframerate(16000);f.writeframes(b''.join(samples[i:i+2]*2 for i in range(0,len(samples),2)))
    swapped=b''.join(samples[i:i+2][::-1] for i in range(0,len(samples),2))
    comm=struct.pack('>HIH',1,len(samples)//2,16)+bytes.fromhex('400cfa00000000000000')
    body=b'AIFFCOMM'+struct.pack('>I',18)+comm+b'SSND'+struct.pack('>I',len(samples)+8)+bytes(8)+swapped
    aiff=b'FORM'+struct.pack('>I',len(body))+body
    fixtures=Path(__file__).parent/'fixtures'
    provenance=json.loads((fixtures/'normalization-provenance.json').read_text())
    compressed=[]
    for file in provenance['formats']:
        p=fixtures/file['name'];assert digest(p)==file['sha256'] and p.stat().st_size==file['byte_len']
        compressed.append((p.name,p.read_bytes(),'aac_m4a' if 'aac' in p.name else 'alac_m4a'))
    return [('generated-stereo.wav',out.getvalue(),'pcm_wav'),('generated.aiff',aiff,'pcm_aiff'),*compressed]
def main():
    parser=argparse.ArgumentParser()
    for name in ['binary','executable','model','generated-wav','report']:parser.add_argument('--'+name,required=True)
    parser.add_argument('--execute-approved-generated-speech',action='store_true');args=parser.parse_args()
    assert args.execute_approved_generated_speech
    binary=Path(args.binary).resolve(strict=True);exe=Path(args.executable).resolve(strict=True);model=Path(args.model).resolve(strict=True);audio=Path(args.generated_wav).resolve(strict=True)
    assert digest(model)==MODEL_SHA256 and model.stat().st_size==77704715
    assert digest(audio)==SPEECH_SHA256
    started=time.monotonic();checks=[];evidence=[];saved=[];normalizer=None
    with tempfile.TemporaryDirectory(prefix='smith-normalized-asr-') as temporary:
        app=EncryptedApp(str(binary),Path(temporary).resolve()/'profile')
        try:
            app.start(True)
            review=app.api('/media/transcription/review',{'executable_path':str(exe),'model_path':str(model)})
            normalizer=review['normalizer'];assert normalizer['adapter']=='apple_afconvert'
            assert normalizer['executable']['sha256']==digest(Path('/usr/bin/afconvert'))
            assert review['allowed_languages']==['en'];checks.append('read-only-selected-model-and-fixed-system-converter-hash-review')
            for name,data,format in sources(audio.read_bytes()):
                rid=str(uuid.uuid4());payload={'review_id':review['review_id'],'request_id':rid,'name':name,'audio_base64':base64.b64encode(data).decode(),'language':'en'}
                admitted=app.api('/media/transcription/start-file',payload);assert admitted['source']['sha256']==hashlib.sha256(data).hexdigest()
                assert app.api('/media/transcription/start-file',payload)['request_id']==rid
                job=app.poll(lambda:app.api('/media/transcription/jobs/'+rid),lambda j:j['state'] not in ['running','cancelling'],seconds=205)
                assert job['state']=='completed',job.get('error');result=job['result'];assert 'robot' in result['text'].lower() and 'microphone' in result['text'].lower(),result['text']
                normalized=result['normalization'];assert normalized['source_format']==format and normalized['truncated'] is False
                assert normalized['executable']==normalizer['executable'] and normalized['normalized']['byte_len']<=2097152
                receipt=app.api('/media/transcription/jobs/'+rid+'/save',{'request_id':str(uuid.uuid4()),'title':'Generated '+format+' qualification','format':'vtt'})
                aid=receipt['artifact']['id'];stored=app.api('/media/artifacts/'+aid+'/original',raw=True);canonical=json.loads(stored)
                assert canonical['source']==job['source'] and canonical['transcript']==result and canonical['original_audio_retained'] is False
                saved.append((aid,stored));evidence.append({'format':format,'source':job['source'],'normalization':normalized,'text':result['text'],'segments':result['segments'],'artifact_sha256':hashlib.sha256(stored).hexdigest()})
                checks.append('real-decoder-and-tiny-en-'+format+'-exact-encrypted-artifact')
            bad=bytearray((Path(__file__).parent/'fixtures/normalization-aac.m4a').read_bytes());i=bad.index(b'url ');bad[i+7]=0
            refused=app.api('/media/transcription/start-file',{**payload,'request_id':str(uuid.uuid4()),'name':'external.m4a','audio_base64':base64.b64encode(bad).decode()},want=415);assert refused['code']=='transcription_audio_unsupported'
            checks.append('external-reference-refused-before-decoding')
            rid=str(uuid.uuid4());app.api('/media/transcription/start-file',{'review_id':review['review_id'],'request_id':rid,'name':'generated-cancel.wav','audio_base64':base64.b64encode(repeated_generated_wave(audio.read_bytes())).decode(),'language':'en'})
            pid,scratch=app.poll(lambda:owned_transcriber(app.process.pid),lambda child:child is not None,seconds=10)
            app.api('/media/transcription/jobs/'+rid+'/cancel',{});job=app.poll(lambda:app.api('/media/transcription/jobs/'+rid),lambda j:j['state'] not in ['running','cancelling'],seconds=10)
            assert job['state']=='cancelled' and not scratch.exists()
            try:os.kill(pid,0)
            except ProcessLookupError:pass
            else:raise AssertionError('Owned transcriber remained after cancellation')
            checks.append('normalized-job-observed-transcriber-cancel-reap-and-scratch-cleanup')
            app.stop();app.start()
            for aid,stored in saved:assert app.api('/media/artifacts/'+aid+'/original',raw=True)==stored
            app.api('/media/transcription/jobs/'+rid,want=409);app.stop();checks.append('restart-preserves-all-canonical-artifact-bytes-and-revokes-jobs')
            for p in app.root.rglob('*'):
                if p.is_file():
                    content=p.read_bytes()
                    for item in evidence:assert item['text'].encode() not in content
            checks.append('disposable-profile-has-no-plaintext-transcript')
        finally:app.stop()
    report={'schema':'smith.audio-normalization-qualification.v1','real_asr':True,'binary_sha256':digest(binary),'executable_sha256':digest(exe),'model_sha256':MODEL_SHA256,'speech_source':'approved task-generated offline speech, no microphone','normalizer':normalizer,'groups':len(checks),'passed':checks,'cases':evidence,'elapsed_seconds':round(time.monotonic()-started,3),'scope':'Finite generated English WAV/AIFF/AAC-LC/ALAC fixtures only. No general codec/audio-quality or microphone claim.'}
    path=Path(args.report);path.parent.mkdir(parents=True,exist_ok=True);path.write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n');print(json.dumps({'groups':len(checks),'real_asr':True,'report':path.name}))
if __name__=='__main__':main()
