'use strict';
// Executes the actual navigation/action modules using synthetic DOM and history.
// App backend functions are controlled dependencies; no browser or network used.
const fs = require('node:fs'), path = require('node:path'), vm = require('node:vm');
const test = require('node:test'), assert = require('node:assert/strict');
const A = '11111111-1111-4111-8111-111111111111';
const B = '22222222-2222-4222-8222-222222222222';
function deferred() { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; }
const flush = () => new Promise(r => setImmediate(r));
function harness(module = 'navigation.js', platform = 'MacIntel', actualOpen = false) {
  const elements = new Map(), windowEvents = new Map(), documentEvents = new Map(), requests = [], errors = [];
  const html = fs.readFileSync(path.join(__dirname, '../web/index.html'), 'utf8');
  const source = fs.readFileSync(path.join(__dirname, '../web', module), 'utf8');
  class El {
    constructor(id = '', tag = 'div') { this.id = id; this.tagName = tag.toUpperCase(); this.dataset = {}; this.children = []; this.listeners = new Map(); this.open = false; this.style = {}; }
    set innerHTML(value) { for (const [, tag, id] of value.matchAll(/<(\w+)[^>]*\bid="([^"]+)"/g)) { const child = get(id, tag); this.append(child); } }
    setAttribute(key, value) { this[key] = value; }
    append(...children) { for (const child of children) { child.parent = this; this.children.push(child); if (child.id) elements.set(child.id, child); } }
    prepend(...children) { this.children.unshift(...children); }
    insertBefore(child, before) { this.children.splice(this.children.indexOf(before), 0, child); }
    replaceChildren(...children) { this.children = children; }
    addEventListener(type, fn) { const list = this.listeners.get(type) || []; list.push(fn); this.listeners.set(type, list); }
    dispatchEvent(event) { return Promise.all((this.listeners.get(event.type) || []).map(fn => fn(event))); }
    showModal() { this.open = true; }
    show() { this.open = true; }
    close() { if (this.open) { this.open = false; queueMicrotask(() => this.dispatchEvent({ type: 'close' })); } }
    closest() { return ['INPUT', 'TEXTAREA', 'SELECT'].includes(this.tagName) || this.role === 'textbox' ? this : null; }
  }
  function get(id, tag = 'div') { if (!elements.has(id)) elements.set(id, new El(id, tag)); return elements.get(id); }
  for (const [, tag, id] of html.matchAll(/<(\w+)[^>]*\bid="([^"]+)"/g)) get(id, tag);
  for (const id of ['documents', 'file-tasks', 'trash', 'shortcuts']) get(id, 'dialog');
  const document = {
    body: new El('body'), head: new El('head'),
    createElement: tag => new El('', tag), createTextNode: text => ({ textContent: text }),
    getElementById: id => elements.get(id), querySelector: selector => get(selector),
    addEventListener(type, fn) { const list = documentEvents.get(type) || []; list.push(fn); documentEvents.set(type, list); }
  };
  let position = 0; const entries = [{ state: null, url: '/?chat=' + A }], historyCalls = [];
  const location = { href: 'http://127.0.0.1/?chat=' + A, reload() { historyCalls.push('reload'); } };
  const history = {
    get state() { return entries[position].state; },
    replaceState(state, _, url) { entries[position] = { state: structuredClone(state), url }; location.href = new URL(url, location.href).href; historyCalls.push('replace'); },
    pushState(state, _, url) { entries.splice(position + 1); entries.push({ state: structuredClone(state), url }); position++; location.href = new URL(url, location.href).href; historyCalls.push('push'); },
    back() { return history.go(-1); }, forward() { return history.go(1); },
    go(delta) { const next = position + delta; if (next < 0 || next >= entries.length) return Promise.resolve(); position = next; location.href = new URL(entries[position].url, location.href).href; historyCalls.push('go:' + delta); return emit('popstate', { state: structuredClone(entries[position].state) }); }
  };
  const window = {
    addEventListener(type, fn) { const list = windowEvents.get(type) || []; list.push(fn); windowEvents.set(type, list); },
    dispatchEvent: event => emit(event.type, event), smithAppReady: false
  };
  async function emit(type, event = {}) { await Promise.all((windowEvents.get(type) || []).map(fn => fn({ type, ...event }))); }
  let apiHandler = async url => url === '/actions' ? { revision: 1, undo: { id: 'action-1', label: 'Rename' }, redo: { id: 'redo-1', label: 'Restore' } } : { history: { revision: 2 }, chats: [] };
  const context = vm.createContext({
    document, window, location, history, URL, URLSearchParams, TextEncoder, WeakSet, Set, Map,
    crypto: { randomUUID: () => 'synthetic-scope' }, navigator: { platform },
    Event: class { constructor(type) { this.type = type; } },
    $: id => document.getElementById(id), profileClosed: false, profileSession: 'synthetic-session', busy: false, loadingChat: false,
    chatId: A, chatRecord: { id: A }, chats: [], navEpoch: 0,
    messages: [], artifacts: [], editTarget: null, managementTarget: null,
    openChat: async id => { context.chatId = id; }, newChat: async () => { context.chatId = null; },
    renderHistory() {}, renderMessages() {}, applyConversation() {}, requestDelete() {}, syncSend() {}, closeNavigation() {},
    conversationRecords: data => data.messages || [], preserveVisible: records => records,
    refreshArtifacts: async () => {}, refreshNative: async () => {}, refreshChats: async () => {},
    restoreDraft: async () => {}, saveDraft: async () => {}, notify() {}, error: e => errors.push(e),
    api: async (url, options = {}) => { requests.push({ url, options }); return apiHandler(url, options); },
    sessionFetch: async () => ({ ok: true, headers: { get: () => 'text/plain' }, text: async () => 'synthetic artifact' }),
  });
  // Use the actual New-button binding to catch stale function captures.
  if (actualOpen) vm.runInContext(html.match(/^async function openChat\(id\).*$/m)[0], context);
  vm.runInContext(html.match(/\$\('new-chat'\)\.onclick=[^;]+;/)[0], context);
  vm.runInContext(source, context, { filename: module });
  return {
    context, elements, history, entries, requests, errors, historyCalls, emit,
    run: s => vm.runInContext(s, context), setApi: fn => { apiHandler = fn; },
    async key(fields) { const event = { key: 'z', target: document.body, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; }, ...fields }; for (const handler of documentEvents.get('keydown') || []) handler(event); await flush(); return event; }
  };
}

