'use strict';
// Headed normal-app acceptance. All profile/browser data and provider responses
// are synthetic and disposable; no real model, provider, microphone or camera.
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const http = require('node:http');
const crypto = require('node:crypto');
const {spawn, execFileSync} = require('node:child_process');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || '/opt/homebrew/lib/node_modules/playwright');
const root = path.resolve(__dirname, '..');
const binary = process.env.SMITH_BROWSER_BINARY || path.join(root, 'target/debug/smith-next');
const browserPath = '/Applications/Brave Browser.app/Contents/MacOS/Brave Browser';
const output = path.join(root, 'docs/screenshots/media-provider');
const fixtures = path.join(root, 'crates/smith-media/tests/fixtures');
const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const checks = [], screenshots = [], downloads = [], pageErrors = [], mediaCalls = [], forbiddenRequests = [];
let temp, child, browser, page, base, copied, provider, endpoint, stage = 'setup', binaryHash;
const allowed = new Set();
const png = require('node:fs').readFileSync(path.join(fixtures, 'provider-256.png'));
function clean(value) {
  let text = String(value);
  for (const secret of [temp, root, browserPath].filter(Boolean)) text = text.split(secret).join('[redacted path]');
  return text.replace(/http:\/\/127\.0\.0\.1:\d+/g, '[owned loopback]').replace(/profile_session=[^\s&"']+/g, 'profile_session=[redacted]');
}
async function eventually(read, label, timeout = 20000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) { if (await read()) return; await delay(60); }
  throw new Error('Timed out: ' + label);
}
function step(name) { stage = name; console.log('Media browser: ' + name); }
async function startApp() {
  let logs = '';
  const env = {...process.env, SMITH_NEXT_DATA_DIR:path.join(temp, 'app'), SMITH_NEXT_PORT:'0'};
  delete env.SMITH_NEXT_SYNTHETIC_PROFILE; delete env.SMITH_NEXT_MANUAL_PROFILE;
  child = spawn(copied, [], {env, stdio:['ignore','pipe','pipe']});
  child.stdout.on('data', b => { logs = (logs+b).slice(-16384); });
  child.stderr.on('data', b => { logs = (logs+b).slice(-16384); });
  await eventually(() => {
    const found = logs.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0];
    if (found) { base = found; allowed.add(base); return true; }
    if (child.exitCode !== null) throw new Error('Owned app stopped: ' + clean(logs));
    return false;
  }, 'automatic normal workspace');
}
async function stopApp() {
  if (!child || child.exitCode !== null) return;
  const exited = new Promise(resolve => child.once('exit', resolve)); child.kill('SIGINT');
  await Promise.race([exited, delay(10000)]);
  if (child.exitCode === null) { child.kill('SIGKILL'); await exited; throw new Error('Owned app did not stop gracefully'); }
}
async function fixtureServer() {
  provider = http.createServer(async(req,res) => {
    const chunks=[]; for await (const chunk of req) chunks.push(chunk);
    const bytes=Buffer.concat(chunks); const route=req.url;
    let body; if ((req.headers['content-type']||'').includes('application/json')) body=JSON.parse(bytes);
    const reply = value => { if(!res.destroyed){res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify(value));} };
    if (route==='/v1/models' && req.method==='GET') return reply({data:[{id:'synthetic-media'}]});
    mediaCalls.push({route,body,bytes});
    if (route==='/v1/embeddings') {
      assert.equal(body.model,'synthetic-media'); assert.equal(body.encoding_format,'float');
      if(body.input[0]==='delay fixture') await delay(2000);
      return reply({model:'synthetic-media',data:[{index:0,embedding:[0.125,-0.5,1]}]});
    }
    if (route==='/v1/images/generations') {
      assert.equal(body.model,'synthetic-media'); assert.equal(body.n,1); assert.equal(body.size,'256x256');
      return reply({data:[body.prompt==='url response fixture'?{url:'http://127.0.0.1:1/never-fetch'}:{b64_json:png.toString('base64')}]});
    }
    if (route==='/v1/chat/completions') {
      assert.equal(body.model,'synthetic-media');assert.equal(body.stream,false);
      const image=body.messages[0].content.find(part=>part.type==='image_url');
      assert.deepEqual(Buffer.from(image.image_url.url.split(',')[1],'base64'),png);
      return reply({choices:[{finish_reason:'stop',message:{content:'Synthetic pixels were explicitly provided.'}}]});
    }
    if (route==='/v1/audio/transcriptions') {
      assert(bytes.includes(Buffer.from('synthetic-media'))); assert(bytes.includes(wav()));
      return reply({text:'Привет из fixture',language:'ru',segments:[{start:0,end:0.5,text:'Привет из fixture'}]});
    }
    forbiddenRequests.push(route);res.writeHead(500);res.end();
  });
  await new Promise(resolve=>provider.listen(0,'127.0.0.1',resolve));
  endpoint='http://127.0.0.1:'+provider.address().port+'/v1';
}
function wav() {
  const bytes=Buffer.alloc(32044);bytes.write('RIFF');bytes.writeUInt32LE(32036,4);bytes.write('WAVEfmt ',8);
  bytes.writeUInt32LE(16,16);bytes.writeUInt16LE(1,20);bytes.writeUInt16LE(1,22);bytes.writeUInt32LE(16000,24);
  bytes.writeUInt32LE(32000,28);bytes.writeUInt16LE(2,32);bytes.writeUInt16LE(16,34);bytes.write('data',36);bytes.writeUInt32LE(32000,40);return bytes;
}
async function api(route) {return page.evaluate(async route=>{const r=await sessionFetch('/api'+route);if(!r.ok)throw new Error('Read-only verification HTTP '+r.status);return r.json()},route);}
async function openApp() {
  await page.goto(base,{waitUntil:'domcontentloaded'});await page.waitForFunction(()=>window.smithAppReady===true&&window.smithNavigationReady===true);
  const status=await page.evaluate(async()=>{const r=await fetch('/api/profile/status');return r.json()});
  assert.equal(status.automatic,true);assert.equal(status.encrypted,true);assert.equal(status.state,'unlocked');
  assert.equal(await page.locator('input[type=password]:visible').count(),0);
}
async function docs() {if(!await page.locator('#documents').isVisible()){await page.locator('#rail-activity').click();await page.getByRole('button',{name:'Open Documents',exact:true}).click();}}
async function shot(name) {
  assert.equal(await page.locator('input[type=password]').evaluateAll(nodes=>nodes.some(n=>n.value)),false);
  if(name>='03-')await page.locator('#media-provider-output').scrollIntoViewIfNeeded();
  await page.screenshot({path:path.join(output,name+'.png'),fullPage:true,animations:'disabled'});screenshots.push(name+'.png');
}
async function uploaded(name,mimeType,buffer,errorCode) {
  await docs();await page.locator('#document-upload').setInputFiles({name,mimeType,buffer});
  const pending=page.waitForResponse(r=>new URL(r.url()).pathname==='/api/media/uploads'&&r.request().method()==='POST');
  await page.locator('#document-upload-submit').click();const response=await pending,body=await response.json();
  if(errorCode){assert.equal(body.code,errorCode);assert(!response.ok());await page.waitForFunction(message=>document.getElementById('document-result').textContent===message,body.error);return body;}
  assert(response.ok(),JSON.stringify(body));await page.waitForFunction(name=>document.getElementById('document-name').textContent===name,name);
  assert.equal(body.sha256,hash(buffer));return body;
}
async function download(metadata,expected) {
  const pending=page.waitForEvent('download');await page.locator('#document-original').click();const file=await pending;
  const bytes=await fs.readFile(await file.path());assert.equal(await file.failure(),null);assert.equal(hash(bytes),metadata.sha256);
  if(expected)assert.deepEqual(bytes,expected);downloads.push({name:file.suggestedFilename(),bytes:bytes.length,sha256:hash(bytes)});return bytes;
}
async function generate(format) {
  await docs();if(!await page.locator('#document-generator').evaluate(node=>node.open))await page.locator('#document-generator > summary').click();
  await page.locator('#document-format').selectOption(format);await page.locator('#document-output-name').fill('synthetic-unicode.'+format);
  await page.locator('#document-table').fill('Title\tDetail\nПривет, мир! Ёж\t00123\nExact text\t9007199254740993.00');
  const pending=page.waitForResponse(r=>new URL(r.url()).pathname==='/api/media/generate'&&r.request().method()==='POST');
  await page.locator('#document-generate').click();const response=await pending,body=await response.json();assert(response.ok(),JSON.stringify(body));
  await page.waitForFunction(name=>document.getElementById('document-name').textContent===name,body.document.name);
  const bytes=await download(body.document);assert.equal(JSON.parse(body.document.content.context).rows[1][1].value,'9007199254740993.00');
  return {body,bytes};
}
async function review(kind,text='synthetic input') {
  await docs();if(!await page.locator('#document-provider').evaluate(node=>node.open))await page.locator('#document-provider > summary').click();
  await page.locator('#media-operation').selectOption(kind);
  if(kind!=='transcribe_audio'){await page.locator('#media-provider-text').fill(text);await page.locator('#media-provider-text').blur();}
  const before=mediaCalls.length;
  const pending=page.waitForResponse(r=>new URL(r.url()).pathname==='/api/media/provider/review');
  await page.locator('#media-provider-review').click();const response=await pending,body=await response.json();assert(response.ok(),JSON.stringify(body));
  assert.equal(body.provenance.model,'synthetic-media');assert.equal(body.provenance.endpoint,endpoint);assert.equal(body.verified_capability,false);
  assert.equal(mediaCalls.length,before);assert(await page.locator('#media-provider-send').isDisabled());
  await page.locator('#media-provider-consent').check();return body;
}
async function send(expectedError) {
  const pending=page.waitForResponse(r=>new URL(r.url()).pathname==='/api/media/provider/execute');await page.locator('#media-provider-send').click();
  const response=await pending,body=await response.json();
  if(expectedError){assert.equal(body.code,expectedError);await page.waitForFunction(message=>document.getElementById('media-provider-output').textContent===message,body.error);}
  else{assert(response.ok(),JSON.stringify(body));await page.waitForFunction(()=>{try{return !!JSON.parse(document.getElementById('media-provider-output').textContent).provenance}catch{return false}});}
  return body;
}
async function main() {
  await fs.mkdir(output,{recursive:true});temp=await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(),'smith-media-provider-')));
  copied=path.join(temp,'smith-next');await fs.copyFile(binary,copied);await fs.chmod(copied,0o700);binaryHash=hash(await fs.readFile(copied));
  await fixtureServer();await startApp();
  browser=await chromium.launchPersistentContext(path.join(temp,'browser'),{executablePath:browserPath,headless:false,viewport:{width:1360,height:1050},colorScheme:'light',acceptDownloads:true,args:['--disable-background-networking','--disable-component-update','--no-first-run','--no-default-browser-check']});
  browser.setDefaultTimeout(15000);await browser.route('**/*',route=>{const url=route.request().url();if(url.startsWith('data:')||url.startsWith('blob:')||allowed.has(new URL(url).origin))return route.continue();forbiddenRequests.push(new URL(url).protocol);return route.abort()});
  page=await browser.newPage();page.on('pageerror',e=>pageErrors.push(clean(e.message)));await openApp();
  step('passive Office uploads');
  for(const [name,mime]of [['passive.xls','application/vnd.ms-excel'],['passive.ods','application/vnd.oasis.opendocument.spreadsheet']]){
    const bytes=await fs.readFile(path.join(fixtures,name));const file=await uploaded(name,mime,bytes);const book=JSON.parse(file.content.context);
    assert(book.sheets[0].cells.some(c=>c.value.value==='00123'||String(c.value.value).includes('00123')));await download(file,bytes);
  }
  const xlsm=execFileSync('python3',['-c',`import io,sys,zipfile\nz=zipfile.ZipFile(sys.argv[1]);o=io.BytesIO()\nwith zipfile.ZipFile(o,'w',zipfile.ZIP_DEFLATED) as w:\n for n in z.namelist():\n  b=z.read(n)\n  if n=='[Content_Types].xml': b=b.replace(b'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml',b'application/vnd.ms-excel.sheet.macroEnabled.main+xml')\n  w.writestr(n,b)\nsys.stdout.buffer.write(o.getvalue())`,path.join(fixtures,'cells.xlsx')]);
  const macroFree=await uploaded('passive.xlsm','application/vnd.ms-excel.sheet.macroenabled.12',xlsm);await download(macroFree,xlsm);await shot('01-passive-spreadsheet-context');
  checks.push('XLS, ODS and macro-free XLSM upload through real UI; derived typed context and exact original downloads');
  step('Unicode PDF and PowerPoint exports');
  const generated=[];for(const format of ['pdf','pptx']){const result=await generate(format);generated.push(result);await shot('02-generated-'+format);await uploaded('reimport.'+format,format==='pdf'?'application/pdf':'application/vnd.openxmlformats-officedocument.presentationml.presentation',result.bytes);}
  await uploaded('foreign.pdf','application/pdf',Buffer.from('%PDF-1.7\nforeign'),'media_unsupported');
  const malformed=Buffer.from(await fs.readFile(path.join(fixtures,'passive.xls')));malformed.writeUInt32LE(100,48);
  await uploaded('malformed.xls','application/vnd.ms-excel',malformed,'media_invalid_content');
  checks.push('Real PDF/PPTX generated, exact bytes downloaded and canonical reimport succeeds; unsupported/malformed files show typed errors');
  step('reference-only image and explicit target selection');
  const reference=await uploaded('reference.png','image/png',png);assert.deepEqual(reference.allowed_delivery,['reference_only']);assert.equal(mediaCalls.length,0);
  await page.locator('#close-documents').click();await page.locator('#rail-settings').click();await page.locator('#provider').selectOption('openai');await page.locator('#endpoint').fill(endpoint);await page.locator('#test-button').click();
  await page.waitForFunction(()=>document.getElementById('test-result').textContent.includes('Connected.'));
  await page.locator('#settings [data-close=settings]').first().click();await page.locator('#model-select').selectOption('synthetic-media');
  await eventually(async()=> (await api('/config')).model==='synthetic-media','selected dropdown model saved');
  checks.push('Image upload sends no pixels; real selected-provider discovery and dropdown select exact synthetic model');
  step('embedding review and execution');await review('embed_text');let result=await send();assert.deepEqual(result.embedding,[0.125,-0.5,1]);await shot('03-explicit-embedding-result');
  checks.push('Review sends no input; consent required; embedding returns fixture vector and exact selected provenance');
  step('image generation and explicit vision');await review('generate_image','synthetic PNG fixture');result=await send();await download(result.document,png);const generatedImage=result.document;await shot('04-generated-image-artifact');
  await review('vision','Read these synthetic pixels explicitly');result=await send();assert.equal(result.pixels_sent,true);assert.match(result.text,/explicitly/);await shot('05-explicit-vision-result');
  checks.push('Image generation saves validated PNG in encrypted library; explicit vision sends exact PNG pixels only after review');
  step('selected WAV transcription');await docs();await page.locator('#media-operation').selectOption('transcribe_audio');await page.locator('#media-audio-file').setInputFiles({name:'synthetic.wav',mimeType:'audio/wav',buffer:wav()});await page.locator('#media-audio-language').selectOption('ru');
  await review('transcribe_audio');result=await send();assert.equal(result.audio_seconds,1);assert.match(result.srt,/Привет/);assert.match(result.vtt,/WEBVTT/);await shot('06-wav-transcript');
  checks.push('Explicit synthetic WAV upload produces validated segments/SRT/VTT; no hardware access or model download');
  step('cancellation and rejected provider URL');await review('embed_text','delay fixture');const execute=page.waitForResponse(r=>new URL(r.url()).pathname==='/api/media/provider/execute');const before=mediaCalls.length;await page.locator('#media-provider-send').click();await eventually(()=>mediaCalls.length>before,'fixture received delayed request');await page.locator('#media-provider-cancel').click();assert.equal((await(await execute).json()).code,'media_provider_cancelled');await eventually(()=>page.locator('#media-operation').isEnabled(),'cancel settles');
  await review('generate_image','url response fixture');await send('media_provider_response_invalid');await shot('07-provider-url-rejected');
  checks.push('Explicit cancellation drains pending operation; provider response URL is rejected without follow-up fetch');
  step('normal encrypted restart and exact original retrieval');await page.locator('#close-documents').click();await stopApp();await startApp();await openApp();
  for(const file of [reference,generatedImage,...generated.map(g=>g.body.document)]){
    const retained=await api('/media/'+file.id);assert.equal(retained.sha256,file.sha256);
    const bytes=await page.evaluate(async url=>Array.from(new Uint8Array(await(await sessionFetch(url)).arrayBuffer())),retained.original_url);assert.equal(hash(Buffer.from(bytes)),file.sha256);
  }
  const config=await api('/config');assert.equal(config.model,'synthetic-media');
  const db=await fs.readFile(path.join(temp,'app','local-workspace','profile.sqlite3'));
  assert.notEqual(db.subarray(0,16).toString(),'SQLite format 3\0');
  assert(!db.includes(Buffer.from('Привет, мир! Ёж')));
  checks.push('Automatic normal workspace restarts without login and retains generated/original bytes exactly; media bindings remain transient');
  assert.deepEqual(pageErrors,[]);assert.deepEqual(forbiddenRequests,[]);
  return {result:'pass',date:new Date().toISOString(),headed:true,browser:'Brave',normalAutomaticEncryptedWorkspace:true,syntheticOnly:true,noActualProvidersOrHardware:true,binarySha256:binaryHash,checks,screenshots,downloads,providerCallCounts:Object.fromEntries([...new Set(mediaCalls.map(c=>c.route))].map(route=>[route,mediaCalls.filter(c=>c.route===route).length])),pageErrors,forbiddenRequests};
}
(async()=>{
  try{const report=await main();await fs.writeFile(path.join(output,'acceptance.json'),JSON.stringify(report,null,2)+'\n');await fs.rm(path.join(output,'failure.json'),{force:true});console.log(JSON.stringify({result:'pass',checks:checks.length,downloads:downloads.length}));}
  catch(error){await fs.mkdir(output,{recursive:true});await fs.writeFile(path.join(output,'failure.json'),JSON.stringify({result:'fail',stage,error:clean(error.stack),checks,pageErrors,forbiddenRequests},null,2)+'\n');console.error(clean(error.stack));process.exitCode=1;}
  finally{await browser?.close().catch(()=>{});await stopApp().catch(()=>{});if(provider){provider.closeAllConnections();await new Promise(resolve=>provider.close(resolve));}if(temp)await fs.rm(temp,{recursive:true,force:true});}
})();
