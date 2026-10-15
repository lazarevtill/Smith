#!/usr/bin/env python3
"""Encrypted uploaded originals through drafts, branch edits, provider context and downloads."""
import argparse,json,struct,tempfile,threading,uuid,zlib
from pathlib import Path
from http.server import ThreadingHTTPServer
from profile_fixture import EncryptedApp
from conversation_parity import Provider

def main():
    root=Path(__file__).resolve().parents[1];parser=argparse.ArgumentParser();parser.add_argument('--binary',default=str(root/'target/debug/smith-next'));args=parser.parse_args();checks=[]
    server=ThreadingHTTPServer(('127.0.0.1',0),Provider);threading.Thread(target=server.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='smith-keyed-media-') as tmp:
        tmp=Path(tmp).resolve();app=EncryptedApp(args.binary,tmp/'profile')
        def upload(name,body,mime):
            boundary='smith-'+str(uuid.uuid4());meta=json.dumps({'request_id':str(uuid.uuid4()),'filename':name,'declared_mime':mime}).encode()
            payload=b'--'+boundary.encode()+b'\r\nContent-Disposition: form-data; name="metadata"\r\n\r\n'+meta+b'\r\n--'+boundary.encode()+b'\r\nContent-Disposition: form-data; name="file"; filename="fixture"\r\nContent-Type: application/octet-stream\r\n\r\n'+body+b'\r\n--'+boundary.encode()+b'--\r\n'
            return app.api('/media/uploads',payload,content_type='multipart/form-data; boundary='+boundary)
        def png():
            def chunk(kind,data):return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data))
            return b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',1,1,8,6,0,0,0))+chunk(b'IDAT',zlib.compress(b'\x00\xff\x00\x00\xff'))+chunk(b'IEND',b'')
        def complete(route,run):
            state=app.poll(lambda:app.api(route),lambda s:next(m for m in s['messages'] if m['id']==run['message_id'])['status']!='streaming');assert state['messages'][-1]['status']=='completed';return state
        try:
            app.start(True);app.api('/config',{'provider':'openai','endpoint':f'http://127.0.0.1:{server.server_port}/v1','model':'fixture-text'},'PUT');assert app.api('/connection/test',app.target())['ok']
            document_bytes=b'Name,Note\nSynthetic,PRIVATE DOCUMENT CONTENT\n';document=upload('context.csv',document_bytes,'text/csv');image_bytes=png();image=upload('reference.png',image_bytes,'image/png')
            docref={'id':document['id'],'delivery':'context'};imgref={'id':image['id'],'delivery':'reference_only'}
            draft=app.api('/drafts/new',{'version':0,'text':'saved document draft','attachments':[],'media':[docref,imgref]},'PUT');assert draft['media']==[docref,imgref]
            app.api('/media/'+document['id'],{},'DELETE',want=409);app.stop();app.start();assert app.api('/drafts/new')['media']==[docref,imgref]
            assert app.api('/media/'+document['id']+'/original',raw=True)==document_bytes;assert app.api('/media/'+image['id']+'/original',raw=True)==image_bytes
            assert app.api('/connection/test',app.target())['ok'];checks.append('encrypted-originals-and-draft-refs-survive-restart-with-draft-owned-delete-refused')
            chat=app.api('/chats',{'title':'Document branch fixture',**app.target()});route='/chats/'+chat['id']
            original=app.api(route+'/messages',{'version':0,'content':'Original with files','media':[docref,imgref],**app.target()});state=complete(route,original)
            wire=Provider.requests[-1]['messages'][0]['content'];assert 'PRIVATE DOCUMENT CONTENT' in wire and 'image pixels are not provided' in wire;assert len(state['active_messages'][0]['media'])==2
            before=state;app.api(route+'/messages',{'version':state['chat']['version'],'content':'No implicit vision','media':[{'id':image['id'],'delivery':'context'}],**app.target()},want=409);assert app.api(route)==before
            checks.append('explicit-image-reference-and-bounded-document-context-reach-only-active-provider-path')
            edited=app.api(route+'/edit',{'version':state['chat']['version'],'message_id':original['user_message_id'],'content':'Edited retaining originals',**app.target()});state=complete(route,edited);assert len(state['active_messages'][0]['media'])==2
            removed=app.api(route+'/edit',{'version':state['chat']['version'],'message_id':edited['user_message_id'],'content':'Edited without originals','media':[],**app.target()});state=complete(route,removed);assert state['active_messages'][0]['media']==[];assert 'PRIVATE DOCUMENT CONTENT' not in Provider.requests[-1]['messages'][0]['content']
            state=app.api(route+'/branch',{'version':state['chat']['version'],'message_id':original['user_message_id'],**app.target()});assert len(state['active_messages'][0]['media'])==2
            assert b'PRIVATE DOCUMENT CONTENT' in app.api(route+'/export',raw=True)
            checks.append('edit-omission-retains-refs-explicit-removal-forks-originals-and-export-preserves-context')
            generated=app.api('/media/generate',{'request_id':str(uuid.uuid4()),'name':'table.xlsx','format':'xlsx','table':{'headers':['Code'],'rows':[[{'type':'text','value':'00123'}]]},'chat_id':chat['id'],'version':state['chat']['version']})
            artifact=generated['artifact']['id'];xlsx=app.api('/artifacts/'+artifact,raw=True);assert xlsx.startswith(b'PK');assert xlsx==app.api('/media/artifacts/'+artifact+'/original',raw=True)
            app.stop();app.start();assert app.api('/artifacts/'+artifact,raw=True)==xlsx;assert len(app.api(route)['active_messages'][0]['media'])==2;app.stop()
            for file in app.root.rglob('*'):
                if file.is_file():assert b'PRIVATE DOCUMENT CONTENT' not in file.read_bytes(),file
            checks.append('generated-xlsx-original-download-is-binary-and-keyed-restart-retains-all-media')
        finally:app.stop();server.shutdown();server.server_close()
    print(json.dumps({'groups':len(checks),'passed':checks,'provider_requests':len(Provider.requests)},indent=2))
if __name__=='__main__':main()