test('New button records its successful destination and never prompt contents', async () => {
  const h = harness(); await h.context.window.SmithNavigation.ready();
  await h.elements.get('new-chat').onclick();
  assert.equal(h.context.chatId, null);
  assert.equal(h.history.state.route.chat, null);
  assert.equal(h.entries.length, 2);
  assert.deepEqual(Object.keys(h.history.state.route).sort(), ['artifact', 'chat', 'view']);
});

test('same conversation selection produces no duplicate navigation entry', async () => {
  const h = harness(); await h.context.window.SmithNavigation.ready();
  await h.context.openChat(A); await h.context.openChat(A);
  assert.equal(h.entries.length, 1);
  assert.equal(h.errors.length, 0);
});

test('late artifact navigation cannot reopen an older screen after Forward wins', async () => {
  const h = harness(); await h.context.window.SmithNavigation.ready();
  h.elements.get('artifacts').showModal();
  h.elements.get('artifacts').close(); await flush();
  const gate = deferred(); h.context.refreshArtifacts = () => gate.promise;
  const earlier = h.history.back(); await flush();
  await h.history.forward();
  gate.resolve(); await earlier;
  assert.equal(h.history.state.route.view, 'conversation');
  assert.equal(h.elements.get('artifacts').open, false, 'superseded async route must not show its dialog');
});

test('busy generation rejects Back without history loops or duplicate entries', async () => {
  const h = harness(); await h.context.window.SmithNavigation.ready();
  await h.context.openChat(B); h.context.busy = true;
  await h.history.back(); await flush();
  assert.equal(h.context.chatId, B);
  assert.equal(h.history.state.route.chat, B);
  assert.equal(h.entries.length, 2);
  assert.equal(h.historyCalls.filter(value => value.startsWith('go:')).length, 2);
});

test('native text editing, composition and repeated keydown do not trigger app undo', async () => {
  const h = harness('undo-trash.js'); await h.context.window.SmithActions.refresh();
  for (const fields of [{ target: h.elements.get('prompt') }, { isComposing: true }, { repeat: true }, { altKey: true }, { target: { isContentEditable: true } }]) {
    const event = await h.key({ metaKey: true, ...fields }); assert.equal(event.defaultPrevented, false);
  }
  assert.equal(h.requests.filter(r => r.url === '/actions/undo').length, 0);
});

test('app undo captures exact history revision and ignores a repeated pending command', async () => {
  const h = harness('undo-trash.js'), gate = deferred();
  h.setApi(async url => url === '/actions' ? { revision: 19, undo: { id: 'exact-action', label: 'Rename' } } : gate.promise);
  await h.context.window.SmithActions.refresh();
  assert.equal((await h.key({ metaKey: true })).defaultPrevented, true);
  await h.key({ metaKey: true });
  const sent = h.requests.filter(r => r.url === '/actions/undo'); assert.equal(sent.length, 1);
  assert.deepEqual(JSON.parse(sent[0].options.body), { revision: 19, action_id: 'exact-action' });
  gate.resolve({ history: { revision: 20 }, chats: [] }); await flush();
});

test('Ctrl+Y is app redo on Windows while macOS leaves it alone', async () => {
  for (const [platform, expected] of [['MacIntel', 0], ['Win32', 1]]) {
    const h = harness('undo-trash.js', platform); await h.context.window.SmithActions.refresh();
    await h.key({ key: 'y', ctrlKey: true });
    assert.equal(h.requests.filter(r => r.url === '/actions/redo').length, expected);
  }
});

