# UI-106 review: `PERMISSION_REVOKED` never surfaces as a stable code

## Defect (severity: low, acceptance-adjacent)

`apps/client/src/core/connection-state.ts:448` — the WebSocket
authorization-revoked path calls:

```ts
options.monitor.permissionDenied('PERMISSION_REVOKED')
```

but `PERMISSION_REVOKED` is **not** in the `PUBLIC_CODES` allowlist
(connection-state.ts:70-128). `safeCode()` therefore rewrites it to the
fallback `CLIENT_FAILURE`, so the diagnostic text and the Error Boundary
detail show `code=CLIENT_FAILURE` for every live permission revocation.

Impact: the connection *status* is correct (`permission-denied`) and no
secret leaks — the failure is purely diagnostic fidelity. UI-106's
acceptance criteria promise "保留稳定错误码" (preserve stable error codes)
for the permission-revoked path, and the stable code chosen by the
implementation itself is dropped by its own sanitizer. Supporting evidence:
every other status transition uses allowlisted codes
(`SUBSCRIPTION_RESET_REQUIRED`, `AUTHENTICATION_REQUIRED`,
`PERMISSION_DENIED`, `VERSION_MISMATCH`, `RECONNECTING`, `OFFLINE`); only
this one site was missed.

Verified against the compiled candidate modules
(`review/ui106/verify-permission-revoked-code.mjs`):

```
status: permission-denied
code:  CLIENT_FAILURE
CONFIRMED: PERMISSION_REVOKED is rewritten to CLIENT_FAILURE
```

Note the same bug shape exists for a second caller: nothing else currently
passes a non-allowlisted code, so one allowlist entry fixes all live paths.

## Minimal fix

Add the code to `PUBLIC_CODES` in
`apps/client/src/core/connection-state.ts` (read-only candidate — apply in
the implementation worktree):

```diff
   'PERMISSION_REVOKED',
```

(alphabetically between `PROTOCOL_VERSION_UNSUPPORTED` and `RATE_LIMITED`),
and extend the taint test in `tests/client-global-reliability.test.mjs`
(`failure classification…` test) with:

```ts
const revoked = classifyClientFailure(
  error('authorization', 'PERMISSION_DENIED'),
  'PERMISSION_REVOKED',
  true,
)
// direct monitor check:
const monitor = createConnectionMonitor()
monitor.permissionDenied('PERMISSION_REVOKED')
assert.equal(monitor.state.code, 'PERMISSION_REVOKED')
monitor.close()
```

Alternatively, change the call site to the already-allowlisted
`PERMISSION_DENIED` — either one-line fix restores a stable code.
