# Repository-split tasks in the Engineering Runtime audit

The historical audit snapshot remains 111 mappings, with its original IDs,
classifications, statuses and source digests. `live_scope_retirements` records a
later scope decision; it does not rewrite that snapshot or mark Beads completed.

## Business scope confirmation

For PR #4, the user confirmed: “是不是拆仓任务 拆仓已经完结了 现在仓库已经专注于开源内容了”.
This confirmation is the authority for treating the five repository-split tasks
below as historical work outside the current live backlog. It does not establish
a historical completion timestamp, a destination task ID, or a release receipt.
No such values are inferred, and no authoritative Beads records are restored or
changed by this decision.

| Historical bead | Original scope |
| --- | --- |
| `winwincode-edition.2` | Freeze and publish the first Community core for the three-repository split |
| `winwincode-edition.2.2` | Split Community, Cloud and Enterprise Web applications |
| `winwincode-edition.2.5` | Split the three repositories' build, version and release scripts |
| `winwincode-edition.2.6` | Rebuild API coverage along the three-repository boundaries |
| `winwincode-edition.3` | Establish cross-repository core version locks and protocol acceptance |

## Independently inspected evidence

- [ADR-0031](../decisions/0031-three-product-editions.md) assigns this repository
  the Community product, open execution core and public protocols; the other
  products own their own repositories, tasks, CI and releases.
- [product-repository.json](../../product-repository.json) declares only
  `community` / `winwincode` ownership. Commit
  `8ad30854c7ac323ca7c54b90095318c5ab77b954` records the Community split source;
  `c1727db5a7fbb4d4d1cdd3cde446335bced78089` further converges Community identity
  to one local owner. These support the repository boundary, not a claim that
  every historical downstream release acceptance ran successfully.
- At Git data ref `4e4de07e1430ba4bf0ab2d4253ee2241f190eee1`, Dolt commit
  `k2938c10lmhaptqpn2ff1nh3eu1ucodl` deleted exactly these five records. Its parent
  `urskvkf8om7krd5k6md7brjkoauanbnv` retained matching audit metadata on each;
  all five were still `in_progress` without close reasons. Deletion alone is
  therefore not used as evidence of completion. The business scope confirmation
  above resolves the distinction between that historical state and current scope.

## Continuing enforcement

Only these five exact IDs may be absent from the live record set. All 111
historical mappings still pass the snapshot checks. All other mapped beads must
exist and retain their audit metadata. The four new beads, all 111 ER task-plan
entries, unique owners, acceptance criteria, dependencies and cycle checks remain
mandatory. A retired record that reappears active, claims runtime work, or blocks
an active task fails the audit and requires a scope review. A retained closed
record still has its mapping metadata checked.

Community core publishing, API coverage, licenses, source ownership and runtime
verification remain current obligations under ADR-0031 and the existing source,
release, coverage and Mainline checks. This narrow retirement does not waive them,
prove a downstream release, or change the real benchmark ledger.
