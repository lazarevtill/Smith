'use strict';
// Independent frontend review. Executes the actual inline scripts against owned,
// synthetic DOM/fetch state. No browser profile, filesystem data or network use.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const test = require('node:test');

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const ok = data => ({ ok: true, status: 200, json: async () => data, text: async () => String(data) });
const fail = (status, error) => ({ ok: false, status, json: async () => ({ error }), text: async () => error });
const flush = () => new Promise(resolve => setImmediate(resolve));

function harness(page = 'index.html') {
  const html = fs.readFileSync(path.join(__dirname, '../web', page), 'utf8');
  let script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
  // Initialization requests are tested separately from these deterministic races.
  if (page === 'index.html') script = script.slice(0, script.lastIndexOf('(async()=>{'));
  else script = script.replace(/refresh\(\);\s*$/, '');
  const elements = new Map(), requests = [], sources = [], replacements = [];
  const timers = new Map(), storage = {}, listeners = new Map();
  let nextTimer = 0, destroyed = false, handler;
  class El {
    constructor(id) { this.id = id; this.children = []; this.dataset = {}; this.style = {}; this.value = ''; this.textContent = ''; this.hidden = false; this.disabled = false; this.open = false; this.className = ''; const classes = new Set(); this.classList = { add: x => classes.add(x), remove: x => classes.delete(x), contains: x => classes.has(x), toggle(x, value) { const on = value === undefined ? !classes.has(x) : value; if (on) classes.add(x); else classes.delete(x); return on; } }; }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = [...children]; }
    setAttribute(key, value) { this[key] = value; }
    removeAttribute(key) { delete this[key]; }
    addEventListener() {}
    focus() { document.activeElement = this; }
    contains(el) { return this === el || this.children.some(child => child.contains?.(el)); }
    querySelector() { return new El(); }
    querySelectorAll() { return this.children; }
    showModal() { this.open = true; }
    close() { this.open = false; }
    get options() { return this.children; }
  }
  for (const [, id] of html.matchAll(/\bid="([^"]+)"/g)) elements.set(id, new El(id));
  const app = new El('app'), body = new El('body'); body.children = [app];
  body.replaceChildren = (...children) => { body.children = children; destroyed = true; };
  const document = {
    body, activeElement: null,
    getElementById: id => destroyed ? null : elements.get(id) || null,
    createElement: () => new El(),
    querySelector: selector => destroyed ? null : selector === '.app' ? app : new El(),
    querySelectorAll: selector => destroyed ? [] : selector === 'input,textarea' ? [...elements.values()] : selector === '[data-profile-secret]' ? [...html.matchAll(/id="([^"]+)" data-profile-secret/g)].map(m=>elements.get(m[1])) : [],
    addEventListener() {}
  };
  const localStorage = {
    getItem: key => storage[key] ?? null,
    setItem(key, value) { storage[key] = String(value); Object.defineProperty(this, key, { value: String(value), enumerable: true, configurable: true }); },
    removeItem(key) { delete storage[key]; delete this[key]; }
  };
  const location = { replace: value => replacements.push(value), reload() {}, href: '' };
  const context = vm.createContext({
    console, document, location, localStorage, TextEncoder, TextDecoder, Blob,
    Event: class { constructor(type) { this.type = type; } },
    URL: class extends URL { static createObjectURL() { return 'blob:review'; } static revokeObjectURL() {} },
    window: { location, addEventListener(type, fn) { const handlers = listeners.get(type) || []; handlers.push(fn); listeners.set(type, handlers); }, dispatchEvent(event) { for (const fn of listeners.get(event.type) || []) fn(event); }, matchMedia: () => ({ matches: false }), confirm: () => true },
    navigator: { clipboard: { writeText: async () => {} } },
    Option: function (text, value) { this.textContent = text; this.value = value; },
    EventSource: class { constructor(url) { this.url = url; this.closed = false; sources.push(this); } addEventListener() {} close() { this.closed = true; } },
    setTimeout(fn) { timers.set(++nextTimer, fn); return nextTimer; }, clearTimeout(id) { timers.delete(id); },
    fetch: async (url, options = {}) => { requests.push({ url, options }); return handler(url, options); }
  });
  handler = async url => {
    if (url.startsWith('/api/drafts/')) return ok({ version: 0, text: '', attachments: [] });
    if (url === '/api/chats' || url.startsWith('/api/artifacts')) return ok([]);
    if (url === '/api/profile/status') return ok({ state: 'unlocked', session: 'profile-A', revision: 'profile-r1' });
    throw new Error('Unexpected synthetic request: ' + url);
  };
  vm.runInContext(script, context, { filename: page });
  const run = source => vm.runInContext(source, context);
  if (page === 'index.html') {
    run("profileSession='profile-A';profileRevision='profile-r1';config={revision:'connection-r1',provider:'mock',endpoint:'',model:'smith-demo',tested:true};");
    // Rendering unrelated to these race tests is kept inert; closeProfileView and
    // actual input/DOM destruction remain real production functions.
    run("renderMessages=()=>{};renderHistory=()=>{};renderDraftAttachments=()=>{};renderConfig=()=>{};syncSend=()=>{};setBusy=value=>{busy=value};conversationFailure=e=>error(e.message);closeNavigation=()=>{};");
    elements.get('model-select').value = 'smith-demo';
  }
  return { run, context, elements, requests, sources, replacements, storage, localStorage, document, setFetch(fn) { handler = fn; } };
}

