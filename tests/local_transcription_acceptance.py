#!/usr/bin/env python3
"""Explicit real-ASR qualification on generated, nonpersonal WAV only.

No downloads, microphone capture, GPU inference, user profile, or remote provider.
The caller supplies the reviewed self-contained CLI and approved tiny.en model.
"""
import argparse, base64, ctypes, hashlib, io, json, os, tempfile, time, uuid, wave as wave_module
from pathlib import Path
from profile_fixture import EncryptedApp

MODEL_SHA256='921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f'
def digest(path):
    h=hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda:f.read(65536),b''):h.update(chunk)
    return h.hexdigest()
def owned_transcriber(parent_pid):
    library=ctypes.CDLL('/usr/lib/libproc.dylib',use_errno=True)
    members=(ctypes.c_int*8)()
    count=library.proc_listchildpids(parent_pid,members,ctypes.sizeof(members))
    assert 0<=count<8,'Owned child listing exceeded qualification bound'
    for pid in list(members)[:count]:
        path=ctypes.create_string_buffer(4096)
        if library.proc_pidpath(pid,path,len(path))>0:
            selected=Path(os.fsdecode(path.value))
            if selected.name=='whisper-cli' and selected.parent.name.startswith('smith-transcription-'):
                return (pid,selected.parent)
    return None

def repeated_generated_wave(value):
    with wave_module.open(io.BytesIO(value),'rb') as source:
        frames=source.readframes(source.getnframes())
    output=io.BytesIO()
    with wave_module.open(output,'wb') as target:
        target.setnchannels(1);target.setsampwidth(2);target.setframerate(16000);target.writeframes(frames*3)
    return output.getvalue()

