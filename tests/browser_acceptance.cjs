// Actual rendered-browser checks with a disposable profile and synthetic Demo data.
// Requires Playwright and a locally installed Chromium-family browser; no downloads.
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const {spawn} = require('node:child_process');
let playwright;
try { playwright = require('playwright'); }
catch { playwright = require(process.env.PLAYWRIGHT_MODULE || '/opt/homebrew/lib/node_modules/playwright'); }
const root = path.resolve(__dirname, '..');
const binary = path.resolve(process.env.SMITH_BROWSER_BINARY || path.join(root, 'target/debug/smith-next'));
const executablePath = process.env.SMITH_BROWSER_EXECUTABLE || '/Applications/Brave Browser.app/Contents/MacOS/Brave Browser';
const output = path.resolve(process.env.SMITH_BROWSER_OUTPUT || path.join(root, 'docs/screenshots/current-design'));
const checks = [], errors = [], blocked = [];
let child, context;
const delay = ms => new Promise(r => setTimeout(r, ms));
async function terminal(page) {
  await page.locator('#cancel-button').waitFor({state:'hidden', timeout:15000});
  await page.locator('.message[data-role="assistant"]').last().waitFor();
  assert.equal(await page.locator('.message[data-status="streaming"]').count(), 0);
}
async function light(page, scope = '') {
  await page.evaluate(()=>document.fonts.ready);
  const data = await page.evaluate(scope => {
    const selectors = ['html','.app','.sidebar','.main','#prompt','#model-select',...(scope?[scope]:[])];
    return {toggle:!!document.getElementById('theme-toggle'), darkClass:document.body.classList.contains('dark'),
      scheme:getComputedStyle(document.documentElement).colorScheme,
      styles:selectors.map(s=>({selector:s,background:getComputedStyle(document.querySelector(s)).backgroundColor,color:getComputedStyle(document.querySelector(s)).color,colorScheme:getComputedStyle(document.querySelector(s)).colorScheme})),
      width:innerWidth, scroll:document.documentElement.scrollWidth,
      inter:document.fonts.check('16px Inter'),poppins:[...document.fonts].some(f=>f.family==='Poppins'&&f.status==='loaded'),mono:document.fonts.check('12px "Spline Sans Mono"')};
  },scope);
  assert.equal(data.toggle,false);assert.equal(data.darkClass,false);assert.equal(data.scheme,'light');
  assert(data.inter&&data.poppins&&data.mono,JSON.stringify(data));assert(data.scroll<=data.width+1,JSON.stringify(data));
  for(const style of data.styles) assert.equal(style.colorScheme,'light',style.selector);
  assert.equal(data.styles.find(s=>s.selector==='#prompt').color,'rgb(0, 0, 0)');
  return data;
}
async function capture(page,name) {
  await page.evaluate(()=>document.fonts.ready);
  await page.screenshot({path:path.join(output,name+'.png'),fullPage:true,animations:'disabled'});
}
async function main() {
  await fs.mkdir(output,{recursive:true});
  const temp = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(),'smith-browser-acceptance-')));
  let logs='';
  try {
    child=spawn(binary,[],{env:{...process.env,SMITH_NEXT_DATA_DIR:path.join(temp,'app'),SMITH_NEXT_PORT:'0'},stdio:['ignore','pipe','pipe']});
    child.stdout.on('data',b=>logs+=b);child.stderr.on('data',b=>logs+=b);
    child.on('error',e=>{logs+=e.message;});
    let base;
    for(let i=0;i<200;i++) {base=logs.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0];if(base)break;if(child.exitCode!==null)throw Error(logs);await delay(25);}
    assert(base,'Disposable app failed to start: '+logs);
    context=await playwright.chromium.launchPersistentContext(path.join(temp,'browser'),{executablePath,headless:process.env.SMITH_BROWSER_HEADLESS==='1',viewport:{width:1280,height:860},colorScheme:'light',acceptDownloads:true,args:['--disable-background-networking','--disable-component-update','--no-first-run','--no-default-browser-check']});
    await context.route('**/*',route=>{
      const url=route.request().url();
      if(url.startsWith(base+'/')||url.startsWith('blob:')||url.startsWith('data:'))return route.continue();
      blocked.push(url);return route.abort();
    });
    await context.addInitScript(()=>localStorage.setItem('smith-next:theme','dark'));
    const page=await context.newPage();page.on('pageerror',e=>errors.push(e.message));
    await page.goto(base,{waitUntil:'networkidle'});await page.locator('#passphrase').fill('Synthetic browser design fixture key');await page.locator('#passphrase-confirm').fill('Synthetic browser design fixture key');await page.locator('#access-submit').click();await page.locator('#open-workspace').waitFor({state:'visible',timeout:45000});await page.locator('#open-workspace').click();await page.waitForFunction(()=>window.smithAppReady&&window.smithNavigationReady);await page.locator('#model-select option').first().waitFor({state:'attached'});
    await page.evaluate(()=>document.fonts.ready);
    const lightStyles=await light(page);await capture(page,'after-desktop-light');
    await page.emulateMedia({colorScheme:'dark'});await page.reload({waitUntil:'networkidle'});await page.waitForFunction(()=>window.smithAppReady&&window.smithNavigationReady);
    const darkStyles=await light(page);assert.deepEqual(darkStyles,lightStyles);await capture(page,'after-desktop-system-dark');
    checks.push('light-only-identical-computed-styles-under-light-dark-and-stored-dark-preference');
    await page.locator('#connection-button').click();await page.locator('#settings').waitFor();await light(page,'#settings');
    await capture(page,'after-connection-dialog');await page.keyboard.press('Escape');assert.equal(await page.locator('#settings').isVisible(),false);
    checks.push('light-native-dialog-controls-and-escape-close');
    await page.locator('#prompt').fill('Synthetic browser acceptance: preserve every branch.');
    await page.locator('#attachment-input').setInputFiles({name:'synthetic-note.txt',mimeType:'text/plain',buffer:Buffer.from('Synthetic attachment with Unicode: Привет 🦀\nOnly test data.')});
    await page.getByRole('button',{name:'Remove attachment synthetic-note.txt'}).waitFor();
    await page.locator('#send-button').click();await terminal(page);
    await page.locator('summary').filter({hasText:'Attached text · synthetic-note.txt'}).waitFor();
    const originalText=await page.locator('.message[data-role="user"] .message-content').first().innerText();
    await capture(page,'after-chat-attachment');checks.push('demo-send-stream-completion-and-durable-text-attachment');
    await page.locator('[data-action="edit"]').first().click();await page.locator('#edit-prompt').fill('Edited synthetic browser acceptance: retain the original.');
    await page.locator('#submit-edit').click();await page.locator('#edit-message').waitFor({state:'hidden'});await terminal(page);
    assert.match(await page.locator('.message[data-role="user"] .message-content').first().innerText(),/^Edited synthetic/);
    await page.locator('[data-action="previous-branch"]').first().click();
    await page.waitForFunction(text=>document.querySelector('.message[data-role="user"] .message-content')?.textContent===text,originalText);
    await page.locator('[data-action="next-branch"]').first().click();
    await page.waitForFunction(()=>document.querySelector('.message[data-role="user"] .message-content')?.textContent.startsWith('Edited synthetic'));
    await page.locator('[data-action="regenerate"]').last().click();await terminal(page);
    await capture(page,'after-branches');checks.push('edit-preserves-original-branch-navigation-and-regenerate');
    await page.locator('[data-action="save"]').last().click();await page.waitForFunction(()=>document.querySelector('#artifact-count').textContent==='1');
    await page.locator('#prompt').fill('Synthetic follow-up after saving an artifact.');await page.locator('#send-button').click();await terminal(page);checks.push('send-after-artifact-save-uses-authoritative-updated-version');
    await page.locator('#manage-chat').click();await page.locator('#conversation-title').fill('Synthetic acceptance conversation');await page.locator('#save-title').click();
    await page.waitForFunction(()=>document.querySelector('#chat-title').textContent==='Synthetic acceptance conversation');
    const downloaded=page.waitForEvent('download');await page.locator('#export-button').click();const file=await downloaded;
    const exported=await fs.readFile(await file.path(),'utf8');assert(exported.includes(originalText));assert(exported.includes('Edited synthetic'));assert(exported.includes('Привет 🦀'));
    checks.push('rename-after-generation-and-export-includes-branches-and-attachment');
    await page.locator('#manage-chat').click();await page.locator('#request-delete').click();await page.waitForFunction(()=>document.querySelectorAll('#history .history-row').length===0);await page.locator('#open-trash').click();await light(page,'#trash');await capture(page,'after-trash-dialog');await page.keyboard.press('Escape');await page.locator('#action-undo').click();await page.locator('#history .history-row>button').first().click();await page.waitForFunction(()=>document.querySelectorAll('.message').length>0);
    assert.equal(await page.locator('[data-action=edit]').first().isEnabled(),true);assert.equal(await page.locator('[data-action=regenerate]').last().isEnabled(),true);
    await page.locator('#history button').first().click();await page.waitForFunction(()=>[...document.querySelectorAll('[data-action=edit],[data-action=regenerate]')].every(b=>!b.disabled));assert.equal(await page.locator('[data-action=edit]').first().isEnabled(),true);assert.equal(await page.locator('[data-action=regenerate]').last().isEnabled(),true);checks.push('branch-controls-reenabled-after-management-and-history-load');
    for(const viewport of [{width:375,height:812},{width:768,height:1024},{width:1440,height:900}]){await page.setViewportSize(viewport);await light(page);const boxes=await page.evaluate(()=>{const a=document.querySelector('#chat-title').getBoundingClientRect(),b=document.querySelector('.top-actions').getBoundingClientRect();return {separate:a.right<=b.left||a.bottom<=b.top||a.left>=b.right||a.top>=b.bottom}});assert(boxes.separate,'Header actions overlap the conversation title');await capture(page,'after-'+viewport.width+'px-chat');}checks.push('375px-768px-1440px-no-horizontal-overflow');
    await page.setViewportSize({width:390,height:844});await light(page);await capture(page,'after-narrow-chat');
    await page.locator('#menu-toggle').click();assert.equal(await page.locator('#main-content').evaluate(e=>e.inert),true);
    await page.locator('#connection-button').focus();await page.keyboard.press('Tab');assert.equal(await page.evaluate(()=>document.activeElement.id),'close-navigation');
    await page.keyboard.press('Escape');assert.equal(await page.locator('#main-content').evaluate(e=>e.inert),false);assert.equal(await page.evaluate(()=>document.activeElement.id),'menu-toggle');
    checks.push('390px-layout-no-horizontal-overflow-navigation-focus-trap-and-return');
    await page.locator('#menu-toggle').click();await page.locator('#connection-button').click();await light(page,'#settings');await capture(page,'after-narrow-connection');await page.keyboard.press('Escape');
    await page.setViewportSize({width:1280,height:860});await page.locator('#new-chat').click();await page.locator('#prompt').fill('Synthetic cancel acceptance.');await page.locator('#send-button').click();await page.locator('#cancel-button').click();await terminal(page);
    assert.equal(await page.locator('.message[data-status="cancelled"]').count(),1);checks.push('actual-browser-stop-response-retains-cancelled-attempt');
    assert.deepEqual(errors,[]);assert.deepEqual(blocked,[]);checks.push('no-page-javascript-errors-and-no-page-external-requests');
    const report={date:new Date().toISOString(),app_sha256:require('node:crypto').createHash('sha256').update(await fs.readFile(binary)).digest('hex'),browser:await context.browser().version(),executable:path.basename(executablePath),headed:process.env.SMITH_BROWSER_HEADLESS!=='1',syntheticOnly:true,checks,computedLight:lightStyles,computedSystemDark:darkStyles,screenshots:(await fs.readdir(output)).filter(name=>name.startsWith('after-'))};
    await fs.writeFile(path.join(output,'browser-acceptance.json'),JSON.stringify(report,null,2)+'\n');console.log(JSON.stringify(report,null,2));
  } finally {
    if(context)await context.close();
    if(child&&child.exitCode===null){child.kill('SIGINT');await Promise.race([new Promise(r=>child.once('exit',r)),delay(4000)]);if(child.exitCode===null)child.kill('SIGKILL');}
    await fs.rm(temp,{recursive:true,force:true});
  }
}
main().catch(e=>{console.error(e.stack);process.exitCode=1});
