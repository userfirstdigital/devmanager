// Bounded source probes, not an Atomic or DevManager integration suite.
// Run: node --experimental-vm-modules probe.mjs /absolute/path/to/atomic-main
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import path from 'node:path';
import vm from 'node:vm';

const root = path.resolve(process.argv[2] ?? path.resolve(import.meta.dirname, '../../../../../atomic-main'));
const manifest = JSON.parse(readFileSync(path.join(import.meta.dirname, 'sources.json'), 'utf8'));
const checks = [];
const observations = {};
const check = (name, fn) => { fn(); checks.push(name); };
const source = (relative) => {
  const raw = readFileSync(path.join(root, relative), 'utf8');
  const expected = manifest.files.find((entry) => entry.path === relative)?.sha256;
  assert.ok(expected, `Unpinned source: ${relative}`);
  assert.equal(createHash('sha256').update(raw).digest('hex'), expected, `Source changed: ${relative}`);
  return raw;
};
const context = vm.createContext({});
const modules = new Map();
const allow = new Set([
  'packages/workflows/builtin/review-convergence.ts',
  'packages/workflows/src/durable/durable-hash.ts',
  'packages/workflows/src/durable/child-invocation.ts',
  'packages/intercom/retry-identity.ts',
  'packages/intercom/retry-policy.ts',
  'packages/intercom/broker/send-signature.ts',
]);
const cryptoModule = new vm.SyntheticModule(['createHash', 'randomUUID'], function () {
  this.setExport('createHash', createHash);
  this.setExport('randomUUID', randomUUID);
}, { context });
async function load(relative) {
  if (modules.has(relative)) return modules.get(relative);
  assert.ok(allow.has(relative), `Unreviewed module import: ${relative}`);
  const code = stripTypeScriptTypes(source(relative), { mode: 'transform' });
  const module = new vm.SourceTextModule(code, { context, identifier: relative });
  modules.set(relative, module);
  await module.link(async (specifier, parent) => {
    if (specifier === 'node:crypto') return cryptoModule;
    assert.ok(specifier.startsWith('.'), `External import refused: ${specifier}`);
    return load(path.posix.normalize(path.posix.join(path.posix.dirname(parent.identifier), specifier)).replace(/\.js$/, '.ts'));
  });
  return module;
}
async function evaluate(relative) {
  const module = await load(relative);
  if (module.status !== 'evaluated') await module.evaluate({ timeout: 1000 });
  return module.namespace;
}
async function evaluateExtract(code, identifier) {
  const module = new vm.SourceTextModule(stripTypeScriptTypes(code, { mode: 'transform' }), { context, identifier });
  await module.link(() => { throw new Error('Extracted probe may not import modules'); });
  await module.evaluate({ timeout: 1000 });
  return module.namespace;
}

const convergence = await evaluate('packages/workflows/builtin/review-convergence.ts');
check('Required work blocks even at minor priority', () => assert.equal(convergence.findingBlocksClosure({ objective_alignment: 'required_by_objective', priority: 3 }), true));
check('Optional minor work does not inflate closure scope', () => assert.equal(convergence.findingBlocksClosure({ objective_alignment: 'consistent_with_objective', priority: 3 }), false));
check('Unclassified findings remain blocking', () => assert.equal(convergence.findingBlocksClosure({}), true));
check('Missing requirement remains unproven in traceability helper', () => assert.equal(convergence.traceabilityProvenExceptFinalAction({ traceability: [{ requirement: 'works', status: 'missing', evidence: '' }], allowFinalActionRemaining: false }), false));

// Extract this pure function because its containing module imports TypeBox.
// No schema/parser, reviewer model, or production workflow is exercised here.
const reviewSource = source('packages/workflows/builtin/goal-review.ts');
const reviewFunction = reviewSource.match(/export function reviewApproved\([\s\S]*?\n}/)?.[0];
assert.ok(reviewFunction, 'Reviewed function shape changed');
const review = await evaluateExtract(reviewFunction, 'extracted:reviewApproved');
const contradictory = {
  stop_review_loop: true, reviewer_error: null, overall_correctness: 'patch is incorrect',
  overall_explanation: 'A required behavior is absent', overall_confidence_score: 1,
  goal_oracle_satisfied: false, receipt_assessment: 'Required check did not pass',
  verification_remaining: 'Implement and verify the missing behavior',
  requirements_traceability: [{ requirement: 'required behavior', status: 'missing', evidence: 'Absent in inspected code' }],
  findings: [{ title: 'Missing required behavior', body: 'The implementation is absent', confidence_score: 1,
    objective_alignment: 'required_by_objective', priority: 1,
    code_location: { absolute_file_path: '/synthetic/project/feature.ts', line_range: { start: 1, end: 1 } } }],
};
check('Observed limitation: approval flag accepts contradictory evidence', () => assert.equal(review.reviewApproved(contradictory), true));
check('Explicit reviewer execution error still refuses approval', () => assert.equal(review.reviewApproved({ ...contradictory, reviewer_error: { kind: 'tool_failure' } }), false));