test('session revocation clears the current DOM and closes its SSE source', () => {
  const h = harness();
  h.run("messages=[{content:'synthetic private answer'}];draftAttachments=[{content:'synthetic attachment'}];eventSource=new EventSource('/synthetic');");
  h.elements.get('prompt').value = 'synthetic draft';
  h.run('closeProfileView()');
  assert.equal(h.document.body.children.length, 1);
  assert.equal(h.document.body.children[0].children[0].textContent,'Workspace unavailable');
  assert.equal(h.elements.get('prompt').value, '');
  assert.equal(h.sources[0].closed, true);
  assert.equal(h.run('messages.length + draftAttachments.length'), 0);
});

test('a successful API body delivered after session revocation is rejected', async () => {
  const h = harness(), body = deferred();
  h.setFetch(async () => ({ ok: true, status: 200, json: () => body.promise }));
  const pending = h.run("api('/chats')");
  await flush();
  h.run('closeProfileView()');
  body.resolve([{ title: 'late synthetic private history' }]);
  await assert.rejects(pending, /locked|session|closed/i);
});

test('session revocation clears sensitive configuration and plan state before navigation', () => {
  const h = harness();
  h.run("config={endpoint:'private endpoint'};chatRecord={title:'private title'};managementTarget={title:'private title'};nativeData={prepared:{plan:{path:'private selected path'}}};nativeOperationTarget={endpoint:'private endpoint'};closeProfileView();");
  const remaining = h.run('JSON.stringify({config,chatRecord,managementTarget,nativeData,nativeOperationTarget})');
  assert.doesNotMatch(remaining, /private/);
});

test('sending in an existing conversation retains the unrelated new-conversation draft', async () => {
  const h = harness();
  h.run("chatId='existing';chatRecord={id:'existing',version:2};draftCache.set('existing',{version:1,text:'send this',attachments:[],loaded:true,chain:Promise.resolve(),conflict:false});");
  h.elements.get('prompt').value = 'send this';
  h.setFetch(async (url, options) => {
    if (url === '/api/chats/existing/messages') return ok({ run_id: 'accepted-run', message_id: 'assistant', user_message_id: 'user', version: 3 });
    if (url === '/api/drafts/new') return ok({ version: 8, text: 'unrelated saved draft', attachments: [] });
    if (url === '/api/drafts/existing') return ok({ version: 2, text: '', attachments: [] });
    if (url === '/api/chats') return ok([]);
    throw new Error('Unexpected synthetic request ' + url + ' ' + options.method);
  });
  await h.run('startSend()');
  assert.equal(h.requests.some(r => r.url === '/api/drafts/new' && r.options.method === 'DELETE'), false);
});