def main():
    parser=argparse.ArgumentParser()
    for name in ['binary','executable','model','generated-wav','report']:parser.add_argument('--'+name,required=True)
    parser.add_argument('--execute-approved-generated-speech',action='store_true')
    args=parser.parse_args()
    assert args.execute_approved_generated_speech,'Real ASR must be an explicit approved qualification step.'
    binary=Path(args.binary).resolve(strict=True);executable=Path(args.executable).resolve(strict=True);model=Path(args.model).resolve(strict=True);audio=Path(args.generated_wav).resolve(strict=True)
    assert model.stat().st_size==77704715 and digest(model)==MODEL_SHA256
    wave=audio.read_bytes();assert len(wave)<=2*1024*1024
    checks=[];started=time.monotonic();result=None
    with tempfile.TemporaryDirectory(prefix='smith-real-asr-') as temporary:
        app=EncryptedApp(str(binary),Path(temporary).resolve()/'profile')
        try:
            app.start(True)
            review=app.api('/media/transcription/review',{'executable_path':str(executable),'model_path':str(model)})
            assert review['model']['sha256']==MODEL_SHA256 and review['executable']['sha256']==digest(executable)
            assert review['verified_model'] is False and review['requested_device']=='cpu'
            assert review['allowed_languages']==['en'] and review['declared_multilingual'] is False
            assert not app.api('/media/artifacts')['artifacts'];checks.append('read-only-exact-executable-and-model-review')
            request_id=str(uuid.uuid4());payload={'review_id':review['review_id'],'request_id':request_id,'name':'generated-speech.wav','wav_base64':base64.b64encode(wave).decode(),'language':'en'}
            for language in ['ru','auto']:
                refused=app.api('/media/transcription/start',{**payload,'request_id':str(uuid.uuid4()),'language':language},want=415)
                assert refused['code']=='transcription_language_unsupported'
            checks.append('english-only-model-rejects-russian-and-auto-without-silent-coercion')
            first=app.api('/media/transcription/start',payload);assert first['request_id']==request_id
            assert app.api('/media/transcription/start',payload)['request_id']==request_id
            output=app.poll(lambda:app.api('/media/transcription/jobs/'+request_id),lambda j:j['state'] not in ['running','cancelling'],seconds=190)
            assert output['state']=='completed',output.get('error')
            result=output['result'];assert result['language']=='en' and result['segments']
            words=result['text'].lower();assert 'robot' in words and 'microphone' in words and len(words.split())>=15,words
            assert output['source']['sha256']==hashlib.sha256(wave).hexdigest()
            assert result['vtt'].startswith('WEBVTT') and '-->' in result['srt'];checks.append('real-cpu-whisper-tiny-en-transcribes-generated-speech-through-normal-app-api')
            save={'request_id':str(uuid.uuid4()),'title':'Generated speech qualification','format':'vtt'}
            saved=app.api('/media/transcription/jobs/'+request_id+'/save',save);again=app.api('/media/transcription/jobs/'+request_id+'/save',save)
            assert again['duplicate'] and saved['artifact']['id']==again['artifact']['id']
            artifact=saved['artifact']['id'];stored=app.api('/media/artifacts/'+artifact+'/original',raw=True)
            canonical=json.loads(stored);assert canonical['transcript']==result and canonical['format']=='vtt' and canonical['source']==output['source']
            assert canonical['original_audio_retained'] is False;checks.append('canonical-encrypted-transcript-artifact-preserves-every-format-and-source-identity')
            bad={**payload,'request_id':str(uuid.uuid4()),'wav_base64':base64.b64encode(b'not wave').decode()}
            assert app.api('/media/transcription/start',bad,want=415)['code']=='transcription_audio_unsupported'
            altered={**payload,'name':'another.wav'};assert app.api('/media/transcription/start',altered,want=409)['code']=='transcription_request_conflict';checks.append('malformed-audio-and-conflicting-request-identity-refused')
            cancel_id=str(uuid.uuid4());app.api('/media/transcription/start',{**payload,'request_id':cancel_id,'wav_base64':base64.b64encode(repeated_generated_wave(wave)).decode()})
            child_pid,child_scratch=app.poll(lambda:owned_transcriber(app.process.pid),lambda child:child is not None,seconds=10)
            assert app.api('/media/transcription/jobs/'+cancel_id)['state']=='running'
            app.api('/media/transcription/jobs/'+cancel_id+'/cancel',{})
            cancelled=app.poll(lambda:app.api('/media/transcription/jobs/'+cancel_id),lambda j:j['state'] not in ['running','cancelling'],seconds=10)
            assert cancelled['state']=='cancelled'
            assert not child_scratch.exists(),'Owned transcription scratch remained after confirmed cancellation'
            try:os.kill(child_pid,0)
            except ProcessLookupError:pass
            else:raise AssertionError('Owned transcription child still exists after cancellation')
            app.api('/media/transcription/jobs/'+cancel_id+'/save',{**save,'request_id':str(uuid.uuid4())},want=409);checks.append('observed-real-transcriber-child-cancellation-reaps-removes-scratch-and-prevents-late-save')
            app.stop();app.start();assert app.api('/media/artifacts/'+artifact+'/original',raw=True)==stored
            app.api('/media/transcription/jobs/'+request_id,want=409)
            assert len(app.api('/media/artifacts')['artifacts'])==1;app.stop();checks.append('restart-keeps-exact-artifact-and-revokes-transient-runtime-review-and-job')
            for path in app.root.rglob('*'):
                if path.is_file():assert result['text'].encode() not in path.read_bytes(),'transcript text leaked outside encrypted storage'
            checks.append('disposable-profile-files-contain-no-plaintext-transcript')
        finally:app.stop()
    report={'schema':'smith.local-transcription-qualification.v1','real_asr':True,'speech_source':'locally generated fixture; no microphone','device_requested':'cpu','binary_sha256':digest(binary),'executable_sha256':digest(executable),'model_sha256':MODEL_SHA256,'model_bytes':77704715,'generated_audio_sha256':hashlib.sha256(wave).hexdigest(),'generated_audio_bytes':len(wave),'groups':len(checks),'passed':checks,'transcript':result,'elapsed_seconds':round(time.monotonic()-started,3),'limits':'English tiny model and generated speech only; no Russian/quality/general audio qualification'}
    destination=Path(args.report);destination.parent.mkdir(parents=True,exist_ok=True);destination.write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n');print(json.dumps({'groups':len(checks),'real_asr':True,'report':destination.name}))
if __name__=='__main__':main()
