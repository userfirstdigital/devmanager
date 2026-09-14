// Read pinned upstream helpers without importing the application or dependencies.
// Usage: node probe.mjs /absolute/path/to/taskmaster-checkout
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import path from 'node:path';
import vm from 'node:vm';

const root = path.resolve(process.argv[2] ?? '');
assert.ok(process.argv[2], 'Pass the pinned Taskmaster source directory');
const sources = [];
const cases = [];
function read(relativePath) {
  const content = readFileSync(path.join(root, relativePath), 'utf8');
  sources.push({ path: relativePath, sha256: createHash('sha256').update(content).digest('hex') });
  return content;
}
function run(code, bindings = {}) {
  const context = vm.createContext(bindings);
  new vm.Script(code).runInContext(context, { timeout: 1000 });
  return context;
}
function check(id, description, actual, expected) {
  assert.deepEqual(JSON.parse(JSON.stringify(actual)), expected, id);
  cases.push({ id, description, observed: actual, status: 'PASS' });
}

const nextSource = read('scripts/modules/task-manager/find-next-task.js');
const nextContext = run(nextSource.replace(/^import .*;\r?\n/gm, '')
  .replace('export default findNextTask;', 'globalThis.next = findNextTask;'), {
  log() { throw new Error('Unexpected log dependency'); },
  addComplexityToTask() { throw new Error('Complexity integration is outside this probe'); }
});
const next = nextContext.next;
check('TM01', 'A completed predecessor permits its pending dependent', next([
  { id: 1, status: 'done' }, { id: 2, status: 'pending', dependencies: [1] }
]).id, 2);
check('TM02', 'A cancelled predecessor does not satisfy the dependent', next([
  { id: 1, status: 'cancelled' }, { id: 2, status: 'pending', dependencies: [1] }
]), null);
check('TM03', 'The helper selects a child without rechecking the in-progress parent prerequisite', next([
  { id: 1, status: 'in-progress', dependencies: [2], subtasks: [{ id: 1, status: 'pending' }] },
  { id: 2, status: 'blocked' }
]).id, '1.1');
check('TM04', 'A done parent label satisfies a dependent despite an unfinished child', next([
  { id: 1, status: 'done', subtasks: [{ id: 1, status: 'pending' }] },
  { id: 2, status: 'pending', dependencies: [1] }
]).id, 2);
check('TM05', 'A cycle produces no eligible item, not a completion result', next([
  { id: 1, status: 'pending', dependencies: [2] },
  { id: 2, status: 'pending', dependencies: [1] }
]), null);

const loopSource = read('packages/tm-core/src/modules/loop/services/loop.service.ts');
const start = loopSource.indexOf('\tprivate parseCompletion(');
const end = loopSource.indexOf('\n\tprivate async executeIteration(', start);
assert.ok(start >= 0 && end > start, 'Expected isolated completion method boundaries');
const completionContext = run(stripTypeScriptTypes(
  `class Probe { ${loopSource.slice(start, end)} }\nglobalThis.parse = Probe.prototype.parseCompletion;`,
  { mode: 'strip' }
));
const parse = completionContext.parse;
check('TM06', 'Nonzero exit without markers is an error', parse('', 1).status, 'error');
check('TM07', 'Completion marker is evaluated before a nonzero exit',
  parse('<loop-complete>ALL_DONE</loop-complete>', 1).status, 'complete');
check('TM08', 'A quoted marker in ordinary output is still matched',
  parse('Example only: "<loop-complete>example</loop-complete>"', 0).status, 'complete');
check('TM09', 'Completion wins when both complete and blocked markers occur',
  parse('<loop-blocked>missing check</loop-blocked>\n<loop-complete>done</loop-complete>', 0).status,
  'complete');

const registrySource = read('mcp-server/src/tools/tool-registry.js');
const registryBody = registrySource.slice(registrySource.indexOf('export const toolRegistry ='));
const registrations = Object.fromEntries([...registryBody.matchAll(/:\s*(register\w+)/g)]
  .map((match) => [match[1], () => {}]));
const registryContext = run(registryBody.replace(/export default toolRegistry;/, '')
  .replace(/\bexport /g, '') + '\nglobalThis.counts = getToolCounts(); globalThis.inherited = isValidTool("constructor");',
  registrations);
check('TM10', 'Actual registry categories; registration callbacks are inert stubs',
  registryContext.counts, { core: 7, standard: 14, total: 44 });
check('TM11', 'Name predicate also admits an inherited Object property', registryContext.inherited, true);

const expansionSource = read('scripts/modules/task-manager/expand-task.js');
const allocation = expansionSource.match(/const nextSubtaskId = [^;]+;/)?.[0];
assert.ok(allocation, 'Expected next-subtask candidate expression');
const allocationContext = run(allocation.replace('const nextSubtaskId', 'globalThis.nextId'), {
  task: { subtasks: [{ id: 1 }, { id: 3 }] }
});
check('TM12', 'Expansion candidate uses array length and can name an existing sparse ID',
  allocationContext.nextId, 3);

console.log(JSON.stringify({
  checkedAt: new Date().toISOString(), node: process.version, sourceRoot: root,
  scope: 'Twelve synthetic helper probes. Import bindings are removed; registration callbacks are inert. No model, CLI, schema pipeline, persistence, process runner, or application is exercised. PASS confirms the described observation, including limitations.',
  sources, passed: cases.length, cases
}, null, 2));