test('draft cleanup failure cannot hide an already accepted generation', async () => {
  const h = harness();
  h.run("chatId='existing';chatRecord={id:'existing',version:2};draftCache.set('existing',{version:1,text:'send this',attachments:[],loaded:true,chain:Promise.resolve(),conflict:false});");
  h.elements.get('prompt').value = 'send this';
  h.setFetch(async (url, options) => {
    if (url === '/api/chats/existing/messages') return ok({ run_id: 'accepted-run', message_id: 'assistant', user_message_id: 'user', version: 3 });
    if (url === '/api/drafts/new') return options.method === 'DELETE' ? fail(409, 'Another tab changed this draft') : ok({ version: 8, text: 'unrelated', attachments: [] });
    if (url === '/api/drafts/existing') return ok({ version: 2, text: '', attachments: [] });
    if (url === '/api/chats') return ok([]);
    throw new Error('Unexpected synthetic request ' + url);
  });
  await h.run('startSend()');
  assert.ok(h.sources.some(source => source.url.includes('accepted-run')), 'accepted run must be observed');
  assert.equal(h.run('runId'), 'accepted-run');
});

test('a draft conflict keeps Data management navigation from losing visible text', async () => {
  const h=harness();h.run("draftCache.set('new',{version:1,text:'saved',attachments:[],loaded:true,chain:Promise.resolve(),conflict:true});");h.elements.get('prompt').value='unsaved visible text';await h.run('openDataManagement()');assert.equal(h.context.location.href,'');assert.equal(h.elements.get('prompt').value,'unsaved visible text');assert.equal(h.run('profileClosed'),false);
});

test('a draft conflict has explicit reload recovery instead of a sticky cache dead end', async () => {
  const h = harness();
  h.run("chatId='same';chatRecord={id:'same',version:2};draftCache.set('same',{version:1,text:'old',attachments:[],loaded:true,chain:Promise.resolve(),conflict:true});");
  h.elements.get('prompt').value = 'current unsaved choice';
  h.setFetch(async url => {
    if (url === '/api/chats/same') return ok({ chat: { id: 'same', version: 3 }, messages: [] });
    if (url === '/api/drafts/same') return ok({ version: 2, text: 'newer saved choice', attachments: [] });
    if (url.startsWith('/api/artifacts')) return ok([]);
    throw new Error('Unexpected synthetic request ' + url);
  });
  await h.run('reloadSavedDraft()');
  assert.ok(h.requests.some(r => r.url === '/api/drafts/same'), 'the advertised reload action must reach the newer draft');
  assert.equal(h.elements.get('prompt').value, 'newer saved choice');
  assert.equal(h.run("draftCache.get('same').conflict"), false);
});

test('automatic Data management never presents workspace password or lock controls',()=>{
 const h=harness('profile.html');h.elements.get('legacy-action').value='legacy-import';h.elements.get('legacy-key-format').value='master';h.run("state={state:'unlocked',automatic:true,retained_manual_profile:true,selection:'default',revision:'r1'};render()");assert.equal(h.elements.has('access-form'),false);assert.equal(h.elements.has('lock-profile'),false);assert.equal(h.elements.has('legacy-new-key-fields'),false);assert.equal(h.elements.has('migration-passphrase'),false);assert.equal(h.elements.get('retained-manual').hidden,false);
});
test('original import clears source key before submission and sends no output passphrase',async()=>{
 const h=harness('profile.html');h.run("state={state:'unlocked',automatic:true,selection:'default',revision:'r1'}");h.elements.get('legacy-action').value='legacy-import';h.elements.get('legacy-key-format').value='master';h.elements.get('legacy-master').value='a'.repeat(64);h.elements.get('legacy-source').value='/synthetic/copy';h.elements.get('legacy-managed').value='/synthetic/artifacts';h.elements.get('legacy-offline').checked=true;
 let submitted=false;h.setFetch(async(url,options)=>{if(url==='/api/profile/legacy-import'){submitted=true;assert.equal(h.elements.get('legacy-master').value,'');const body=JSON.parse(options.body);assert.equal(body.original_key.master_hex,'a'.repeat(64));assert.equal('passphrase'in body,false);return ok({});}return ok({state:'unlocked',automatic:true,revision:'r2',selection:'imported'});});await h.elements.get('legacy-controls').onsubmit({preventDefault(){}});assert.equal(submitted,true);assert.deepEqual(h.storage,{});
});
test('a Data operation clears an unsubmitted original key when its state changes',async()=>{
 const h=harness('profile.html');h.run("state={state:'unlocked',automatic:true,revision:'r1'};render()");h.elements.get('legacy-master').value='b'.repeat(64);h.setFetch(async()=>ok({state:'operating',automatic:true,revision:'r2'}));await h.run('refresh()');assert.equal(h.elements.get('legacy-master').value,'');
});

