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
const output = path.resolve(process.env.SMITH_MIGRATION_BROWSER_OUTPUT || path.join(root, 'docs/screenshots/encrypted-migration'));
const reportName = 'migration-browser-acceptance.json';
const checks = [], screenshots = [], pageErrors = [], unexpectedOrigins = new Set();
const allowedOrigins = new Set();
let temp, copiedBinary, servedProfileSha256, browserServer, browser, context, page, active, stage = 'initialization';
let passphrase = 'Synthetic browser only ' + crypto.randomBytes(24).toString('base64url');
const PROMPT = 'Synthetic migration prompt — every branch stays intact.';
const ANSWER = 'Verified synthetic migration answer. No real information.';
const ARTIFACT = 'Synthetic artifact bytes preserved exactly 🦀\n';
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
function fixture(folder, version = 5) {
  // Public arguments contain only this invocation's synthetic path and version.
  execFileSync('python3', ['-c', String.raw`
import pathlib, sqlite3, sys
p=pathlib.Path(sys.argv[1]);p.mkdir()
c=sqlite3.connect(p/'smith-next.sqlite3')
c.executescript("""
PRAGMA foreign_keys=ON;
CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE chats(id TEXT PRIMARY KEY,title TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL DEFAULT 0,version INTEGER NOT NULL DEFAULT 0,title_locked INTEGER NOT NULL DEFAULT 0,activity INTEGER NOT NULL DEFAULT 0);
CREATE TABLE messages(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL,error TEXT,created_at INTEGER NOT NULL,run_id TEXT);
CREATE TABLE artifacts(id TEXT PRIMARY KEY,chat_id TEXT REFERENCES chats(id),name TEXT NOT NULL,content TEXT NOT NULL,created_at INTEGER NOT NULL);
CREATE TABLE run_configs(run_id TEXT PRIMARY KEY,provider TEXT NOT NULL,endpoint TEXT NOT NULL,model TEXT NOT NULL,warning TEXT,finish_reason TEXT,transport_incomplete INTEGER NOT NULL DEFAULT 0);
CREATE TABLE message_branches(message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,parent_id TEXT REFERENCES messages(id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX message_branch_parent ON message_branches(parent_id);
CREATE TABLE conversation_heads(chat_id TEXT PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE,leaf_id TEXT REFERENCES messages(id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE text_attachments(id TEXT PRIMARY KEY,message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,name TEXT NOT NULL,media_type TEXT NOT NULL,content TEXT NOT NULL);
CREATE INDEX attachment_message ON text_attachments(message_id);
INSERT INTO settings VALUES('config','{"provider":"mock","endpoint":"http://127.0.0.1:11434","model":"smith-demo"}');
INSERT INTO chats(rowid,id,title,created_at,updated_at,version,title_locked,activity) VALUES(41,'migration-chat','Synthetic migration evidence',1,3,7,1,41);
INSERT INTO messages(rowid,id,chat_id,role,content,status,error,created_at,run_id) VALUES(-30,'migration-user','migration-chat','user','Synthetic migration prompt — every branch stays intact.','completed',NULL,1,NULL),(7,'migration-old','migration-chat','assistant','Earlier synthetic attempt is retained.','cancelled','Synthetic cancellation',2,'migration-run-old'),(91,'migration-answer','migration-chat','assistant','Verified synthetic migration answer. No real information.','completed',NULL,3,'migration-run-current');
INSERT INTO run_configs(run_id,provider,endpoint,model,finish_reason) VALUES('migration-run-old','mock','http://127.0.0.1:11434','smith-demo',NULL),('migration-run-current','mock','http://127.0.0.1:11434','smith-demo','stop');
INSERT INTO message_branches VALUES('migration-user',NULL),('migration-old','migration-user'),('migration-answer','migration-user');
INSERT INTO conversation_heads VALUES('migration-chat','migration-answer');
INSERT INTO text_attachments VALUES('migration-text','migration-user','synthetic-source.txt','text/plain','Synthetic attached text is retained.');
""")
c.execute("INSERT INTO artifacts(rowid,id,chat_id,name,content,created_at) VALUES(113,'migration-artifact','migration-chat','migration-proof.txt',?,3)",('Synthetic artifact bytes preserved exactly 🦀\n',))
c.execute('PRAGMA user_version='+str(int(sys.argv[2])))
c.commit();c.close()
(p/'ancillary').mkdir();(p/'ancillary'/'synthetic-note.txt').write_text('Synthetic copied ancillary data. No real profile.\n')
`, folder, String(version)], {stdio: ['ignore', 'pipe', 'pipe']});
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
  const needles = [passphrase, PROMPT, ANSWER, ARTIFACT, DRAFT].map(value => Buffer.from(value));
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
  const folder = path.join(temp, name); await fs.mkdir(folder);
  if (sourceForDetectedPlaintext) await fs.copyFile(path.join(sourceForDetectedPlaintext, 'smith-next.sqlite3'), path.join(folder, 'smith-next.sqlite3'));
  const env = {...global.process.env, SMITH_NEXT_DATA_DIR: folder, SMITH_NEXT_PORT: '0'};
  delete env.SMITH_NEXT_SYNTHETIC_PROFILE;
  const process = spawn(copiedBinary, [], {env, stdio: ['ignore', 'pipe', 'pipe']});
  const exited = new Promise(resolve => process.once('exit', (code, signal) => resolve({code, signal})));
  let logs = ''; process.stdout.on('data', bytes => { logs += bytes; }); process.stderr.on('data', bytes => { logs += bytes; });
  process.on('error', () => {});
  active = {process, exited, folder};
  const base = await poll(async () => logs.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0], Boolean, 'Owned app did not start', 12000);
  active.base = base; allowedOrigins.add(base);
  servedProfileSha256 = hash(Buffer.from(await (await fetch(base + '/profile')).text()));
  await page.goto(base + '/profile', {waitUntil: 'domcontentloaded'});
  await poll(status, value => ['needs_creation', 'migration_required'].includes(value.state), 'Profile screen did not settle');
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
async function tools() { if (!(await page.locator('#profile-tools').evaluate(element => element.open))) await page.locator('#profile-tools > summary').click(); }
async function migrateUI(source, outcome = 'succeeded') {
  await tools(); await page.locator('#migration-action').selectOption('migrate');
  await page.locator('#source-copy').fill(source); await page.locator('#offline-copy').check();
  await page.locator('#migration-passphrase').fill(passphrase); await page.locator('#migration-repeat').fill(passphrase);
  const result = await operationClick('#migration-submit', 'migrate', outcome);
  if (outcome === 'succeeded') { await page.locator('#migration-report').waitFor({state: 'visible'}); check(result.state === 'locked', 'Migration must finish locked'); check(result.operation.result.target_schema === 6, 'Migration must reach encrypted schema 6'); }
  return result;
}
async function rollbackUI(source, outcome = 'succeeded') {
  await tools(); await page.locator('#migration-action').selectOption('rollback'); await page.locator('#source-copy').fill(source); await page.locator('#offline-copy').check(); await page.locator('#migration-passphrase').fill(passphrase);
  return operationClick('#migration-submit', 'rollback', outcome);
}
async function unlockUI(wrong = false) {
  await page.locator('#passphrase').fill(wrong ? 'Wrong synthetic browser passphrase' : passphrase);
  const result = await operationClick('#access-submit', 'unlock', wrong ? 'failed' : 'succeeded');
  check(result.state === (wrong ? 'locked' : 'unlocked'), 'Unlock state is incorrect'); return result;
}
async function capture(name) {
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({path: path.join(output, name + '.png'), fullPage: true, animations: 'disabled', mask: [page.locator('#source-copy'), page.locator('input[type="password"]')], maskColor: '#d9d9de'});
  screenshots.push(name + '.png');
}
async function browserStorageHasNoSecret() {
  const stored = await page.evaluate(() => JSON.stringify({local: {...localStorage}, session: {...sessionStorage}}));
  check(!stored.includes(passphrase), 'Browser storage must not contain the passphrase');
}
async function main() {
  stage = 'synthetic-fixture-creation';
  await fs.mkdir(output, {recursive: true}); temp = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), 'smith-migration-browser-')));
  copiedBinary = path.join(temp, 'smith-next'); await fs.copyFile(binary, copiedBinary); await fs.chmod(copiedBinary, 0o700);
  const source = path.join(temp, 'stopped-source'); fixture(source); const sourceBefore = await tree(source);
  const unsupported = path.join(temp, 'unsupported-source'); fixture(unsupported, 99); const unsupportedBefore = await tree(unsupported);
  stage = 'headed-browser-launch';
  browserServer = await playwright.chromium.launchServer({executablePath: browserExecutable, headless: false, args: ['--disable-background-networking', '--disable-component-update', '--no-first-run', '--no-default-browser-check']});
  browser = await playwright.chromium.connect(browserServer.wsEndpoint());
  context = await browser.newContext({viewport: {width: 1280, height: 960}, colorScheme: 'light', acceptDownloads: true});
  await context.route('**/*', route => { const url = new URL(route.request().url()); if (allowedOrigins.has(url.origin) || ['blob:', 'data:'].includes(url.protocol)) return route.continue(); unexpectedOrigins.add(url.protocol); return route.abort(); });
  page = await context.newPage(); page.on('pageerror', () => pageErrors.push('Page JavaScript error'));

  stage = 'verified-copy-and-wrong-key';
  const first = await startHost('host-with-write', source); check((await status()).state === 'migration_required', 'Existing plaintext must request migration');
  const migrated = await migrateUI(source); await capture('01-migration-verified');
  const migrationCounts = {source_schema: migrated.operation.result.source_schema, target_schema: migrated.operation.result.target_schema, tables: migrated.operation.result.tables, rows: migrated.operation.result.rows, files: migrated.operation.result.files};
  assert.deepEqual(await tree(source), sourceBefore, 'Selected source changed during UI migration');
  const encrypted = path.join(first.folder, 'migrated', 'profile.sqlite3'); const encryptedBefore = hash(await fs.readFile(encrypted));
  await unlockUI(true); check(hash(await fs.readFile(encrypted)) === encryptedBefore, 'Wrong key changed encrypted database bytes'); await capture('02-wrong-passphrase-keeps-profile-locked');
  checks.push('UI-explicit-copy-attestation-migration-report-schema5-to6-source-byte-immutability');
  checks.push('UI-wrong-passphrase-refused-with-encrypted-database-byte-preservation');

  stage = 'migrated-history-artifact-and-real-write';
  await unlockUI(); await browserStorageHasNoSecret(); await page.locator('#open-workspace').click();
  stage = 'migrated-conversation-load';
  await page.waitForFunction(() => window.smithAppReady === true);
  await page.locator('#history button').filter({hasText: 'Synthetic migration evidence'}).click();
  await page.locator('.message[data-role="user"] .message-content').filter({hasText: PROMPT}).waitFor();
  check((await page.locator('.message[data-role="assistant"] .message-content').last().innerText()).includes(ANSWER), 'Selected migrated branch is incorrect');
  await page.locator('summary').filter({hasText: 'synthetic-source.txt'}).waitFor();
  stage = 'migrated-branch-export';
  const exported = page.waitForEvent('download'); await page.locator('#export-button').click(); const download = await exported;
  const exportedFile = path.join(temp, 'synthetic-conversation-export.md'); await download.saveAs(exportedFile);
  const exportedBytes = await fs.readFile(exportedFile, 'utf8');
  check(exportedBytes.includes('Earlier synthetic attempt is retained.') && exportedBytes.includes(ANSWER) && exportedBytes.includes('cancelled'), 'Export omitted an original branch or status');
  stage = 'migrated-artifact-preview';
  await page.locator('#artifacts-button').click(); const artifactRow = page.locator('.artifact-row').filter({hasText: 'migration-proof.txt'}); await artifactRow.getByRole('button', {name: 'Open', exact: true}).click();
  await page.locator('#artifact-preview').waitFor({state: 'visible'}); check(await page.locator('#preview-content').textContent() === ARTIFACT, 'Artifact preview changed copied bytes'); await capture('03-migrated-conversation-and-artifact');
  await page.keyboard.press('Escape'); await page.keyboard.press('Escape');
  stage = 'save-new-draft-and-lock';
  const savedDraft = page.waitForResponse(response => response.url().includes('/api/drafts/') && response.request().method() === 'PUT' && response.ok());
  await page.locator('#prompt').fill(DRAFT); await savedDraft;
  await page.locator('#lock-workspace').click(); await page.waitForURL('**/profile**'); await page.locator('#access-form').waitFor({state: 'visible'});
  stage = 'rollback-refuses-new-draft';
  const refused = await rollbackUI(source, 'failed'); check(refused.state === 'locked', 'Failed rollback must leave migrated profile locked'); await capture('04-rollback-refuses-later-write');
  check((await fs.stat(encrypted)).isFile(), 'Rollback refusal removed destination'); assert.deepEqual(await tree(source), sourceBefore, 'Rollback refusal changed source');
  await unlockUI(); await page.locator('#open-workspace').click(); await page.waitForFunction(() => window.smithAppReady === true); await page.locator('#history button').filter({hasText: 'Synthetic migration evidence'}).click();
  await poll(() => page.locator('#prompt').inputValue(), value => value === DRAFT, 'Refused rollback lost the later draft');
  await capture('04b-new-draft-retained-after-refused-rollback');
  await page.locator('#lock-workspace').click(); await page.waitForURL('**/profile**'); await poll(status, value => value.state === 'locked', 'Profile did not lock');
  await bytesAbsent(path.join(first.folder, 'migrated')); await browserStorageHasNoSecret();
  checks.push('UI-unlock-restores-selected-branch-text-attachment-all-branch-export-and-text-artifact');
  checks.push('UI-committed-draft-write-lock-rollback-refusal-and-reunlock-preserves-new-data');
  checks.push('published-encrypted-files-contain-no-synthetic-plaintext-or-passphrase');

  stage = 'immediate-nondestructive-deactivation';
  const second = await startHost('host-immediate-rollback'); await migrateUI(source); const immediately = await rollbackUI(source);
  check(immediately.state === 'migration_required' && immediately.operation.result.deactivated === true, 'Immediate rollback did not deactivate');
  check((await fs.stat(path.join(second.folder, 'migrated', 'profile.sqlite3'))).isFile(), 'Immediate rollback deleted encrypted profile');
  check((await fs.stat(path.join(second.folder, 'migrated', 'migration.db'))).isFile(), 'Immediate rollback deleted manifest');
  assert.deepEqual(await tree(source), sourceBefore, 'Immediate rollback changed original copy'); await capture('05-immediate-rollback-retains-both-profiles');
  const denied = await fetch(second.base + '/api/chats', {headers: {Origin: second.base}}); check(denied.status === 423, 'Deactivated profile must not expose plaintext data');
  checks.push('UI-immediate-deactivation-retains-encrypted-destination-manifest-and-unchanged-original');
  checks.push('deactivated-current-app-refuses-plaintext-data-access');

  stage = 'unsupported-schema-no-reset';
  const third = await startHost('host-unsupported'); await migrateUI(unsupported, 'failed');
  check((await status()).state === 'needs_creation', 'Unsupported source changed active profile state');
  check(!(await fs.stat(path.join(third.folder, 'migrated')).catch(() => null)), 'Unsupported schema published a destination');
  assert.deepEqual(await tree(unsupported), unsupportedBefore, 'Unsupported source was modified'); await capture('06-unsupported-schema-retained');
  checks.push('UI-unsupported-future-source-schema-refused-without-reset-or-destination-publication');
  await browserStorageHasNoSecret(); check(pageErrors.length === 0, 'Browser reported a JavaScript error'); check(unexpectedOrigins.size === 0, 'Page attempted an external request');
  checks.push('secret-fields-cleared-no-passphrase-browser-storage-no-page-errors-no-external-page-requests');
  return {date: new Date().toISOString(), headed: true, syntheticOnly: true, browser: {executable: path.basename(browserExecutable), version: browser.version()}, binarySha256: hash(await fs.readFile(copiedBinary)), servedProfileHtmlSha256: servedProfileSha256, sourceContract: {schema: 5, sparseMessageRowids: [-30, 7, 91], branchAlternatives: 2, artifactRowid: 113, ancillaryFiles: 1}, migrationCounts, groups: checks.length, checks, screenshots, passphrasesAndSourcePathsRedacted: true};
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
  passphrase = '';
  if (error) throw error;
}
main().then(async report => {
  stage = 'owned-process-and-synthetic-data-cleanup'; await cleanup();
  report.passed = true; report.ownedProcessesReapedAndSyntheticDirectoriesRemoved = true;
  await fs.rm(path.join(output, 'migration-browser-failure.json'), {force: true});
  await fs.writeFile(path.join(output, reportName), JSON.stringify(report, null, 2) + '\n'); console.log(JSON.stringify(report, null, 2));
}).catch(async error => {
  // Playwright call logs can include fill values or selected paths. Do not emit
  // raw errors, traces, request bodies, paths or passphrases into evidence.
  let message = String(error.message).split('\n')[0];
  for (const sensitive of [passphrase, temp, root].filter(Boolean)) message = message.split(sensitive).join('[redacted]');
  const report = {date: new Date().toISOString(), headed: true, syntheticOnly: true, passed: false, stage, errorType: error.name, message, checks, screenshots};
  await fs.mkdir(output, {recursive: true}); await fs.writeFile(path.join(output, 'migration-browser-failure.json'), JSON.stringify(report, null, 2) + '\n');
  console.error('Synthetic migration browser acceptance failed at: ' + stage); process.exitCode = 1;
}).finally(async () => { try { await cleanup(); } catch { console.error('Owned synthetic process cleanup did not finish normally.'); process.exitCode = 1; } });