// Preserve reducer source logic, remove imports, and provide its real pure
// summary dependency. Only the early quorum branch is exercised. Any attempt
// to use convergence scoring fails instead of simulating that subsystem.
context.summarizeReviewConvergence = convergence.summarizeReviewConvergence;
context.convergence_escalation_evidence = () => { throw new Error('Unprobed scoring branch'); };
const reducerSource = stripTypeScriptTypes(source('packages/workflows/builtin/goal-reducer.ts'), { mode: 'transform' }).replace(/^import[\s\S]*?;\n/gm, '');
const reducer = await evaluateExtract(reducerSource, 'extracted:goal-reducer quorum branch');
const reviews = [0, 1, 2].map((n) => ({ ...contradictory, reviewer: `reviewer-${n}`, decision: n === 2 ? 'continue' : 'complete', parsed: true, parse_diagnostics: [], gaps: ['required behavior still missing'], blocker: null }));
observations.quorum = reducer.reduceGoalDecision({ blockers: [] }, reviews, { turn: 1, maxTurns: 10, reviewQuorum: 2, blockerThreshold: 3, nextActionOnComplete: 'finish' });
check('Observed limitation: two complete votes close despite unresolved evidence', () => assert.equal(observations.quorum.status, 'complete'));

const { durableHash } = await evaluate('packages/workflows/src/durable/durable-hash.ts');
const { durableChildInvocationFingerprint: fingerprint } = await evaluate('packages/workflows/src/durable/child-invocation.ts');
check('Canonical object key ordering gives a stable hash', () => assert.equal(durableHash({ b: 2, a: 1 }), durableHash({ a: 1, b: 2 })));
check('Changed child input changes invocation fingerprint', () => assert.notEqual(fingerprint({ name: 'build', normalizedName: 'build' }, { target: 'a' }), fingerprint({ name: 'build', normalizedName: 'build' }, { target: 'b' })));
check('Observed boundary: child fingerprint alone does not bind implementation code', () => assert.equal(fingerprint({ name: 'build', normalizedName: 'build', run: () => 'old' }, {}), fingerprint({ name: 'build', normalizedName: 'build', run: () => 'new' }, {})));

const { RetryIdentityReservations } = await evaluate('packages/intercom/retry-identity.ts');
let now = 0;
let serial = 0;
const retry = new RetryIdentityReservations({ now: () => now, ttlMs: 100, maxReuses: 2, createId: () => `message-${++serial}`, createToken: () => `token-${++serial}` });
const input = { sessionId: 'worker-1', action: 'ask', target: 'lead-1', text: 'Which contract?', expectsReply: true };
const original = retry.begin(input);
const second = retry.begin(input);
check('Identical fresh asks keep distinct intent identities', () => assert.notEqual(original.messageId, second.messageId));
retry.release(second);
const token = retry.retainAfterRecoverableDisconnect(original);
check('Disconnect retry receives an explicit retained token', () => assert.equal(typeof token, 'string'));
check('Retained token refuses changed text or recipient', () => {
  assert.throws(() => retry.begin({ ...input, text: 'Changed' }, token), (error) => error.code === 'mismatch');
  assert.throws(() => retry.begin({ ...input, target: 'lead-2' }, token), (error) => error.code === 'mismatch');
});
const resumed = retry.begin(input, token);
check('Retry preserves the original message identity', () => assert.equal(resumed.messageId, original.messageId));
check('Concurrent retry cannot claim the same operation', () => assert.throws(() => retry.begin(input, token), (error) => error.code === 'in_flight'));
retry.release(resumed);
check('Settled operation cannot be reused', () => assert.throws(() => retry.begin(input, token), (error) => error.code === 'settled'));
const expiring = retry.begin(input);
const expiringToken = retry.retainAfterRecoverableDisconnect(expiring);
now = 101;
check('Expired retry authority remains expired', () => assert.throws(() => retry.begin(input, expiringToken), (error) => error.code === 'expired'));

const results = { reviewedAt: new Date().toISOString(), node: process.version, sourceManifestSha256: createHash('sha256').update(readFileSync(path.join(import.meta.dirname, 'sources.json'))).digest('hex'), scope: 'Synthetic pure-helper and process-local identity probes. No Atomic runtime, broker, model, DBOS/Postgres, full suite, native DevManager, or external effect executed.', checks, observations, errors: [] };
writeFileSync(path.join(import.meta.dirname, 'probe-results.json'), JSON.stringify(results, null, 2) + '\n');
console.log(JSON.stringify({ checks: checks.length, errors: results.errors, quorumObservation: observations.quorum.status, modulesLoaded: modules.size }));
