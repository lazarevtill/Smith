// Opt-in real-model acceptance. The operator supplies an already approved local
// qualification JSON; no model discovery, download, existing-server call or key.
// Never include that private input file in the repository or the public evidence.
'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs/promises');
const path=require('node:path');
const os=require('node:os');
const {spawn,execFileSync}=require('node:child_process');
let playwright;try{playwright=require('playwright')}catch{playwright=require(process.env.PLAYWRIGHT_MODULE||'/opt/homebrew/lib/node_modules/playwright')}
const root=path.resolve(__dirname,'..');
const binary=path.resolve(process.env.SMITH_BROWSER_BINARY||path.join(root,'target/release/smith-next'));
const browser=process.env.SMITH_BROWSER_EXECUTABLE||'/Applications/Brave Browser.app/Contents/MacOS/Brave Browser';
const output=path.resolve(process.env.SMITH_BROWSER_OUTPUT||path.join(root,'docs/screenshots/current-design'));
const delay=ms=>new Promise(r=>setTimeout(r,ms));
let child,context,base,profileSession,monitor,monitorBusy=false,guardError=null,peakRss=0,minFree=100,currentStage='startup';
function stage(name){currentStage=name;console.log('Native browser: '+name)}
const ownedPids=new Set(),checks=[],errors=[],blocked=[];
function exists(pid){try{process.kill(pid,0);return true}catch(e){if(e.code==='ESRCH')return false;throw e}}
function memoryFree(){const s=execFileSync('/usr/bin/memory_pressure',['-Q'],{encoding:'utf8',timeout:3000});const m=s.match(/System-wide memory free percentage:\s*(\d+)%/);assert(m,'Memory guard unavailable');return Number(m[1])}
async function api(route){const r=await fetch(base+'/api'+route,{headers:{'X-Smith-Profile-Session':profileSession||''}});assert(r.ok,'Read-only qualification status failed');return r.json()}
async function guard(){
  if(monitorBusy||!base)return;monitorBusy=true;
  try{
    const free=memoryFree();minFree=Math.min(minFree,free);
    if(!profileSession)return;const state=await api('/native/status');const pid=state.runtime?.pid;
    if(pid){ownedPids.add(pid);let rss=0;try{rss=Number(execFileSync('/bin/ps',['-o','rss=','-p',String(pid)],{encoding:'utf8',timeout:2000}).trim())}catch{if(exists(pid))throw Error('Owned process resource sample failed')};peakRss=Math.max(peakRss,rss);if(rss>7*1024*1024)throw Error('Owned process exceeded the 7 GiB RSS guard')}
    if(free<20)throw Error('System memory free percentage fell below 20');
  }catch(e){guardError=e;if(child?.exitCode===null)child.kill('SIGINT')}finally{monitorBusy=false}
}
async function waitFor(predicate,seconds=90){const deadline=Date.now()+seconds*1000;while(Date.now()<deadline){if(guardError)throw guardError;const value=await predicate();if(value)return value;if(child.exitCode!==null)throw Error('Owned application stopped during qualification');await delay(200)}throw Error('Bounded qualification deadline exceeded at '+currentStage)}
async function main(){
  assert(process.platform==='darwin','This real-model evidence harness requires macOS resource checks');
  assert(process.env.SMITH_NATIVE_APPROVED_PLAN,'Set SMITH_NATIVE_APPROVED_PLAN only for an explicitly authorized existing-files model qualification');
  const input=JSON.parse(await fs.readFile(process.env.SMITH_NATIVE_APPROVED_PLAN,'utf8'));
  const manifest=input.reviewed_plan?.manifest;assert(manifest?.execution_kind==='native','Expected previously approved native manifest');
  const startFree=memoryFree();assert(startFree>=30,'Defer real-model load: system memory free percentage is below 30');
  const temp=await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(),'smith-native-browser-')));let logs='',report;
  await fs.mkdir(output,{recursive:true});
  try{
    child=spawn(binary,[],{env:{...process.env,SMITH_NEXT_DATA_DIR:path.join(temp,'app'),SMITH_NEXT_PORT:'0'},stdio:['ignore','pipe','pipe']});
    child.stdout.on('data',b=>logs+=b);child.stderr.on('data',b=>logs+=b);
    await waitFor(()=>{base=logs.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0];return base},10);
    monitor=setInterval(()=>guard(),1000);await guard();
    context=await playwright.chromium.launchPersistentContext(path.join(temp,'browser'),{executablePath:browser,headless:false,viewport:{width:1280,height:860},colorScheme:'dark',acceptDownloads:true,args:['--disable-background-networking','--disable-component-update','--no-first-run','--no-default-browser-check']});
    await context.route('**/*',route=>{const url=route.request().url();if(url.startsWith(base+'/')||url.startsWith('blob:')||url.startsWith('data:'))return route.continue();blocked.push(url);return route.abort()});
    const page=await context.newPage();page.on('pageerror',e=>errors.push(e.message));
    await page.goto(base,{waitUntil:'networkidle'});stage('create encrypted fixture profile');await page.locator('#passphrase').fill('Synthetic native browser fixture key');await page.locator('#passphrase-confirm').fill('Synthetic native browser fixture key');await page.locator('#access-submit').click();await page.locator('#open-workspace').waitFor({state:'visible',timeout:45000});await page.locator('#open-workspace').click();await page.waitForFunction(()=>window.smithAppReady&&window.smithNavigationReady);profileSession=await page.evaluate(()=>profileSession);checks.push('real model runs only inside explicitly unlocked encrypted profile');stage('select managed connection');await page.locator('#connection-button').click();await page.locator('#provider').selectOption('managed');
    await page.locator('#settings-form button[type=submit]').click();
    await waitFor(async()=>((await api('/config')).provider==='managed'));
    for(const kind of ['executable','model'])for(const [field,key]of [['path','path'],['sha256','sha256'],['source','source'],['revision','source_revision']])await page.locator('#native-'+kind+'-'+field).fill(manifest[kind][key]);
    await page.locator('#native-model-id').fill(manifest.model_id);
    await page.locator('#native-kind').selectOption('native');await page.locator('#native-backend').selectOption('cpu');
    for(const [name,value]of [['context','512'],['threads','2'],['startup','45'],['generation','45']])await page.locator('#native-'+name).fill(value);
    stage('prepare reviewed files');const prepareStart=Date.now();await page.locator('#native-prepare').click();
    const prepared=await waitFor(async()=>{const s=await api('/native/status');if(s.operation?.status==='failed')throw Error('Native plan preparation failed');return s.prepared},90);
    assert.equal(prepared.plan.manifest.executable.sha256,manifest.executable.sha256);assert.equal(prepared.plan.manifest.model.sha256,manifest.model.sha256);assert.equal(prepared.max_tokens,256);
    assert.equal((await api('/native/status')).runtime?.pid,null);assert(await page.locator('#native-load').isDisabled());
    checks.push('exact-file-hashes-reviewed-before-start-and-unchecked-consent-blocks-load');
    stage('approve and load');await page.locator('#native-consent').check();const loadStart=Date.now();await page.locator('#native-load').click();
    const ready=await waitFor(async()=>{const s=await api('/native/status');if(s.operation?.status==='failed')throw Error('Owned runtime load failed');return s.runtime?.phase==='ready'&&s.operation?.status==='succeeded'?s.runtime:null},90);
    const loadMs=Date.now()-loadStart;ownedPids.add(ready.pid);assert.equal(ready.execution_kind,'native');assert.equal(ready.observed_backend,'unknown');
    stage('adopt loaded target');await waitFor(async()=>((await api('/config')).tested===true));
    await page.keyboard.press('Escape');await page.locator('#send-button').waitFor({state:'visible'});
    stage('generate synthetic response');await page.locator('#prompt').fill('Reply with exactly the word READY.');const generationStart=Date.now();await page.locator('#send-button').click();
    await waitFor(async()=>{const messages=await page.locator('.message[data-role="assistant"]').count();return messages&&(await page.locator('#cancel-button').isHidden())},55);
    const generationMs=Date.now()-generationStart;
    const message=page.locator('.message[data-role="assistant"]').last();assert.equal(await message.getAttribute('data-status'),'completed');assert.equal((await message.locator('.message-content').innerText()).trim(),'READY');
    assert((await page.locator('#top-status').innerText()).includes(manifest.model_id));assert(!(await page.locator('#top-status').innerText()).includes('managed://'));
    checks.push('real-native-text-through-reviewed-app-browser-flow-and-friendly-model-provenance');
    await page.evaluate(()=>document.fonts.ready);assert.equal(await page.evaluate(()=>getComputedStyle(document.documentElement).colorScheme),'light');
    await page.screenshot({path:path.join(output,'after-native-real-chat.png'),fullPage:true,animations:'disabled'});
    const downloadEvent=page.waitForEvent('download');await page.locator('#export-button').click();const download=await downloadEvent;const exported=await fs.readFile(await download.path(),'utf8');assert(exported.includes('READY'));assert(exported.includes('assistant [completed]'));const saved=await api('/chats/'+await page.evaluate(()=>chatId));assert.equal(saved.active_messages.at(-1).snapshot.model,manifest.model_id);assert.equal(saved.active_messages.at(-1).snapshot.provider,'managed');checks.push('native-response-and-provenance-persisted-and-response-exported');
    await page.reload({waitUntil:'networkidle'});await page.waitForFunction(()=>window.smithAppReady&&window.smithNavigationReady);await page.locator('#history .history-row>button').first().click();assert.equal((await page.locator('.message[data-role="assistant"] .message-content').last().innerText()).trim(),'READY');checks.push('native-conversation-survives-browser-reload');
    await page.locator('#connection-button').click();await page.locator('#provider').selectOption('ollama');await page.locator('#endpoint').fill('http://127.0.0.1:19999');await delay(1500);assert.equal(await page.locator('#provider').inputValue(),'ollama');assert.equal(await page.locator('#endpoint').inputValue(),'http://127.0.0.1:19999');checks.push('native-status-polling-preserves-unsaved-connection-choice');
    await page.locator('#provider').selectOption('mock');await page.locator('#settings-form button[type=submit]').click();await page.locator('#settings').waitFor({state:'hidden'});
    stage('lock profile with owned model retained');const priorRuntime=(await api('/native/status')).runtime;assert.equal(priorRuntime.pid,ready.pid);clearInterval(monitor);while(monitorBusy)await delay(25);await page.locator('#lock-workspace').click();await page.locator('#access-form').waitFor({state:'visible',timeout:20000});const locked=await (await fetch(base+'/api/profile/status')).json();assert.equal(locked.state,'locked');for(const pid of ownedPids)assert.equal(exists(pid),false,'Owned native process survived profile lock');checks.push('profile lock drains and reaps the real owned model before reporting locked');
    profileSession=null;await page.locator('#passphrase').fill('Synthetic native browser fixture key');await page.locator('#access-submit').click();await page.locator('#open-workspace').waitFor({state:'visible',timeout:45000});await page.locator('#open-workspace').click();await page.waitForFunction(()=>window.smithAppReady&&window.smithNavigationReady);profileSession=await page.evaluate(()=>profileSession);const reopened=await api('/native/status');assert.equal(reopened.runtime?.pid||null,null);const retained=await api('/chats/'+await page.evaluate(()=>chatId));assert.equal(retained.active_messages.at(-1).content.trim(),'READY');checks.push('reunlock retains real response without automatically loading a model');const stopped={phase:'profile_locked_and_reopened_without_runtime'};
    assert.deepEqual(errors,[]);assert.deepEqual(blocked,[]);checks.push('no-page-javascript-errors-or-page-external-requests');
    report={date:new Date().toISOString(),browser:await context.browser().version(),headed:true,syntheticOnly:true,realModel:true,checks,model:{id:manifest.model_id,sha256:manifest.model.sha256},executable:{sha256:manifest.executable.sha256,revision:manifest.executable.source_revision},limits:{requestedBackend:'cpu',observedBackend:ready.observed_backend,contextTokens:512,maxOutputTokens:256,threads:2,startupSeconds:45,generationSeconds:45},durationsMs:{prepare:loadStart-prepareStart,loadAndRevalidation:loadMs,generation:generationMs},memory:{initialFreePercent:startFree,minimumSampledFreePercent:minFree,peakSampledChildRssKiB:peakRss,minimumAllowedFreePercent:20,maximumAllowedChildRssKiB:7*1024*1024,guardTriggered:!!guardError},lifecycle:{lockSucceeded:true,ownedPidCount:ownedPids.size,ownedPidsAbsent:true,reapEvidence:'successful profile-close barrier and all owned PIDs absent; no post-close runtime counter is available',phase:stopped.phase},downloads:false,existingServerCalls:false,privateInputCopied:false,screenshot:'after-native-real-chat.png'};
  }finally{
    clearInterval(monitor);while(monitorBusy)await delay(50);
    if(child&&child.exitCode===null){child.kill('SIGINT');await Promise.race([new Promise(r=>child.once('exit',r)),delay(10000)]);if(child.exitCode===null){child.kill('SIGKILL');throw Error('Application graceful shutdown exceeded its bound')}}
    if(context)await context.close();
    for(const pid of ownedPids)assert.equal(exists(pid),false,'Owned process remains after app shutdown');
    await fs.rm(temp,{recursive:true,force:true});
  }
  await fs.writeFile(path.join(output,'native-browser-acceptance.json'),JSON.stringify(report,null,2)+'\n');console.log(JSON.stringify(report,null,2));
}
main().catch(e=>{console.error(e.name+': '+e.message);process.exitCode=1});
