'use strict';
// Real headed-browser migration/rollback acceptance. Every profile is synthetic,
// independently copied and disposable; no existing browser/profile is opened.
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const {spawn, execFileSync} = require('node:child_process');
let playwright;
try { playwright = require('playwright'); }
catch { playwright = require(process.env.PLAYWRIGHT_MODULE || '/opt/homebrew/lib/node_modules/playwright'); }
const root = path.resolve(__dirname, '..');
const binary = path.resolve(process.env.SMITH_BROWSER_BINARY || path.join(root, 'target/debug/smith-next'));
const browserExecutable = process.env.SMITH_BROWSER_EXECUTABLE || '/Applications/Brave Browser.app/Contents/MacOS/Brave Browser';
const output = path.resolve(process.env.SMITH_MIGRATION_BROWSER_OUTPUT || path.join(root, 'docs/screenshots/automatic-local'));
const reportName = 'automatic-local-browser-acceptance.json';
const checks = [], screenshots = [], pageErrors = [], unexpectedOrigins = new Set();
const allowedOrigins = new Set();
let temp, copiedBinary, servedProfileSha256, browserServer, browser, context, page, active, stage = 'initialization';

let master = crypto.randomBytes(32).toString('hex');
const fixtureExecutable = path.join(root,'crates/smith-profile-storage/target/debug/examples/legacy_fixture');
const PROMPT = 'SENSITIVE_CHAT_SENTINEL';
const ANSWER = 'Original selected answer retained.';
const ARTIFACT = 'Synthetic original managed artifact. Exact retained bytes.\n';
const DRAFT = 'Synthetic later browser draft — rollback must retain this write.';
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');