test('legacy browser draft is retained until explicit deletion', () => {
  const h = harness();
  h.localStorage.setItem('smith-next:draft:older', 'synthetic legacy draft');
  h.localStorage.setItem('smith-next:draft:older:attachments', '[]');
  h.run('reviewLegacyDrafts()');
  assert.equal(h.localStorage.getItem('smith-next:draft:older'), 'synthetic legacy draft');
  const remove = h.elements.get('legacy-drafts-list').children[0].children[3];
  remove.onclick();
  assert.equal(h.localStorage.getItem('smith-next:draft:older'), null);
});

test('draft saves serialize CAS versions while retaining the most recent text', async () => {
  const h = harness(), firstPut = deferred();
  h.run("draftCache.set('new',{version:3,text:'old',attachments:[],loaded:true,chain:Promise.resolve(),conflict:false});");
  let writes = 0;
  h.setFetch(async (_url, options) => {
    const body = JSON.parse(options.body); writes++;
    if (writes === 1) { assert.equal(body.version, 3); assert.equal(body.text, 'first edit'); return firstPut.promise; }
    assert.equal(body.version, 4); assert.equal(body.text, 'latest edit');
    return ok({ version: 5, text: body.text, attachments: body.attachments });
  });
  h.elements.get('prompt').value = 'first edit'; const first = h.run('saveDraft()');
  await flush();
  h.elements.get('prompt').value = 'latest edit'; const second = h.run('saveDraft()');
  assert.equal(writes, 1);
  firstPut.resolve(ok({ version: 4, text: 'first edit', attachments: [] }));
  await Promise.all([first, second]);
  assert.equal(h.run("draftCache.get('new').text"), 'latest edit');
  assert.deepEqual(h.storage, {});
});

test('new-conversation cleanup cannot delete a newer other-tab draft or hide the accepted run', async () => {
  const h = harness(), create = deferred();
  let draft = { version: 0, text: '', attachments: [] };
  h.setFetch(async (url, options) => {
    if (url === '/api/drafts/new') {
      if (!options.method) return ok({ ...draft });
      const body = JSON.parse(options.body);
      if (body.version !== draft.version) return fail(409, 'Newer draft retained');
      draft = { version: draft.version + 1, text: options.method === 'DELETE' ? '' : body.text, attachments: body.attachments || [] };
      return ok({ ...draft });
    }
    if (url === '/api/chats' && options.method === 'POST') return create.promise;
    if (url === '/api/chats/created/messages') return ok({ run_id: 'new-run', message_id: 'assistant', user_message_id: 'user', version: 1 });
    if (url === '/api/drafts/created') return ok({ version: 1, text: '', attachments: [] });
    if (url === '/api/chats') return ok([]);
    throw new Error('Unexpected synthetic request ' + url);
  });
  await h.run('restoreDraft()');
  h.elements.get('prompt').value = 'submitted new-conversation prompt';
  const sending = h.run('startSend()');
  await flush();
  assert.equal(draft.version, 1);
  draft = { version: 2, text: 'newer other-tab draft', attachments: [] };
  create.resolve(ok({ id: 'created', title: 'Created', version: 0 }));
  await sending; await flush();
  assert.equal(draft.text, 'newer other-tab draft');
  assert.ok(h.sources.some(source => source.url.includes('new-run')));
});

test('Data management saves the current draft before navigating',async()=>{
 const h=harness();h.run("draftCache.set('new',{version:1,text:'saved',attachments:[],loaded:true,chain:Promise.resolve(),conflict:false});");h.elements.get('prompt').value='latest edit';h.setFetch(async(_url,options)=>ok({version:2,text:JSON.parse(options.body).text,attachments:[]}));await h.run('openDataManagement()');assert.equal(h.context.location.href,'/profile');assert.ok(h.requests.some(r=>r.url==='/api/drafts/new'&&r.options.method==='PUT'));assert.equal(h.requests.some(r=>r.url==='/api/profile/lock'),false);
});

