import { createHash } from 'node:crypto';
import { readFileSync, existsSync } from 'node:fs';
import { resolve } from 'node:path';

const root = resolve(import.meta.dirname, '../..');
const design = JSON.parse(readFileSync(resolve(root, 'docs/engineering-runtime/runtime-contract.design.json'), 'utf8'));
if (design.status !== 'accepted_implemented') throw new Error('design must remain accepted_implemented');
const expectedCardinality = {
  'WorkItem.workRuns': '0..*',
  'WorkRun.executionAttempt': 'exactly 1',
  'WorkRun.acceptedCandidate': '0..1',
  'Candidate.workRun': 'exactly 1',
  'StageRun.nullTask': 'historical-only; 0 executable WorkItem'
};
for (const [key, value] of Object.entries(expectedCardinality)) {
  if (design.cardinality?.[key] !== value) throw new Error(`cardinality drift: ${key}`);
}
const relationText = design.relationRules.join(' ');
if (!relationText.includes('Separate WorkRun records may reuse the same commit')) throw new Error('missing independent same-commit WorkRun rule');
if (relationText.includes('Attempt may serve') || relationText.includes('ordered Candidate sequence')) throw new Error('forbidden cross-WorkItem/multi-accepted-Candidate rule');
if (!design.decisionBoundary.verifier.includes('不能修改产品源代码或直接写权威 Verdict')) throw new Error('verifier authority rule missing');
const raw = JSON.stringify(design);
if (/(?:^|[" ])\/(?:Users|Volumes)\//.test(raw)) throw new Error('design contains an absolute local path');
for (const source of design.currentCanonicalSources) {
  const file = resolve(root, source.path);
  if (!existsSync(file)) throw new Error(`missing source: ${source.path}`);
  const sha = createHash('sha256').update(readFileSync(file)).digest('hex');
  if (sha !== source.sha256) throw new Error(`sha256 drift: ${source.path}`);
}
console.log(`ok: ${design.currentCanonicalSources.length} source hashes; accepted implementation status`);