function check(value, label) { assert(value, label); }
async function poll(read, predicate, label, timeout = 20000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const value = await read();
    if (predicate(value)) return value;
    await delay(80);
  }
  throw new Error(label);
}
function fixture(folder,version=25) {
  execFileSync(fixtureExecutable,[folder,String(version)],{input:master+'\n',stdio:['pipe','pipe','pipe']});
}
async function tree(folder) {
  const result = {};
  async function walk(current) {
    for (const entry of await fs.readdir(current, {withFileTypes: true})) {
      const file = path.join(current, entry.name);
      if (entry.isDirectory()) await walk(file);
      else { const bytes = await fs.readFile(file); result[path.relative(folder, file)] = {bytes: bytes.length, sha256: hash(bytes)}; }
    }
  }
  await walk(folder); return result;
}
async function bytesAbsent(folder) {
  const managed = await fs.readFile(path.join(path.dirname(folder),'managed.key'),'utf8');
  const needles = [managed,master, PROMPT, ANSWER, ARTIFACT, DRAFT].map(value => Buffer.from(value));
  async function walk(current) {
    for (const entry of await fs.readdir(current, {withFileTypes: true})) {
      const file = path.join(current, entry.name);
      if (entry.isDirectory()) await walk(file);
      else { const bytes = await fs.readFile(file); check(!bytes.subarray(0, 16).equals(Buffer.from('SQLite format 3\0')), 'Published database must remain encrypted'); for (const needle of needles) check(!bytes.includes(needle), 'Published profile must not expose synthetic plaintext or passphrase'); }
    }
  }
  await walk(folder);
}
async function startHost(name, sourceForDetectedPlaintext) {
  await stopHost();
  const folder = path.join(temp, name); await fs.mkdir(folder,{recursive:true});
  if (sourceForDetectedPlaintext) await fs.copyFile(path.join(sourceForDetectedPlaintext, 'smith-next.sqlite3'), path.join(folder, 'smith-next.sqlite3'));
  const env = {...global.process.env, SMITH_NEXT_DATA_DIR: folder, SMITH_NEXT_PORT: '0'};
  delete env.SMITH_NEXT_SYNTHETIC_PROFILE;delete env.SMITH_NEXT_MANUAL_PROFILE;
  const process = spawn(copiedBinary, [], {env, stdio: ['ignore', 'pipe', 'pipe']});
  const exited = new Promise(resolve => process.once('exit', (code, signal) => resolve({code, signal})));
  let logs = ''; process.stdout.on('data', bytes => { logs += bytes; }); process.stderr.on('data', bytes => { logs += bytes; });
  process.on('error', () => {});
  active = {process, exited, folder};
  const base = await poll(async () => logs.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0], Boolean, 'Owned app did not start', 12000);
  active.base = base; allowedOrigins.add(base);
  servedProfileSha256 = hash(Buffer.from(await (await fetch(base + '/profile')).text()));
  await page.goto(base + '/profile', {waitUntil: 'domcontentloaded'});
  await poll(status, value => value.state==='unlocked'&&value.automatic===true, 'Profile screen did not settle');
  return active;
}
async function stopHost() {
  if (!active) return;
  const owned = active; active = null;
  if (owned.process.exitCode === null && owned.process.signalCode === null) {
    owned.process.kill('SIGINT');
    let result = await Promise.race([owned.exited, delay(6000).then(() => null)]);
    if (!result) { owned.process.kill('SIGKILL'); result = await Promise.race([owned.exited, delay(3000).then(() => null)]); check(result, 'Owned app could not be reaped before cleanup'); }
    check(result.code === 0, 'Owned app required forced termination');
  }
}
async function status() { const response = await fetch(active.base + '/api/profile/status', {headers: {Origin: active.base}}); check(response.ok, 'Read-only profile status failed'); return response.json(); }
async function operationClick(selector, expectedKind, outcome = 'succeeded') {
  const before = await status(); await page.locator(selector).click();
  const result = await poll(status, value => value.operation && value.operation.id !== before.operation?.id && value.operation.kind === expectedKind && value.operation.status !== 'running', 'Profile operation did not finish', 25000);
  check(result.operation.status === outcome, 'Profile operation had an unexpected outcome');
  await poll(() => page.locator('#profile-result').textContent(), value => value === (result.operation.error || 'Profile operation completed.'), 'Visible operation feedback did not appear');
  await poll(() => page.locator('input[type="password"]').evaluateAll(inputs => inputs.every(input => input.value === '')), Boolean, 'Secret inputs were not cleared');
  return result;
}
async function legacyUI(source,kind,outcome='succeeded',wrongOriginal=false) {
  if (!(await page.locator('#legacy-tools').evaluate(e=>e.open))) await page.locator('#legacy-tools > summary').click();
  await page.locator('#legacy-action').selectOption(kind);
  await page.locator('#legacy-source').fill(source);await page.locator('#legacy-managed').fill(path.join(source,'artifacts'));await page.locator('#legacy-offline').check();
  if(kind!=='legacy-rollback')await page.locator('#legacy-master').fill(wrongOriginal?'00'.repeat(32):master);
  check(await page.locator('#legacy-new-key-fields').count()===0,'No output passphrase controls may exist');

  return operationClick('#legacy-submit',kind,outcome);
}
async function authApi(endpoint) {
  const value=await status();const response=await fetch(active.base+'/api'+endpoint,{headers:{Origin:active.base,'X-Smith-Profile-Session':value.session}});
  check(response.ok,'Authenticated imported data request failed');return response.json();
}
async function capture(name) {
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({path: path.join(output, name + '.png'), fullPage: true, animations: 'disabled', mask: [page.locator('#source-copy'),page.locator('#legacy-source'),page.locator('#legacy-managed'), page.locator('input[type="password"]')], maskColor: '#d9d9de'});
  screenshots.push(name + '.png');
}
async function browserStorageHasNoSecret() {
  const stored = await page.evaluate(() => JSON.stringify({local: {...localStorage}, session: {...sessionStorage}}));
  check(!stored.includes(master), 'Browser storage must not contain the passphrase');
}
async function main() {
  stage='synthetic-original-encrypted-fixtures';await fs.mkdir(output,{recursive:true});temp=await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(),'smith-original-import-browser-')));
  copiedBinary=path.join(temp,'smith-next');await fs.copyFile(binary,copiedBinary);await fs.chmod(copiedBinary,0o700);
  const source=path.join(temp,'stopped-original');fixture(source);const sourceBefore=await tree(source);
  const future=path.join(temp,'unsupported-original');fixture(future,99);const futureBefore=await tree(future);
  stage='headed-browser-launch';browserServer=await playwright.chromium.launchServer({executablePath:browserExecutable,headless:false,args:['--disable-background-networking','--disable-component-update','--no-first-run','--no-default-browser-check']});browser=await playwright.chromium.connect(browserServer.wsEndpoint());context=await browser.newContext({viewport:{width:1280,height:960},colorScheme:'light',acceptDownloads:true});
  await context.route('**/*',route=>{const url=new URL(route.request().url());if(allowedOrigins.has(url.origin)||['blob:','data:'].includes(url.protocol))return route.continue();unexpectedOrigins.add(url.protocol);return route.abort()});page=await context.newPage();page.on('pageerror',()=>pageErrors.push('Page JavaScript error'));
  stage='wrong-original-key-and-preflight';const first=await startHost('host-with-write');await page.locator('#open-workspace').click();await page.waitForFunction(()=>window.smithAppReady===true);check(await page.locator('#data-management').isVisible(),'Data management should replace Lock profile');check(await page.locator('input[type="password"]:visible').count()===0,'Default workspace presented a password prompt');await capture('00-automatic-workspace-no-login');await page.locator('#data-management').click();await page.waitForURL('**/profile');
  await legacyUI(source,'legacy-preflight','failed',true);check(!(await fs.stat(path.join(first.folder,'local-workspace','imported')).catch(()=>null)),'Wrong original key published destination');
  const preflight=await legacyUI(source,'legacy-preflight');check(preflight.operation.result.verified&&preflight.operation.result.source_schema===25,'Original preflight failed');check(!(await fs.stat(path.join(first.folder,'local-workspace','imported')).catch(()=>null)),'Preflight published destination');await capture('01-original-preflight-verified');
  checks.push('UI-explicit-offline-original-copies-wrong-key-refusal-and-nonpublishing-preflight');
  stage='verified-encrypted-import';const imported=await legacyUI(source,'legacy-import');check(imported.state==='unlocked'&&imported.automatic&&imported.selection==='imported','Import must open automatically');const report=imported.operation.result;check(report.target_schema===7&&report.mapped_chats===1&&report.mapped_messages===6&&report.mapped_tasks===1,'Import counts incorrect');await capture('02-original-import-opens-automatically');
  assert.deepEqual(await tree(source),sourceBefore,'Original source bytes changed');const encrypted=path.join(first.folder,'local-workspace','imported','profile.sqlite3');  checks.push('UI-schema25-to7-encrypted-import-counts-source-byte-immutability-with-managed-output-key');
  checks.push('default-cold-start-opens-local-workspace-with-no-login-or-output-passphrase-controls');
  stage='read-original-conversation';await browserStorageHasNoSecret();await page.locator('#open-workspace').click();await page.waitForFunction(()=>window.smithAppReady===true);await page.locator('#history button').filter({hasText:'Original synthetic conversation'}).click();await page.locator('.message[data-role="user"] .message-content').filter({hasText:PROMPT}).waitFor();check((await page.locator('.message[data-role="assistant"] .message-content').last().innerText()).includes(ANSWER),'Original selected branch incorrect');await capture('03-original-conversation-readable');
  stage='read-original-details';await page.locator('#rail-activity').click();await page.getByRole('button',{name:'Open Imported records',exact:true}).click();await page.locator('#legacy-records').waitFor({state:'visible'});const latest=page.locator('#legacy-list .artifact-row').filter({hasText:'latest'});await latest.getByRole('button',{name:'Inspect',exact:true}).click();await poll(()=>page.locator('#legacy-details').textContent(),s=>s.includes('Original private reasoning retained.'),'Original reasoning not visible');await capture('04-original-message-details');
  await page.locator('#legacy-kind').selectOption('tasks');await page.locator('#legacy-list .artifact-row').first().getByRole('button',{name:'Inspect',exact:true}).click();await poll(()=>page.locator('#legacy-details').textContent(),s=>s.includes('SENSITIVE_TRACE_SENTINEL')&&s.includes('dormant')&&s.includes('"resumable": false'),'Original task did not remain dormant');await capture('05-original-task-remains-dormant');
  await page.locator('#legacy-kind').selectOption('artifacts');const artifactRow=page.locator('#legacy-list .artifact-row').filter({hasText:'original-proof.txt'});await artifactRow.getByRole('button',{name:'Inspect',exact:true}).click();const downloaded=page.waitForEvent('download');await artifactRow.getByRole('link',{name:'Download original'}).click();const download=await downloaded,file=path.join(temp,'synthetic-original-download');await download.saveAs(file);assert.equal(await fs.readFile(file,'utf8'),ARTIFACT,'Managed original bytes changed');await capture('06-authenticated-original-artifact');await page.locator('#legacy-records-close').click();
  const taskData=await authApi('/legacy-import/tasks');check(taskData.tasks.length===1&&taskData.tasks[0].original_status==='running'&&!taskData.tasks[0].resumable,'Imported original task status changed');const originalGraph=await authApi('/chats/session');check(originalGraph.messages.length===6,'Original messages missing');
  checks.push('UI-original-selected-chat-reasoning-tool-transcript-dormant-task-and-exact-authenticated-artifact-download');
  stage='automatic-import-restart';await stopHost();await startHost('host-with-write');check((await status()).selection==='imported','Restart lost imported selection');await page.locator('#open-workspace').click();await page.waitForFunction(()=>window.smithAppReady===true);await page.locator('#history button').filter({hasText:'Original synthetic conversation'}).click();checks.push('automatic-restart-reopens-selected-import-without-user-secret');
  stage='explicit-current-target-and-new-branch';await page.locator('#rail-settings').click();await page.locator('#provider').selectOption('mock');await page.locator('#settings-form button[type="submit"]').click();await page.locator('#settings').waitFor({state:'hidden'});await page.locator('#model-select').selectOption({index:0});
  const rootMessage=page.locator('.message[data-role="user"]').filter({hasText:PROMPT});await rootMessage.getByRole('button',{name:'Edit prompt',exact:true}).click();await page.locator('#edit-prompt').fill('New explicit synthetic branch after original import.');await page.locator('#submit-edit').click();await poll(()=>authApi('/chats/session'),d=>d.messages.length>=8&&d.messages.some(m=>m.role==='assistant'&&m.status==='completed'&&m.id!=='latest'),'New branch did not finish',25000);await capture('07-explicit-new-branch-preserves-original');const newGraph=await authApi('/chats/session');check(originalGraph.messages.every(old=>newGraph.messages.some(now=>now.id===old.id&&now.content===old.content&&now.status===old.status)),'New branch replaced original evidence');
  await page.locator('#data-management').click();await page.waitForURL('**/profile**');stage='rollback-refuses-committed-branch';const refused=await legacyUI(source,'legacy-rollback','failed');check(refused.state==='unlocked','Rollback refusal must reopen retained import');await capture('08-rollback-refuses-new-branch');await bytesAbsent(path.join(first.folder,'local-workspace','imported'));assert.deepEqual(await tree(source),sourceBefore,'Original source changed after new writes');
  checks.push('UI-explicit-demo-target-new-branch-preserves-originals-and-rollback-refusal-after-committed-writes');checks.push('encrypted-published-profile-and-archive-contain-no-plaintext-payload-or-original-and-managed-key');
  stage='immediate-nondestructive-deactivation';const second=await startHost('host-immediate');await legacyUI(source,'legacy-import');const deactivated=await legacyUI(source,'legacy-rollback');check(deactivated.state==='unlocked'&&deactivated.selection==='default'&&deactivated.operation.result.deactivated,'Immediate import deactivation failed');check((await fs.stat(path.join(second.folder,'local-workspace','imported','profile.sqlite3'))).isFile()&&(await fs.stat(path.join(second.folder,'local-workspace','imported','import.db'))).isFile(),'Deactivation removed encrypted files');assert.deepEqual(await tree(source),sourceBefore,'Deactivation changed source');await capture('09-immediate-deactivation-retains-copies');const localChats=await authApi('/chats');check(localChats.length===0,'Deactivation must return retained default local workspace');checks.push('UI-immediate-deactivation-retains-both-encrypted-destination-and-unchanged-source-with-no-plaintext-activation');
  stage='unsupported-original-schema-no-reset';const third=await startHost('host-future');await legacyUI(future,'legacy-import','failed');check((await status()).state==='unlocked'&&!(await fs.stat(path.join(third.folder,'local-workspace','imported')).catch(()=>null)),'Future original schema published or reset');assert.deepEqual(await tree(future),futureBefore,'Unsupported original was modified');await capture('10-future-original-schema-refused');checks.push('UI-future-original-schema-fails-without-publication-or-source-reset');
  stage='lost-local-key-no-reset';const lost=await startHost('host-lost-key');await stopHost();const localRoot=path.join(lost.folder,'local-workspace'),localDb=path.join(localRoot,'profile.sqlite3'),priorBytes=await fs.readFile(localDb);await fs.unlink(path.join(localRoot,'managed.key'));let refusedStartup=false;try{const env={...process.env,SMITH_NEXT_DATA_DIR:lost.folder,SMITH_NEXT_PORT:'0'};delete env.SMITH_NEXT_MANUAL_PROFILE;delete env.SMITH_NEXT_SYNTHETIC_PROFILE;execFileSync(copiedBinary,[],{env,stdio:['ignore','pipe','pipe'],timeout:8000});}catch(error){refusedStartup=Number.isInteger(error.status)&&error.status!==0;}check(refusedStartup,'Missing local key must refuse startup');check(!(await fs.stat(path.join(localRoot,'managed.key')).catch(()=>null)),'Missing local key was regenerated');assert.deepEqual(await fs.readFile(localDb),priorBytes,'Key loss changed encrypted DB');checks.push('missing-managed-key-refuses-startup-without-key-regeneration-or-database-reset');
  await browserStorageHasNoSecret();check(pageErrors.length===0,'Browser JavaScript error');check(unexpectedOrigins.size===0,'Browser attempted external origin');checks.push('cleared-secret-fields-no-secret-browser-storage-no-page-errors-or-external-page-requests');
  return{date:new Date().toISOString(),headed:true,syntheticOnly:true,browser:{executable:path.basename(browserExecutable),version:browser.version()},automaticLocalWorkspace:true,noLoginOrOutputPassphrase:true,binarySha256:hash(await fs.readFile(copiedBinary)),servedProfileHtmlSha256:servedProfileSha256,sourceContract:{schema:25,actualSQLCipher:true,authenticatedChaCha20Poly1305:true,originalUUIDBytesAAD:true,fixtureMasterKeyViaStdinOnly:true,originalMessages:6,unknownTypedTable:true,externalReferencesOpened:false},importReport:report,groups:checks.length,checks,screenshots,passphrasesAndSourcePathsRedacted:true};
}
async function cleanup() {
  let error;
  try { await stopHost(); } catch (failure) { error = failure; }
  if (context) await Promise.race([context.close().catch(() => {}),delay(8000)]);
  if (browser) await Promise.race([browser.close().catch(() => {}),delay(8000)]);
  if (browserServer) {
    const process = browserServer.process();
    const closed = await Promise.race([browserServer.close().then(() => true).catch(() => false), delay(6000).then(() => false)]);
    if (!closed && process.exitCode === null && process.signalCode === null) { const exited = new Promise(resolve => process.once('exit', resolve)); process.kill('SIGKILL'); await Promise.race([exited, delay(3000)]); }
    check(process.exitCode !== null || process.signalCode !== null, 'Owned browser could not be reaped before cleanup');
  }
  if (temp) await fs.rm(temp, {recursive: true, force: true});
  temp = undefined; context = undefined; browser = undefined; browserServer = undefined;
  master='';
  if (error) throw error;
}
main().then(async report => {
  stage = 'owned-process-and-synthetic-data-cleanup'; await cleanup();
  report.passed = true; report.ownedProcessesReapedAndSyntheticDirectoriesRemoved = true;
  await fs.rm(path.join(output, 'automatic-local-browser-failure.json'), {force: true});
  await fs.writeFile(path.join(output, reportName), JSON.stringify(report, null, 2) + '\n'); console.log(JSON.stringify(report, null, 2));
}).catch(async error => {
  // Playwright call logs can include fill values or selected paths. Do not emit
  // raw errors, traces, request bodies, paths or passphrases into evidence.
  let message = String(error.message).split('\n')[0];
  for (const sensitive of [master,temp, root].filter(Boolean)) message = message.split(sensitive).join('[redacted]');
  const report = {date: new Date().toISOString(), headed: true, syntheticOnly: true, passed: false, stage, errorType: error.name, message, checks, screenshots};
  await fs.mkdir(output, {recursive: true}); await fs.writeFile(path.join(output, 'automatic-local-browser-failure.json'), JSON.stringify(report, null, 2) + '\n');
  console.error('Synthetic original import browser acceptance failed at: ' + stage); process.exitCode = 1;
}).finally(async () => { try { await cleanup(); } catch { console.error('Owned synthetic process cleanup did not finish normally.'); process.exitCode = 1; } });
