#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { isDeepStrictEqual, parseArgs } from "node:util";

const root = path.resolve(import.meta.dirname, "..");
const rawArgs = process.argv.slice(2);
for (const name of ["--mode", "--snapshot", "--records"]) { if (rawArgs.filter((arg) => arg === name || arg.startsWith(`${name}=`)).length > 1) { console.error(`duplicate argument: ${name}`); process.exit(2); } }
let options;
try {
  ({ values: options } = parseArgs({ args: process.argv.slice(2), options: {
    mode: { type: "string", default: "snapshot" }, snapshot: { type: "string" }, records: { type: "string" },
  }, strict: true, allowPositionals: false }));
} catch (error) { console.error(`invalid arguments: ${error.message}`); process.exit(2); }
if (!["snapshot", "records", "live"].includes(options.mode)) { console.error(`unknown mode: ${options.mode}`); process.exit(2); }
if (options.mode === "live" && options.records) { console.error("live mode does not accept --records"); process.exit(2); }
if (options.mode === "snapshot" && options.records) { console.error("snapshot mode does not accept --records"); process.exit(2); }
if (options.mode === "records" && !options.records) { console.error("records mode requires --records FILE"); process.exit(2); }
const snapshot = JSON.parse(fs.readFileSync(path.resolve(root, options.snapshot ?? "docs/engineering-runtime/backlog-migration.json"), "utf8"));
const errors = [];
const rows = snapshot.mapping;
const ids = new Set(rows.map((row) => row.old_id));
const allowed = new Set(["KEEP", "REWRITE", "MERGE", "DEFER"]);
const counts = { KEEP: 59, REWRITE: 47, MERGE: 2, DEFER: 3 };
if (ids.size !== rows.length) errors.push("duplicate old_id");
if (rows.length !== snapshot.active_snapshot_count) errors.push("mapping count drift");
for (const [kind, count] of Object.entries(counts)) if (snapshot.counts?.[kind] !== count || rows.filter((row) => row.classification === kind).length !== count) errors.push(`classification count drift: ${kind}`);
if (!/^4359a7084e904640a1e4383d70278101cbbbf9aabf2eb4afc2ba78ff834671f8$/.test(snapshot.source_plan_sha256 ?? "")) errors.push("source plan hash drift");
if (!/^baedb1ef0849f188b800431fe1a016f0af0caa79$/.test(snapshot.baseline_head ?? "")) errors.push("baseline head drift");
if (snapshot.audit_bead !== "winwincode-dwz" || snapshot.source_plan !== "winwincode-community-engineering-runtime-bd-backlog.md") errors.push("audit identity drift");
const newBeads = snapshot.new_beads ?? {};
if (new Set(Object.values(newBeads)).size !== 4 || Object.values(newBeads).some((id) => typeof id !== "string" || !id.startsWith("winwincode-") || ids.has(id)) || newBeads["WWC-ER-0001"] !== snapshot.audit_bead) errors.push("new bead IDs must be distinct from old mappings");
if (JSON.stringify(Object.keys(newBeads).sort()) !== JSON.stringify(["WWC-ER-0001", "WWC-ER-0002", "WWC-ER-0003", "WWC-ER-0004"])) errors.push("stable ID set drift");
const taskPlan = snapshot.task_plan;
const expectedPlanCounts = { "00": 6, "01": 7, "02": 8, "03": 8, "04": 8, "05": 8, "06": 8, "07": 10, "08": 7, "09": 7, "10": 9, "11": 7, "12": 8, "13": 10 };
const expectedStableIds = new Set(Object.entries(expectedPlanCounts).flatMap(([prefix, count]) => Array.from({ length: count }, (_, index) => `WWC-ER-${prefix}${String(index + 1).padStart(2, "0")}`)));
const planEntries = taskPlan?.entries ?? [];
const planByStable = new Map(planEntries.map((entry) => [entry.stable_id, entry]));
if (taskPlan?.tracking_bead !== "winwincode-ds4" || planEntries.length !== 111 || planByStable.size !== planEntries.length) errors.push("task plan identity/count drift");
if (planByStable.size !== expectedStableIds.size || [...expectedStableIds].some((id) => !planByStable.has(id))) errors.push("task plan stable ID set drift");
for (const [prefix, count] of Object.entries(expectedPlanCounts)) if (planEntries.filter((entry) => entry.stable_id?.startsWith(`WWC-ER-${prefix}`)).length !== count) errors.push(`task plan count drift: E${prefix}`);
for (const entry of planEntries) {
  if (!/^WWC-ER-\d{4}$/.test(entry.stable_id ?? "") || !entry.title || !entry.bead_id || !["REUSE", "REWRITE", "CREATE"].includes(entry.action) || !entry.reason) errors.push(`invalid task plan entry: ${entry.stable_id ?? "<empty>"}`);
  if (!/^[a-f0-9]{64}$/.test(entry.record_sha256 ?? "") || /^0+$/.test(entry.record_sha256 ?? "")) errors.push(`invalid task plan fingerprint: ${entry.stable_id ?? "<empty>"}`);
  if (!Array.isArray(entry.depends_on) || !Array.isArray(entry.internal_dependencies) || !Array.isArray(entry.related)) errors.push(`invalid task plan lists: ${entry.stable_id}`);
  for (const dep of [...(entry.depends_on ?? []), ...(entry.internal_dependencies ?? [])]) if (!planByStable.has(dep)) errors.push(`task plan dependency missing: ${entry.stable_id} -> ${dep}`);
  for (const dep of entry.internal_dependencies ?? []) if (!(entry.depends_on ?? []).includes(dep)) errors.push(`internal dependency not declared: ${entry.stable_id} -> ${dep}`);
  for (const dep of entry.depends_on ?? []) if (planByStable.get(dep)?.bead_id === entry.bead_id && !(entry.internal_dependencies ?? []).includes(dep)) errors.push(`same-owner dependency not internal: ${entry.stable_id} -> ${dep}`);
  for (const dep of entry.internal_dependencies ?? []) if (planByStable.get(dep)?.bead_id !== entry.bead_id) errors.push(`cross-owner dependency marked internal: ${entry.stable_id} -> ${dep}`);
}
const planVisiting = new Set(), planVisited = new Set();
function visitPlan(id) {
  if (planVisiting.has(id)) return true;
  if (planVisited.has(id)) return false;
  planVisiting.add(id);
  const cycle = (planByStable.get(id)?.depends_on ?? []).some((dep) => visitPlan(dep));
  planVisiting.delete(id); planVisited.add(id); return cycle;
}
for (const entry of planEntries) if (visitPlan(entry.stable_id)) { errors.push("task plan dependency cycle"); break; }
const prerequisites = snapshot.new_bead_prerequisites ?? {};
const expectedPrerequisites = { "WWC-ER-0002": "WWC-ER-0001", "WWC-ER-0003": "WWC-ER-0002", "WWC-ER-0004": "WWC-ER-0002" };
if (!isDeepStrictEqual(prerequisites, expectedPrerequisites)) errors.push("new bead prerequisites incomplete or incorrect");
for (const row of rows) {
  if (!row.old_id || !allowed.has(row.classification) || !/^[a-f0-9]{64}$/.test(row.source_record_sha256 ?? "") || /^0+$/.test(row.source_record_sha256)) errors.push(`${row.old_id ?? "<empty>"}: invalid mapping/hash`);
  if (row.classification !== "MERGE" && row.canonical_owner !== row.old_id) errors.push(`${row.old_id}: non-merge owner reassignment`);
  if (row.classification === "MERGE" && (!ids.has(row.canonical_owner) || row.canonical_owner === row.old_id)) errors.push(`${row.old_id}: invalid merge owner`);
}
const groups = snapshot.duplicate_scope_groups ?? [];
if (groups.length !== 2) errors.push("duplicate scope group count drift");
if (new Set(groups.map((group) => group.source)).size !== groups.length) errors.push("duplicate scope source repeated");
for (const group of groups) {
  if (!ids.has(group.source) || !ids.has(group.target) || group.source === group.target) errors.push(`invalid duplicate scope: ${group.source} -> ${group.target}`);
  const row = rows.find((item) => item.old_id === group.source);
  if (!row || row.classification !== "MERGE" || row.canonical_owner !== group.target) errors.push(`duplicate scope unresolved: ${group.source} -> ${group.target}`);
}
for (const row of rows.filter((item) => item.classification === "MERGE")) {
  if (!groups.some((group) => group.source === row.old_id)) errors.push(`merge missing duplicate scope: ${row.old_id}`);
  const seen = new Set([row.old_id]); let target = row.canonical_owner;
  while (target && rows.find((item) => item.old_id === target)?.classification === "MERGE") {
    if (seen.has(target)) { errors.push(`merge cycle: ${row.old_id}`); break; }
    seen.add(target); target = rows.find((item) => item.old_id === target).canonical_owner;
  }
  if (!target || !rows.find((item) => item.old_id === target) || rows.find((item) => item.old_id === target).classification === "MERGE") errors.push(`merge has no non-merge terminal: ${row.old_id}`);
}
function digest(value) {
  const canonical = JSON.stringify(value, (_, item) => item && typeof item === "object" && !Array.isArray(item) ? Object.fromEntries(Object.entries(item).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)) : item);
  return createHash("sha256").update(canonical).digest("hex");
}
if (options.mode === "snapshot") {
  if (errors.length) { console.error(errors.join("\n")); process.exit(1); }
  console.log(`engineering runtime backlog: ${rows.length} mappings; mode=snapshot; live Beads status=NOT_RUN`); process.exit(0);
}
let records;
try { records = options.mode === "records" ? JSON.parse(fs.readFileSync(path.resolve(options.records), "utf8")) : JSON.parse(execFileSync("bd", ["--readonly", "list", "--all", "--limit", "0", "--json"], { cwd: root, encoding: "utf8", maxBuffer: 50 * 1024 * 1024 })); }
catch (error) { console.error(`${options.mode} records query failed: ${error.message}`); process.exit(1); }
const byId = new Map(records.map((record) => [record.id, record]));
if (byId.size !== records.length) errors.push("duplicate records ID");
const recordKeys = ["id", "title", "description", "acceptance_criteria", "status", "issue_type", "assignee", "labels", "dependencies"];
const planNewIds = new Set(planEntries.map((entry) => entry.bead_id).filter((id) => !ids.has(id)));
const excluded = new Set([...Object.values(newBeads), ...planNewIds, snapshot.audit_bead, taskPlan?.tracking_bead]);
const active = records.filter((record) => record.status !== "closed" && !excluded.has(record.id));
if (active.length !== rows.length || new Set(active.map((record) => record.id)).size !== rows.length) errors.push(`active coverage drift: ${active.length}`);
for (const row of rows) {
  const record = byId.get(row.old_id);
  if (!record || record.status === "closed") { errors.push(`missing active bead: ${row.old_id}`); continue; }
  if (digest(Object.fromEntries(recordKeys.map((key) => [key, record[key] ?? null]))) !== row.source_record_sha256) errors.push(`source task changed: ${row.old_id}`);
  let annotation = record.metadata?.engineering_runtime_review;
  if (typeof annotation === "string") try { annotation = JSON.parse(annotation); } catch { annotation = null; }
  if (!annotation || annotation.classification !== row.classification || annotation.audit_bead !== snapshot.audit_bead || annotation.canonical_owner !== row.canonical_owner || !isDeepStrictEqual(annotation.aligned_er_ids, row.aligned_er_ids) || annotation.decision_status !== row.decision_status) errors.push(`source metadata changed: ${row.old_id}`);
}
for (const [stable, id] of Object.entries(newBeads)) {
  const record = byId.get(id);
  if (!record || !record.title?.includes(`[${stable}]`)) errors.push(`new bead identity drift: ${stable}`);
  const parent = prerequisites[stable];
  if (parent && (!record || !record.dependencies?.some((dep) => dep.depends_on_id === newBeads[parent] && dep.type === "blocks"))) errors.push(`missing prerequisite: ${stable}`);
}
const blockEdges = new Map();
for (const record of records) for (const dep of record.dependencies ?? []) if (dep.type === "blocks") {
  if (!blockEdges.has(record.id)) blockEdges.set(record.id, []);
  blockEdges.get(record.id).push(dep.depends_on_id);
}
const planClaims = new Map();
for (const record of records) {
  const values = record.metadata?.engineering_runtime_plan_ids;
  const claimed = Array.isArray(values) ? values : typeof values === "string" ? (() => { try { return JSON.parse(values); } catch { return []; } })() : [];
  for (const stable of claimed) if (planByStable.has(stable)) {
    if (!planClaims.has(stable)) planClaims.set(stable, new Set());
    planClaims.get(stable).add(record.id);
  }
}
for (const entry of planEntries) {
  const claimants = planClaims.get(entry.stable_id) ?? new Set();
  if (claimants.size !== 1 || !claimants.has(entry.bead_id)) errors.push(`task plan owner claim conflict: ${entry.stable_id}`);
}
for (const entry of planEntries) {
  const record = byId.get(entry.bead_id);
  if (!record) errors.push(`task plan owner unavailable: ${entry.stable_id}`);
  if (record && digest(Object.fromEntries(recordKeys.map((key) => [key, record[key] ?? null]))) !== entry.record_sha256) errors.push(`task plan record changed: ${entry.stable_id}`);
  if (record?.status === "closed" && !record.close_reason) errors.push(`closed task missing close_reason: ${entry.stable_id}`);
  const metadata = record?.metadata?.engineering_runtime_plan_ids;
  const planIds = Array.isArray(metadata) ? metadata : typeof metadata === "string" ? (() => { try { return JSON.parse(metadata); } catch { return []; } })() : [];
  if (!record || !record.title?.includes(entry.stable_id) && !record.description?.includes(entry.stable_id)) errors.push(`task plan stable ID missing: ${entry.stable_id}`);
  if (!planIds.includes(entry.stable_id)) errors.push(`task plan metadata missing: ${entry.stable_id}`);
  if (!record?.acceptance_criteria) errors.push(`task plan acceptance missing: ${entry.stable_id}`);
  for (const dep of entry.depends_on ?? []) {
    const dependencyOwner = planByStable.get(dep)?.bead_id;
    if (dependencyOwner && dependencyOwner !== entry.bead_id && !blockEdges.get(entry.bead_id)?.includes(dependencyOwner)) errors.push(`missing cross-owner block: ${entry.stable_id} -> ${dep}`);
  }
}
const visiting = new Set(), visited = new Set();
function visit(id) {
  if (visiting.has(id)) return true;
  if (visited.has(id)) return false;
  visiting.add(id);
  const cycle = (blockEdges.get(id) ?? []).some((dep) => byId.has(dep) && visit(dep));
  visiting.delete(id); visited.add(id); return cycle;
}
for (const record of records) if (visit(record.id)) { errors.push("beads dependency cycle"); break; }
if (errors.length) { console.error(errors.join("\n")); process.exit(1); }
console.log(`engineering runtime backlog: ${rows.length} mappings; mode=${options.mode}; status/content/owner/dependencies/metadata checked; live Beads status=${options.mode === "live" ? "CHECKED" : "NOT_RUN"}`);
