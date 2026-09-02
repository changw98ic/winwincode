# winwincode-server

The standalone public network boundary for the embedded Control Plane. One
configured origin serves health, HTTP commands, HTTP queries, and WebSocket
events. Worker execution and provider addresses are not part of this router.

Runtime configuration is supplied through `ServerConfig`. TLS certificate and
key paths, allowed browser origins, bind address, public URL, storage path, and
graceful-shutdown timeout are explicit. A short-lived in-memory bootstrap proof
creates an independent random browser session. SQLite stores only the session's
SHA-256 digest, subject, creation time, expiry, and optional revocation time.

`GeneratedContractDispatcher` is the single application entry. It accepts only
the generated `winwincode/v1` command, query, and WebSocket types; checks that
the authenticated subject equals the request actor; asks the application to
authorize the exact scope; and rejects mismatched response correlation before
bytes reach the public connection.

The binary reads these required values from its environment:

- `WWC_SERVER_BIND`
- `WWC_SERVER_PUBLIC_URL`
- `WWC_SERVER_DATA_DIRECTORY`
- `WWC_SERVER_ALLOWED_ORIGINS`
- `WWC_SERVER_BOOTSTRAP_PROOF`
- `WWC_SERVER_AUTH_SUBJECT`
- `WWC_SERVER_REPOSITORY_ROOT`
- `WWC_SERVER_ORGANIZATION_ID`
- `WWC_SERVER_WORKSPACE_ID`
- `WWC_SERVER_PROJECT_ID`
- `WWC_SERVER_REPOSITORY_ID`
- `GITHUB_REPOSITORY`
- `GITHUB_CREDENTIAL_REFERENCE_ID`
- `GITHUB_API_BASE_URL`
- `SECRET_DIRECTORY`
- `PUBLICATION_REQUESTERS`
- `PUBLICATION_APPROVERS`
- `PUBLICATION_APPROVAL_MAX_AGE_MILLIS`

The repository root and four IDs bind the process to one local Git repository
and its exact tenant scope. Startup opens the repository scanner, candidate
resolver, and durable Delivery execution queue before the listener accepts a
request; invalid or missing values stop startup.

Publication configuration binds the same repository scope to one GitHub
repository, one Credential Reference, and one protected secret directory.
Requester and approver values are comma-separated canonical actor IDs. The
approval age is expressed in milliseconds. Startup validates and installs the
Publication authority and provider registry before the listener accepts a
request; HTTP publication commands do not supply policy or provider facts.

`WWC_SERVER_BOOTSTRAP_WINDOW_SECONDS` optionally changes the ten-minute login
window, and `WWC_SERVER_SESSION_TTL_SECONDS` optionally changes the eight-hour
browser-session lifetime. The Client sends the proof only in the Authorization
header of `POST /api/v1/auth/session`; commands, queries, WebSocket upgrades,
and `DELETE /api/v1/auth/session` use only the secure `wwc_session` cookie.

TLS additionally requires both `WWC_SERVER_TLS_CERTIFICATE` and
`WWC_SERVER_TLS_PRIVATE_KEY`. The binary composes the generated dispatcher,
Control Plane application services, and durable event hub over one configured
SQLite authority. Unsupported state transitions fail through the generated
canonical error envelope.
