'use strict';
// Actual headed Brave journey against the normal encrypted app. Only disposable,
// synthetic profile/browser data and self-authored fixtures; no provider traffic.
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
const executablePath = process.env.SMITH_BROWSER_EXECUTABLE || '/Applications/Brave Browser.app/Contents/MacOS/Brave Browser';
const output = path.resolve(process.env.SMITH_MEDIA_BROWSER_OUTPUT || path.join(root, 'docs/screenshots/media-journey'));
const fixtures = path.join(root, 'crates/smith-media/tests/fixtures');
const sha256 = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const checks = [], screenshots = [], downloads = [], typedErrors = [], pageErrors = [], externalRequests = [];
let child, context, page, base, temp, copiedBinary, currentStage = 'setup', secret, appLogs = '';
function stage(name) { currentStage = name; console.log('Document browser: ' + name); }
function sanitize(value) {
  let text = String(value);
  for (const sensitive of [secret, temp, root, executablePath].filter(Boolean)) text = text.split(sensitive).join('[redacted]');
  return text.replace(/\u001b\[[0-9;]*m/g, '').replace(/http:\/\/127\.0\.0\.1:\d+/g, '[owned loopback]').replace(/profile_session=[^\s&"']+/g, 'profile_session=[redacted]');
}
async function eventually(test, label, timeout = 20000) {
  const until = Date.now() + timeout;
  while (Date.now() < until) { if (await test()) return; await delay(80); }
  throw new Error('Timed out: ' + label);
}
async function startApp() {
  appLogs = '';
  const env = {...process.env, SMITH_NEXT_DATA_DIR:path.join(temp, 'app'), SMITH_NEXT_PORT:'0'};
  delete env.SMITH_NEXT_SYNTHETIC_PROFILE;
  child = spawn(copiedBinary, [], {env, stdio:['ignore','pipe','pipe']});
  child.stdout.on('data', data => { appLogs = (appLogs + data).slice(-16384); });
  child.stderr.on('data', data => { appLogs = (appLogs + data).slice(-16384); });
  child.on('error', error => { appLogs = error.message; });
  await eventually(() => {
    const found = appLogs.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0];
    if (found) { base = found; return true; }
    if (child.exitCode !== null) throw new Error('Owned app failed to start: ' + sanitize(appLogs));
    return false;
  }, 'normal encrypted app listener');
  assert.match(appLogs, /encrypted profile starts locked/);
}
async function stopApp() {
  if (!child || child.exitCode !== null) return;
  const stopped = new Promise(resolve => child.once('exit', resolve));
  child.kill('SIGINT');
  await Promise.race([stopped, delay(8000)]);
  if (child.exitCode === null) { child.kill('SIGKILL'); await stopped; throw new Error('Owned app did not complete graceful shutdown'); }
}
async function readApi(route) {
  return page.evaluate(async route => {
    const response = await sessionFetch('/api' + route);
    if (!response.ok) throw new Error('Read-only verification failed: ' + response.status);
    return response.json();
  }, route);
}
async function capture(name) {
  // No profile screen capture: a passphrase or session token never enters evidence.
  assert(await page.locator('.app').isVisible());
  assert.equal(await page.locator('input[type=password]').evaluateAll(inputs => inputs.some(input => input.value)), false);
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({path:path.join(output,name + '.png'), fullPage:true, animations:'disabled'});
  screenshots.push(name + '.png');
}
async function openWorkspace(create) {
  await page.goto(base + '/profile', {waitUntil:'domcontentloaded'});
  await page.locator('#access-form').waitFor({state:'visible'});
  assert.match(await page.locator('#access-title').innerText(), create ? /Create/ : /Unlock/);
  await page.locator('#passphrase').fill(secret);
  if (create) await page.locator('#passphrase-confirm').fill(secret);
  await page.locator('#access-submit').click();
  await page.locator('#open-workspace').waitFor({state:'visible',timeout:30000});
  assert.equal(await page.locator('#passphrase').inputValue(), '');
  assert.equal(await page.locator('#passphrase-confirm').inputValue(), '');
  await page.locator('#open-workspace').click();
  await page.waitForFunction(() => window.smithAppReady === true);
  const config = await readApi('/config');
  assert.equal(config.provider, 'mock', 'Acceptance uses offline Demo only');
  assert.equal(await page.evaluate(() => Object.keys(localStorage).some(key => key.startsWith('smith-next:draft:'))), false);
}
async function openDocuments() {
  if (!await page.locator('#documents').isVisible()) await page.locator('#documents-button').click();
  await page.locator('#documents').waitFor({state:'visible'});
}
async function upload(name, mimeType, buffer, expectedCode) {
  await openDocuments();
  await page.locator('#document-upload').setInputFiles({name,mimeType,buffer});
  const received = page.waitForResponse(response => new URL(response.url()).pathname === '/api/media/uploads' && response.request().method() === 'POST');
  await page.locator('#document-upload-submit').click();
  const response = await received, body = await response.json();
  if (expectedCode) {
    assert.equal(body.code, expectedCode);
    assert(!response.ok());
    await page.waitForFunction(message => document.getElementById('document-result').textContent === message, body.error);
    typedErrors.push({file:name, status:response.status(), code:body.code, displayedMessage:body.error});
    return body;
  }
  assert.equal(response.status(), 200, JSON.stringify(body));
  await page.waitForFunction(name => document.getElementById('document-name').textContent === name, name);
  await page.locator('#document-attach').waitFor({state:'visible'});
  assert.equal(body.sha256, sha256(buffer));
  assert.equal(body.byte_len, buffer.length);
  return body;
}
async function downloadLink(locator, expected, metadata) {
  const received = page.waitForEvent('download');
  await locator.click();
  const downloaded = await received;
  assert.equal(await downloaded.failure(), null);
  const bytes = await fs.readFile(await downloaded.path());
  if (expected) assert.deepEqual(bytes, expected);
  if (metadata) {
    assert.equal(sha256(bytes), metadata.sha256);
    assert.equal(bytes.length, metadata.byte_len);
    assert.equal(downloaded.suggestedFilename(), metadata.name);
  }
  downloads.push({name:downloaded.suggestedFilename(), bytes:bytes.length, sha256:sha256(bytes)});
  return bytes;
}
async function attach() {
  await page.locator('#document-attach').click();
  await page.locator('#documents').waitFor({state:'hidden'});
}
async function terminal() {
  await page.locator('.message[data-role=assistant]').last().waitFor();
  await page.locator('#cancel-button').waitFor({state:'hidden',timeout:20000});
  assert.equal(await page.locator('.message[data-status=streaming]').count(),0);
  assert.equal(await page.locator('.message[data-role=assistant]').last().getAttribute('data-status'),'completed');
}
async function generate(format, tableText, linked) {
  await openDocuments();
  if (!await page.locator('#document-generator').evaluate(element => element.open)) await page.locator('#document-generator > summary').click();
  await page.locator('#document-format').selectOption(format);
  const name = 'generated-table.' + ({csv:'csv',markdown:'md',xlsx:'xlsx'}[format]);
  await page.locator('#document-output-name').fill(name);
  await page.locator('#document-table').fill(tableText);
  await page.locator('#document-link-chat').setChecked(linked);
  const received = page.waitForResponse(response => new URL(response.url()).pathname === '/api/media/generate' && response.request().method() === 'POST');
  await page.locator('#document-generate').click();
  const response = await received, body = await response.json();
  assert.equal(response.status(),200,JSON.stringify(body));
  await page.waitForFunction(name => document.getElementById('document-name').textContent === name, name);
  assert.equal(body.document.name,name);
  const bytes = await downloadLink(page.locator('#document-original'),null,body.document);
  return {body,bytes};
}
function verifyGeneratedXlsx(bytes) {
  // Independent Python stdlib ZIP/XML structure check; no package installation.
  const script = `import io, sys, json, zipfile, xml.etree.ElementTree as ET\nb=sys.stdin.buffer.read()\nn='{http://schemas.openxmlformats.org/spreadsheetml/2006/main}'\nwith zipfile.ZipFile(io.BytesIO(b)) as z:\n assert z.testzip() is None\n parts={p:ET.fromstring(z.read(p)) for p in z.namelist()}\n cells={c.attrib['r']:c for c in parts['xl/worksheets/sheet1.xml'].iter(n+'c')}\n assert all(c.attrib['t']=='inlineStr' for c in cells.values())\n assert cells['A2'].find(n+'is/'+n+'t').text=='00123'\n assert cells['A3'].find(n+'is/'+n+'t').text=='00007'\n assert cells['B2'].find(n+'is/'+n+'t').text=='Résumé 🦀'\n assert cells['B3'].find(n+'is/'+n+'t').text=='<literal> & pipes |'\n assert all(p.find('.//'+n+'f') is None for p in parts.values())\n print(json.dumps({'zip_crc':True,'typed_text_cells':True,'leading_zeros':True,'unicode':True,'formulas_absent':True}))\n`;
  return JSON.parse(execFileSync('python3',['-c',script],{input:bytes,encoding:'utf8',timeout:10000,maxBuffer:4096}));
}
async function main() {
  await fs.mkdir(output,{recursive:true});
  temp = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(),'smith-media-browser-')));
  secret = 'synthetic-' + crypto.randomUUID() + '-fixture-only';
  copiedBinary = path.join(temp,'smith-next');
  await fs.copyFile(binary,copiedBinary); await fs.chmod(copiedBinary,0o700);
  const binaryHash = sha256(await fs.readFile(copiedBinary));
  let report;
  try {
    stage('create normal encrypted profile'); await startApp();
    context = await playwright.chromium.launchPersistentContext(path.join(temp,'browser'),{
      executablePath,headless:false,viewport:{width:1280,height:900},colorScheme:'light',acceptDownloads:true,
      args:['--disable-background-networking','--disable-component-update','--no-first-run','--no-default-browser-check']
    });
    context.setDefaultTimeout(15000);
    await context.route('**/*',route => {
      const url = route.request().url();
      if (url.startsWith(base + '/') || url.startsWith('blob:') || url.startsWith('data:')) return route.continue();
      externalRequests.push(new URL(url).protocol); return route.abort();
    });
    page = await context.newPage(); page.on('pageerror',error => pageErrors.push(sanitize(error.message)));
    await openWorkspace(true);
    const profile = await page.evaluate(async()=>{const r=await fetch('/api/profile/status');return r.json()});
    assert.equal(profile.encrypted,true); assert.equal(profile.synthetic,undefined);
    checks.push('normal encrypted profile created through UI; secrets cleared; offline Demo only');

    stage('upload and review truncated CSV');
    const csvBytes = Buffer.from('identifier,note\n00123,"Résumé 🦀 '+ 'a'.repeat(21000) +'"\n');
    const csv = await upload('synthetic-context.csv','text/csv',csvBytes);
    assert.equal(csv.content.truncated,true); assert.equal(csv.content.included_context_chars,20000);
    assert(csv.content.total_context_chars>csv.content.included_context_chars);
    assert.match(await page.locator('#document-description').innerText(),/Excerpt only: 20000 of/);
    assert.equal(await page.locator('#document-context').innerText(),csv.content.context);
    await downloadLink(page.locator('#document-original'),csvBytes,csv);
    await capture('01-csv-truncated-context'); await attach();
    checks.push('CSV full original preserved; explicit Unicode-safe 20000-character excerpt shown and attached');

    stage('upload XLSX and reference-only image');
    const xlsxBytes = await fs.readFile(path.join(fixtures,'cells.xlsx'));
    const xlsx = await upload('synthetic-cells.xlsx','application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',xlsxBytes);
    const workbook = JSON.parse(xlsx.content.context);
    assert.equal(workbook.sheets[0].cells.find(cell=>cell.row===2&&cell.column===1).value.value,'00123');
    assert.equal(workbook.sheets[0].cells.find(cell=>cell.row===2&&cell.column===2).value.value,'123.50');
    assert.equal(workbook.sheets[1].visibility,'hidden');
    await downloadLink(page.locator('#document-original'),xlsxBytes,xlsx);
    await capture('02-xlsx-typed-context'); await attach();
    const pngBytes = await fs.readFile(path.join(fixtures,'pixels.png'));
    const png = await upload('synthetic-reference.png','image/png',pngBytes);
    assert.deepEqual(png.allowed_delivery,['reference_only']);
    assert.match(await page.locator('#document-description').innerText(),/pixels are not sent to the model/);
    assert.equal(await page.locator('#document-attach').innerText(),'Attach image reference only');
    await capture('03-image-reference-only'); await attach();
    const expectedRefs = [csv,xlsx,png].map((file,index)=>({id:file.id,delivery:index===2?'reference_only':'context'}));
    assert.deepEqual((await readApi('/drafts/new')).media,expectedRefs);
    checks.push('XLSX typed context/hidden sheet preserved; image explicitly attached as reference only; draft encrypted');

    stage('send and edit immutable attachment branches');
    await page.locator('#prompt').fill('Synthetic document journey: keep these three originals.');
    await page.locator('#send-button').click(); await terminal();
    const chats = await readApi('/chats'); assert.equal(chats.length,1); const chatId = chats[0].id;
    let conversation = await readApi('/chats/'+chatId);
    const firstUser = conversation.active_messages.find(message=>message.role==='user');
    assert.deepEqual(firstUser.media.map(file=>({id:file.id,delivery:file.delivery})),expectedRefs);
    assert.match(firstUser.media_context,/Excerpt: 20000 of/);
    assert.match(firstUser.media_context,/image pixels are not provided to the model/);
    assert.match(await page.locator('.message[data-role=assistant] .message-content').last().innerText(),/offline demonstration/);
    await capture('04-message-attachment-references');
    await page.locator('[data-action=edit]').first().click();
    await page.locator('#edit-prompt').fill('Edited synthetic document journey: retain the same originals.');
    await page.locator('#submit-edit').click(); await page.locator('#edit-message').waitFor({state:'hidden'}); await terminal();
    conversation = await readApi('/chats/'+chatId);
    const editedUser = conversation.active_messages.find(message=>message.role==='user');
    assert.notEqual(editedUser.id,firstUser.id);
    assert.deepEqual(editedUser.media,firstUser.media);
    assert.equal(editedUser.media_context,firstUser.media_context);
    const retained = conversation.messages.find(message=>message.id===firstUser.id);
    assert.deepEqual(retained.media,firstUser.media);
    await page.locator('[data-action=previous-branch]').first().click();
    await page.waitForFunction(()=>document.querySelector('.message[data-role=user] .message-content')?.textContent.startsWith('Synthetic document journey'));
    await page.locator('[data-action=next-branch]').first().click();
    await page.waitForFunction(()=>document.querySelector('.message[data-role=user] .message-content')?.textContent.startsWith('Edited synthetic document journey'));
    checks.push('send stores three immutable references; edit retains excerpts/originals; both sibling branches remain selectable');

    stage('generate CSV Markdown and XLSX artifacts');
    const tableText='Code\tNote\n00123\tRésumé 🦀\n00007\t<literal> & pipes |';
    const generated=[]; let xlsxStructure;
    for(const format of ['csv','markdown','xlsx']) {
      const item=await generate(format,tableText,format==='xlsx'); generated.push(item);
      if(format==='csv')assert.equal(item.bytes.toString(),'Code,Note\n00123,Résumé 🦀\n00007,<literal> & pipes |\n');
      if(format==='markdown'){assert.match(item.bytes.toString(),/00123/);assert.match(item.bytes.toString(),/&lt;literal&gt;/);assert.match(item.bytes.toString(),/\\\|/)}
      if(format==='xlsx')xlsxStructure=verifyGeneratedXlsx(item.bytes);
    }
    await capture('05-generated-xlsx-artifact');
    await page.locator('#close-documents').click(); await page.locator('#artifacts-button').click(); await page.locator('#library-artifacts').click();
    await eventually(async()=>await page.locator('#artifact-list .artifact-row').count()===3,'generated artifact library');
    for(const {body,bytes} of generated) {
      const row=page.locator('#artifact-list .artifact-row').filter({hasText:body.document.name});
      await downloadLink(row.getByRole('link',{name:'Download'}),bytes,body.document);
      await row.getByRole('button',{name:'Open',exact:true}).click();
      await page.locator('#artifact-preview').waitFor({state:'visible'});
      assert.equal(await page.locator('#preview-content').innerText(),body.document.content.context);
      await page.locator('[data-close=artifact-preview]').click();
    }
    await capture('06-artifact-library'); await page.locator('[data-close=artifacts]').click();
    checks.push('real CSV/Markdown/XLSX generated through UI; library and original downloads byte-identical; independent XLSX ZIP/XML checks pass');

    stage('typed unsupported and malformed document failures');
    await upload('unsupported.pdf','application/pdf',Buffer.from('%PDF-1.7\nsynthetic unsupported fixture'),'media_unsupported');
    await upload('malformed.json','application/json',Buffer.from('{"unclosed":'),'media_invalid_content');
    await capture('07-malformed-document-error');
    assert.equal((await readApi('/media/artifacts')).artifacts.length,3);
    await page.locator('#close-documents').click();
    checks.push('unsupported PDF and malformed JSON show typed API errors in UI without creating artifacts');

    stage('restart and unlock same encrypted profile');
    await page.locator('#lock-workspace').click(); await page.locator('#access-form').waitFor({state:'visible'});
    await page.goto('about:blank'); await stopApp();
    const encryptedBytes=await fs.readFile(path.join(temp,'app','profile.sqlite3'));
    assert(!encryptedBytes.subarray(0,16).equals(Buffer.from('SQLite format 3\0')));
    for(const marker of [Buffer.from('synthetic-context.csv'),Buffer.from('Synthetic document journey'),csvBytes,pngBytes])assert(!encryptedBytes.includes(marker));
    const profileFiles=await fs.readdir(path.join(temp,'app'));
    assert(!profileFiles.includes('attachments'));assert(!profileFiles.includes('.staging'));
    await startApp(); await openWorkspace(false);
    await page.locator('#history button').filter({hasText:'Synthetic document journey'}).first().click();
    await page.waitForFunction(()=>document.querySelector('.message[data-role=user] .message-content')?.textContent.startsWith('Edited synthetic document journey'));
    const reopened=await readApi('/chats/'+chatId);
    assert.deepEqual(reopened.active_messages.find(message=>message.role==='user').media,editedUser.media);
    const article=page.locator('.message[data-role=user]').first();
    for(const [index,file] of [csv,xlsx,png].entries()) {
      const details=article.locator('details').filter({has:page.locator('summary').filter({hasText:file.name})});
      await details.locator('summary').click();await details.getByRole('button',{name:'Review original and context'}).click();
      await page.locator('#documents').waitFor({state:'visible'});
      await page.waitForFunction(name=>document.getElementById('document-name').textContent===name,file.name);
      await downloadLink(page.locator('#document-original'),[csvBytes,xlsxBytes,pngBytes][index],file);
      await page.locator('#close-documents').click();
    }
    await capture('08-reopened-attachment-branches');
    await page.locator('#artifacts-button').click();await page.locator('#library-artifacts').click();
    await eventually(async()=>await page.locator('#artifact-list .artifact-row').count()===3,'reopened artifact library');
    for(const {body,bytes} of generated)await downloadLink(page.locator('#artifact-list .artifact-row').filter({hasText:body.document.name}).getByRole('link',{name:'Download'}),bytes,body.document);
    checks.push('process restarted; UI unlock restores immutable branch media and all generated originals exactly; ciphertext has no fixture plaintext/attachment sidecars');
    assert.deepEqual(pageErrors,[]);assert.deepEqual(externalRequests,[]);
    assert.equal(await page.evaluate(()=>Object.keys(localStorage).some(key=>key.startsWith('smith-next:draft:'))),false);
    checks.push('no page JavaScript errors, no external page requests, no plaintext browser draft storage');
    report={result:'pass',date:new Date().toISOString(),browser:await context.browser().version(),headed:true,normalEncryptedProfile:true,syntheticOnly:true,provider:'offline Demo (no inference)',binarySha256:binaryHash,checks,typedErrors,downloads,xlsxStructure,screenshots,limits:['Screenshots and script exercise rendered UI; offline Demo does not validate model comprehension.','Independent XLSX checks use Python stdlib ZIP/XML, not Excel/LibreOffice/openpyxl.','Browser routing guards page requests; browser-vendor background traffic is disabled by flags but is not a packet-capture claim.']};
  } catch(error) {
    report={result:'fail',stage:currentStage,error:sanitize(error.message),checks,typedErrors,pageErrors,externalRequestCount:externalRequests.length,screenshots,syntheticOnly:true,headed:true};
    process.exitCode=1;
  } finally {
    if(context)await context.close();
    try{await stopApp()}catch(error){report.cleanupError=sanitize(error.message);process.exitCode=1}
    if(temp)await fs.rm(temp,{recursive:true,force:true});
    secret='';
    await fs.writeFile(path.join(output,'media-browser-acceptance.json'),JSON.stringify(report,null,2)+'\n');
    console.log(JSON.stringify(report,null,2));
  }
}
main().catch(error=>{console.error(sanitize(error.message));process.exitCode=1});