test('a pending attachment read settles safely after the real DOM is destroyed', async () => {
  const h = harness(), read = deferred();
  h.context.files = [{ name: 'synthetic.txt', size: 5, arrayBuffer: () => read.promise }];
  const reading = h.run('attachTextFiles(files)');
  h.run('closeProfileView()');
  read.resolve(new TextEncoder().encode('later').buffer);
  await assert.doesNotReject(reading);
  assert.equal(h.run('draftAttachments.length'), 0);
});

test('profile status polling cannot restore an older state after a newer response', async () => {
  const h = harness('profile.html'), earlier = deferred();
  let call = 0;
  h.setFetch(async () => ++call === 1 ? earlier.promise : ok({ state: 'unlocked', revision: 'r2', session: 'new-session' }));
  const first = h.run('refresh()');
  await h.run('refresh()');
  earlier.resolve(ok({ state: 'locked', revision: 'r1' }));
  await first;
  assert.equal(h.run('state.state'), 'unlocked');
  assert.equal(h.run('state.revision'), 'r2');
});

test('a failed draft restore cannot silently overwrite unseen saved content', async () => {
  const h = harness();
  h.run("chatId='existing'");
  let reads = 0;
  h.setFetch(async (_url, options) => {
    if (!options.method && ++reads === 1) return fail(500, 'Synthetic interrupted read');
    if (!options.method) return ok({ version: 7, text: 'unseen retained draft', attachments: [] });
    const body = JSON.parse(options.body);
    return ok({ version: 8, text: body.text, attachments: body.attachments });
  });
  await assert.rejects(h.run('restoreDraft()'));
  h.elements.get('prompt').value = 'replacement typed before reviewing retained draft';
  await h.run('saveDraft()').catch(() => {});
  assert.equal(h.requests.some(r => r.options.method === 'PUT'), false, 'failed restore requires an explicit reload or replace choice');
});

test('navigation selected during a pending save wins over an earlier New conversation action', async () => {
  const h = harness(), save = deferred();
  h.run("chatId='A';chatRecord={id:'A',version:1};draftCache.set('A',{version:1,text:'old',attachments:[],loaded:true,chain:Promise.resolve(),conflict:false});");
  h.elements.get('prompt').value = 'current saved transition';
  h.setFetch(async (url, options) => {
    if (url === '/api/drafts/A' && options.method === 'PUT') return save.promise;
    if (url === '/api/chats/B') return ok({ chat: { id: 'B', title: 'Later selected conversation', version: 1 }, messages: [] });
    if (url.startsWith('/api/drafts/')) return ok({ version: 0, text: '', attachments: [] });
    if (url.startsWith('/api/artifacts')) return ok([]);
    throw new Error('Unexpected synthetic request ' + url);
  });
  const earlier = h.run('newChat()'); await flush();
  const later = h.run("openChat('B')");
  save.resolve(ok({ version: 2, text: 'current saved transition', attachments: [] }));
  await Promise.all([earlier, later]);
  assert.equal(h.run('chatId'), 'B');
});

test('model selection reply after workspace destruction cannot touch DOM or adopt configuration', async () => {
  const h=harness(),reply=deferred();let adopted=0;
  h.elements.get('model-select').value='different-synthetic-model';
  h.context.window.addEventListener('smith-model-selected',()=>adopted++);
  h.setFetch(async(url,options)=>{assert.equal(url,'/api/model');assert.equal(JSON.parse(options.body).model,'different-synthetic-model');return reply.promise;});
  const selecting=h.run('selectModel()');await flush();assert.equal(h.run('modelSelectionPending'),true);
  h.run('closeProfileView()');assert.equal(h.document.getElementById('model-select'),null);
  reply.resolve(ok({revision:'connection-r2',provider:'mock',endpoint:'',model:'different-synthetic-model',tested:true}));
  await assert.doesNotReject(selecting);
  assert.equal(h.run('Object.keys(config).length'),0);
  assert.equal(h.run('modelSelectionPending'),false);
  assert.equal(adopted,0);
  assert.equal(h.requests.length,1,'no model-refresh request follows a revoked selection');
});
