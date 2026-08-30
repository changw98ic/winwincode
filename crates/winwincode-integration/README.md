# WinWinCode Integration Framework

This crate owns connector registration, webhook authentication boundaries,
durable inbound command dispatches, outbound retry receipts, and a secret-safe
audit outbox. Protocol adapters normalize or deliver provider payloads; they do
not create Delivery state or make Control Plane decisions.

Each connector uses the canonical `EnterpriseIntegrationId`, one tenant scope,
and one `CredentialReferenceId`. Raw signatures, external event identifiers,
payloads, credentials, and remote diagnostics are excluded from audit facts.
Inbound command facts and outbound operations use durable digests so exact
replay is harmless while changed reuse is rejected.

The GitHub enterprise adapter binds one GitHub App installation and repository
to that authority. It authenticates raw webhooks with HMAC-SHA256, keeps
ordering watermarks per GitHub resource, uses short-lived installation tokens,
and supports closed Issue comment, pull-request review, and check-run
operations. Remote writes carry a stable operation key; bounded lookup before
each retry makes a recovered lease observe an already-created remote resource
instead of creating it again. GitHub `Retry-After` is a lower bound on the
framework's durable backoff. The same connector configuration can construct the
canonical Publication GitHub adapter without introducing a second provider
contract.

## GitHub App live gate

`tests/github_live_gate.rs` is the production acceptance path for the GitHub
connector. It uses one `IntegrationFramework`, the crate's
`GitHubEnterpriseConnector`, and the canonical Publication coordinator and
GitHub adapter. A captured `issues` webhook becomes a formal
`delivery.create` command. An approved, completed Delivery fact set then drives
the branch, pull request, publication comment/status, pull-request review, and
check run. The same credential reference supplies short-lived installation
credentials to both adapters.

The sandbox GitHub App installation must select exactly one disposable
repository. Its repository permissions must be exactly:

```text
checks: write
contents: write
issues: write
metadata: read
pull_requests: write
statuses: write
```

Its subscribed events must be exactly `check_run`, `issues`, `pull_request`,
and `pull_request_review`. The gate checks the installation before asking for a
one-repository token, then checks that GitHub returned the same repository and
permission set. The API endpoint must use HTTPS and publicly trusted roots;
use `https://api.github.com` for GitHub.com or the `/api/v3` endpoint of a GHES
installation with a publicly trusted certificate.

Create these four input files outside the state directory:

- a JSON configuration file using the schema below;
- the GitHub App RSA private-key PEM, as an owner-only regular file;
- the webhook secret, as an owner-only regular file;
- the exact raw webhook request body captured for the configured Issue.

On Unix, set both credential files to mode `0600`. Symlinks are rejected. The
state directory is created with mode `0700`; input files inside it are rejected.
The configuration is credential-free and rejects unknown fields, embedded
tokens, secrets, and private keys.

```json
{
  "schemaVersion": 1,
  "apiBaseUrl": "https://api.github.com",
  "integrationId": "int_00000000000000000000000001",
  "credentialReferenceId": "crd_00000000000000000000000001",
  "appId": 123456,
  "installationId": 789012,
  "repository": "sandbox-owner/sandbox-repository",
  "scope": {
    "organizationId": "org_00000000000000000000000001",
    "workspaceId": "wsp_00000000000000000000000001",
    "projectId": "prj_00000000000000000000000001",
    "repositoryId": "rep_00000000000000000000000001"
  },
  "webhook": {
    "deliveryId": "captured-github-delivery-id",
    "eventType": "issues",
    "signature256": "sha256=0000000000000000000000000000000000000000000000000000000000000000",
    "receivedAtMillis": 1787880000000,
    "issueNumber": 7
  },
  "delivery": {
    "deliveryId": "dlv_00000000000000000000000001",
    "requestId": "req_00000000000000000000000001",
    "systemActorId": "sys_00000000000000000000000001",
    "deliveryRevision": 21,
    "deliverySpecId": "spec_00000000000000000000000001",
    "deliverySpecRevision": 1,
    "candidateRef": "git-candidate:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "diffSha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    "verdictId": "verdict:sandbox:pass",
    "approvalId": "att_00000000000000000000000001",
    "approvalReviewSetSha256": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
    "candidateCommitId": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "artifactId": "art_00000000000000000000000001",
    "artifactDigest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "approvedBy": "usr_00000000000000000000000001",
    "approvedAtMillis": 1787880000000,
    "productSessionId": "psn_00000000000000000000000001"
  },
  "publication": {
    "publicationId": "pub_00000000000000000000000001",
    "requestId": "req_00000000000000000000000002",
    "baseBranch": "main",
    "headRepository": "sandbox-owner/sandbox-repository",
    "headBranch": "winwincode/github-live-gate",
    "maxApprovalAgeMillis": 86400000
  }
}
```

Replace the example values with the captured webhook and the authoritative
facts of one approved Delivery. `approvedAtMillis` must still be inside
`maxApprovalAgeMillis`, and `candidateCommitId` must exist in the selected
repository. The webhook's repository, Issue number, installation ID, delivery
header, HMAC signature, and raw body must all agree.

Run the deterministic TLS, duplicate-delivery, lease-recovery, `Retry-After`,
permission, and secret-safety checks first:

```bash
cargo test -p winwincode-integration --test github_connector --locked
cargo test -p winwincode-integration --test github_live_gate --locked
```

Then export paths only and run the ignored live test. Do not put credential
contents in environment variables.

```bash
export WINWINCODE_GITHUB_LIVE_GATE=1
export WINWINCODE_GITHUB_LIVE_CONFIG_FILE=/secure/github-live/config.json
export WINWINCODE_GITHUB_LIVE_APP_PRIVATE_KEY_FILE=/secure/github-live/app-private-key.pem
export WINWINCODE_GITHUB_LIVE_WEBHOOK_SECRET_FILE=/secure/github-live/webhook-secret
export WINWINCODE_GITHUB_LIVE_WEBHOOK_PAYLOAD_FILE=/secure/github-live/issues-webhook.json
export WINWINCODE_GITHUB_LIVE_STATE_DIRECTORY=/secure/github-live/state

cargo test -p winwincode-integration --test github_live_gate --locked -- \
  --ignored --exact live_github_app_issue_delivery_publication_trace
```

The live gate replays the webhook, abandons one outbound lease after the remote
review write, resumes through remote lookup, replays the check-run operation,
and scans all integration/publication durable state and audit output for the App
private key, webhook secret, and installation token. It passes only when the
replay is idempotent, recovery performs no second remote write, Publication is
terminal, and the scan is clean.