test('deep link restores its conversation and dialog without appending history', async () => {
  const h = harness();
  h.context.location.href = 'http://127.0.0.1/?chat=' + B + '&view=documents';
  await h.context.window.SmithNavigation.ready();
  assert.equal(h.context.chatId, B);
  assert.equal(h.elements.get('documents').open, true);
  assert.equal(h.entries.length, 1);
  assert.equal(h.context.window.smithNavigationReady, true);
  assert.equal(h.errors.length, 0);
});

test('reload retains saved navigation scope/index and resumes Forward', async () => {
  const h = harness();
  const route = { chat: A, view: 'settings', artifact: null };
  h.history.replaceState({ smithNavigation: 1, scope: 'retained-scope', index: 4, forward: true, route }, '', '/?chat=' + A + '&view=settings');
  await h.context.window.SmithNavigation.ready();
  assert.equal(h.history.state.scope, 'retained-scope');
  assert.equal(h.history.state.index, 4);
  assert.equal(h.history.state.forward, true);
  assert.equal(h.elements.get('nav-forward').disabled, false);
  assert.equal(h.elements.get('settings').open, true);
  assert.equal(h.entries.length, 1);
  assert.equal(h.errors.length, 0);
});

test('older action refresh cannot restore redo invalidated by a newer edit', async () => {
  const h = harness('undo-trash.js'), old = deferred(); let reads = 0;
  h.setApi(async url => { assert.equal(url, '/actions'); return ++reads === 1 ? old.promise : { revision: 2, undo: null, redo: null }; });
  const pending = h.context.window.SmithActions.refresh();
  await h.context.window.SmithActions.refresh();
  old.resolve({ revision: 1, redo: { id: 'stale-redo', label: 'Old action' } }); await pending;
  assert.equal(h.elements.get('action-redo').disabled, true);
  await h.context.window.SmithActions.redo();
  assert.equal(h.requests.some(r => r.url === '/actions/redo'), false);
});

test('immediate Trash deduplicates repeated clicks and sends exact selected versions', async () => {
  const h = harness('undo-trash.js'), gate = deferred();
  h.setApi(async url => url === '/actions' ? { revision: 5 } : gate.promise);
  const targets = [{ id: A, version: 7 }, { id: B, version: 11 }];
  const first = h.context.window.SmithActions.trash(targets);
  const duplicate = h.context.window.SmithActions.trash(targets);
  await flush();
  const sent = h.requests.filter(r => r.url === '/chats/trash'); assert.equal(sent.length, 1);
  assert.deepEqual(JSON.parse(sent[0].options.body), { chats: targets, history_revision: 5 });
  gate.resolve({ history: { revision: 6 }, chats: [] }); await Promise.all([first, duplicate]);
});

test('view-only Back during a pending actual conversation load clears loading ownership', async () => {
  const h = harness('navigation.js', 'MacIntel', true); await h.context.window.SmithNavigation.ready();
  h.elements.get('settings').showModal(); h.elements.get('settings').close(); await flush();
  const gate = deferred(); h.setApi(async () => gate.promise);
  const loading = h.context.openChat(B); await flush();
  assert.equal(h.context.loadingChat, true);
  await h.history.back();
  gate.resolve({ chat: { id: B, title: 'Superseded conversation' }, messages: [] }); await loading;
  assert.equal(h.context.chatId, A);
  assert.equal(h.elements.get('settings').open, true);
  assert.equal(h.context.loadingChat, false, 'the superseded load must not permanently disable sending');
});


test('copied historical opaque IDs survive a deep link and reload', async () => {
  const h = harness(), id = 'migration-chat';
  h.context.location.href = 'http://127.0.0.1/?chat=' + encodeURIComponent(id);
  await h.context.window.SmithNavigation.ready();
  assert.equal(h.context.chatId, id);
  assert.equal(h.history.state.route.chat, id);
  assert.equal(h.errors.length, 0);
});

// Workspace screens retain one logical navigation entry when switching from the rail.
test('desktop screen transitions retain Back and avoid intermediate chat entries', async () => {
  const h = harness();
  const settings=h.elements.get('settings'),documents=h.elements.get('documents');
  settings.dataset.desktopScreen='true';documents.dataset.desktopScreen='true';
  await h.context.window.SmithNavigation.ready();
  settings.showModal();await flush();
  documents.showModal();await flush();
  assert.equal(settings.open,false);assert.equal(documents.open,true);
  assert.equal(h.entries.length,3);assert.equal(h.history.state.route.view,'documents');
  await h.history.back();await flush();
  assert.equal(settings.open,true);assert.equal(documents.open,false);
  assert.equal(h.history.state.route.view,'settings');assert.equal(h.entries.length,3);
  assert.deepEqual(h.errors,[]);
});
