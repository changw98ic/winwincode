// SPDX-License-Identifier: Apache-2.0

export const WINWINCODE_CLIENT_METADATA = {
  "http": {
    "authentication": {
      "anonymousAllowed": false,
      "bearerAuth": {
        "format": "JWT",
        "location": "header",
        "name": "Authorization",
        "scheme": "Bearer"
      },
      "credentialsInBodyAllowed": false,
      "credentialsInQueryAllowed": false,
      "sessionCookie": {
        "location": "cookie",
        "name": "wwc_session"
      }
    },
    "commandEndpoint": "/api/v1/commands",
    "queries": [
      {
        "query": "delivery.get",
        "requestDefinition": "DeliveryGetQuery",
        "resultDefinition": "DeliveryDetailProjection"
      },
      {
        "query": "runtime.projection.get",
        "requestDefinition": "RuntimeProjectionGetQuery",
        "resultDefinition": "RuntimeProjectionSnapshot"
      },
      {
        "query": "workrun.get",
        "requestDefinition": "WorkRunGetQuery",
        "resultDefinition": "WorkRunAggregateProjection"
      }
    ],
    "queryEndpoint": "/api/v1/queries",
    "responseCorrelation": {
      "command": "command",
      "mismatch": "reject",
      "query": "query"
    },
    "snapshotHandoff": {
      "deliveryGetCursorPath": "result.readCursor.eventCursor",
      "runtimeProjectionGetCursorPath": "result.eventCursor",
      "subscribeStartAtField": "startAt"
    },
    "strongFlowRead": {
      "bootstrapQuery": "delivery.get",
      "failureMode": "discard_partial_pair",
      "followupQuery": "runtime.projection.get",
      "requestCursorField": "atCursor",
      "resultCursorField": "readCursor"
    },
    "transport": "http"
  },
  "websocket": {
    "authentication": {
      "bearerAuth": {
        "format": "JWT",
        "location": "header",
        "name": "Authorization",
        "scheme": "Bearer"
      },
      "credentialsInFramesAllowed": false,
      "credentialsInUrlQueryAllowed": false,
      "sessionCookie": {
        "location": "cookie",
        "name": "wwc_session"
      },
      "upgradeOnly": true
    },
    "endpoint": "/api/v1/events",
    "invalidationReloadQueries": {
      "product-session": [
        "runtime.projection.get"
      ],
      "work-run": [
        "delivery.get",
        "runtime.projection.get"
      ]
    },
    "resetStrategy": "discard_then_full_reload_by_original_subscription_stream",
    "runtimeEvent": "runtime-projection.invalidated.v1",
    "snapshotHandoff": {
      "acceptedBaselineFrame": "transport.subscription-accepted.v1",
      "authorizationEpochBaseline": "accepted.authorizationEpoch",
      "cursorMustMatchSubscription": [
        "scope",
        "stream"
      ],
      "firstEventSequence": "accepted.cursor.sequence + 1",
      "httpCursorField": "eventCursor",
      "retentionLossCloseCode": 4409,
      "retentionLossFrame": "transport.reset-required.v1",
      "subscribeCursorField": "startAt"
    },
    "transport": "websocket",
    "updateMode": "invalidation_then_http_reload"
  }
} as const

export type AcceptanceCriterionInput = {
  readonly "id": string
  readonly "required": boolean
  readonly "title": string
}

export enum ActionEnforcementDecision {
  Permit = "permit",
  Reject = "reject",
}

export type ActionEnforcementReceiptMessage = {
  readonly "actor": UserActor
  readonly "decision": ActionEnforcementDecision
  readonly "evaluatedAt": Instant
  readonly "evaluationSha256": Sha256Digest
  readonly "jobId": ExecutionJobId
  readonly "kind": "action.enforcement_receipt"
  readonly "lease": ExecutionLeaseStamp
  readonly "matchedConditionSha256": ReadonlyArray<Sha256Digest>
  readonly "messageId": ExecutionMessageId
  readonly "policyKind": ActionPolicyKind
  readonly "policyMode": ActionPolicyMode | null
  readonly "policyVersion": ActionPolicyVersionReference | null
  readonly "requestId": RequestId
  readonly "resource": string
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "subjectSha256": Sha256Digest
  readonly "workerSessionId": WorkerSessionId
}

export type ActionEnforcementRequestMessage = {
  readonly "jobId": ExecutionJobId
  readonly "kind": "action.enforcement_request"
  readonly "lease": ExecutionLeaseStamp
  readonly "matchedConditionSha256": ReadonlyArray<Sha256Digest>
  readonly "messageId": ExecutionMessageId
  readonly "policyKind": ActionPolicyKind
  readonly "requestId": RequestId
  readonly "resource": string
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "subjectSha256": Sha256Digest
  readonly "workerSessionId": WorkerSessionId
}

export enum ActionPolicyKind {
  Repository = "repository",
  Tool = "tool",
  Network = "network",
}

export enum ActionPolicyMode {
  Enforce = "enforce",
  Audit = "audit",
}

export type ActionPolicyVersionReference = {
  readonly "effectiveDefinitionSha256": Sha256Digest
  readonly "policyId": string
  readonly "version": number
  readonly "versionDigest": Sha256Digest
}

export type ActiveLeaseSummary = {
  readonly "attempt": number
  readonly "expiresAt": Instant
  readonly "fencingToken": FencingToken
  readonly "jobId": ExecutionJobId
  readonly "lastEventSequence": ExecutionAckSequence
  readonly "leaseId": LeaseId
}

export type Actor = UserActor | ServiceAccountActor | SystemActor

export type ActorId = UserId | ServiceAccountId | SystemActorId

export type AgentIdentityId = string & { readonly __brand: "AgentIdentityId" }

export type ApiTokenId = string & { readonly __brand: "ApiTokenId" }

export enum AppliedFileOperation {
  Create = "create",
  Update = "update",
  Delete = "delete",
  Move = "move",
}

export type AppliedFileSummary = {
  readonly "afterSha256"?: Sha256Digest
  readonly "beforeSha256"?: Sha256Digest
  readonly "bytesAfter": number
  readonly "bytesBefore": number
  readonly "modeAfter"?: string
  readonly "modeBefore"?: string
  readonly "movePath"?: string
  readonly "operation": AppliedFileOperation
  readonly "path": string
} & ({
  readonly "bytesAfter": number
  readonly "bytesBefore": number
  readonly "movePath"?: string
  readonly "path": string
})

export type ApprovalAction = {
  readonly "category": ApprovalActionCategory
  readonly "sanitizedDetail"?: ApprovalActionSanitizedDetail
  readonly "summary": string
}

export enum ApprovalActionCategory {
  FilesystemWrite = "filesystem_write",
  Mcp = "mcp",
  Network = "network",
  Shell = "shell",
}

export enum ApprovalActionOperation {
  Execute = "execute",
  Modify = "modify",
}

export enum ApprovalActionReasonCode {
  SandboxEscalation = "sandbox_escalation",
  McpPermission = "mcp_permission",
  NetworkAccess = "network_access",
  FilesystemWrite = "filesystem_write",
}

export enum ApprovalActionRiskLevel {
  Medium = "medium",
  High = "high",
}

export type ApprovalActionSanitizedDetail = {
  readonly "kind": "available"
  readonly "operation": ApprovalActionOperation
  readonly "reasonCode": ApprovalActionReasonCode
  readonly "requestSha256": Sha256Digest
  readonly "riskLevel": ApprovalActionRiskLevel
  readonly "targetCount": number
  readonly "targetSummaries": ReadonlyArray<string>
  readonly "workingDirectory": string | null
}

export type ApprovalDecideCommand = {
  readonly "actor": Actor
  readonly "command": "approval.decide"
  readonly "expectedRevision": Revision
  readonly "payload": ApprovalDecidePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type ApprovalDecideCompletedResponse = {
  readonly "command": "approval.decide"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": ApprovalProjection
  readonly "schemaVersion": SchemaVersion
}

export type ApprovalDecidePayload = {
  readonly "approvalId": ApprovalId
  readonly "binding": ChatInteractionBindingProjection
  readonly "decision": "approve" | "reject"
  readonly "reason": string
}

export type ApprovalDecisionMessage = {
  readonly "approvalId": ApprovalId
  readonly "decidedAt": Instant
  readonly "decision": "approved" | "cancelled" | "denied" | "expired"
  readonly "kind": "approval.decision"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "reason"?: string
  readonly "schemaVersion": SchemaVersion
  readonly "scope": "once" | "worker_session"
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "workerSessionId": WorkerSessionId
}

export enum ApprovalEffectiveDecisionScope {
  Once = "once",
}

export type ApprovalGetParameters = {
  readonly "approvalId": ApprovalId
}

export type ApprovalGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": ApprovalGetParameters
  readonly "query": "approval.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type ApprovalGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "approval.get"
  readonly "requestId": RequestId
  readonly "result": ApprovalProjection
  readonly "schemaVersion": SchemaVersion
}

export type ApprovalId = string & { readonly __brand: "ApprovalId" }

export type ApprovalListParameters = {
  readonly "states": ReadonlyArray<"pending" | "approved" | "rejected" | "expired">
}

export type ApprovalListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": ApprovalListParameters
  readonly "query": "approval.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type ApprovalListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "approval.list"
  readonly "requestId": RequestId
  readonly "result": ApprovalPage
  readonly "schemaVersion": SchemaVersion
}

export type ApprovalPage = {
  readonly "items": ReadonlyArray<ApprovalProjection>
  readonly "kind": "approval_page"
}

export type ApprovalProjection = {
  readonly "binding": ChatInteractionBindingProjection
  readonly "category": ApprovalProjectionCategory
  readonly "decisionEnabled": boolean
  readonly "effectiveDecisionScope": ApprovalEffectiveDecisionScope
  readonly "expiresAt": Instant
  readonly "id": ApprovalId
  readonly "requestedAt": Instant
  readonly "revision": Revision
  readonly "sanitizedDetail": ApprovalSanitizedDetailProjection
  readonly "state": "pending" | "approved" | "rejected" | "expired"
  readonly "subject": string
}

export enum ApprovalProjectionCategory {
  FilesystemWrite = "filesystem_write",
  Mcp = "mcp",
  Network = "network",
  Shell = "shell",
}

export type ApprovalRequestMessage = {
  readonly "action": ApprovalAction
  readonly "approvalId": ApprovalId
  readonly "expiresAt": Instant
  readonly "kind": "approval.request"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "workerSessionId": WorkerSessionId
}

export type ApprovalSanitizedDetailAvailableProjection = {
  readonly "kind": "available"
  readonly "operation": "execute" | "modify"
  readonly "reasonCode": "sandbox_escalation" | "mcp_permission" | "network_access" | "filesystem_write"
  readonly "requestSha256": Sha256Digest
  readonly "riskLevel": "medium" | "high"
  readonly "targetCount": number
  readonly "targetSummaries": ReadonlyArray<string>
  readonly "workingDirectory": string | null
}

export type ApprovalSanitizedDetailProjection = ApprovalSanitizedDetailAvailableProjection | ApprovalSanitizedDetailUnavailableProjection

export type ApprovalSanitizedDetailUnavailableProjection = {
  readonly "kind": "unavailable"
  readonly "reason": ApprovalSanitizedDetailUnavailableReason
}

export enum ApprovalSanitizedDetailUnavailableReason {
  ProducerUnavailable = "producer_unavailable",
}

export type ArtifactAckMessage = {
  readonly "ackSequence": ExecutionAckSequence
  readonly "artifactId": ArtifactId
  readonly "error"?: ExecutionPortError
  readonly "kind": "artifact.ack"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "replayFromSequence"?: ExecutionSequence
  readonly "retainedArtifact"?: ArtifactReference
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "status": LeaseWriteStatus
  readonly "workerSessionId": WorkerSessionId
}

export type ArtifactChunkMessage = {
  readonly "artifactId": ArtifactId
  readonly "isFinal": boolean
  readonly "kind": "artifact.chunk"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "payload": EncodedPayload
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sequence": ExecutionSequence
  readonly "sessionIdentity": SessionIdentity
  readonly "snapshotId"?: SnapshotId
  readonly "workerSessionId": WorkerSessionId
}

export type ArtifactDescriptor = {
  readonly "artifactId": ArtifactId
  readonly "digest": Sha256Digest
  readonly "fileName"?: string
  readonly "kind": ArtifactKind
  readonly "mediaType": string
  readonly "sizeBytes": number
}

export type ArtifactId = string & { readonly __brand: "ArtifactId" }

export enum ArtifactKind {
  Candidate = "candidate",
  CommandOutput = "command_output",
  Diff = "diff",
  Log = "log",
  Report = "report",
  TestOutput = "test_output",
  Usage = "usage",
}

export type ArtifactOpenMessage = {
  readonly "artifact": ArtifactDescriptor
  readonly "kind": "artifact.open"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "replacesArtifactId"?: ArtifactId
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "snapshotId"?: SnapshotId
  readonly "workerSessionId": WorkerSessionId
}

export type ArtifactReference = {
  readonly "artifactId": ArtifactId
  readonly "digest": Sha256Digest
}

export type AttentionItemId = string & { readonly __brand: "AttentionItemId" }

export type AuthSessionRequest = {
  readonly "schemaVersion": SchemaVersion
}

export type AuthSessionResponse = {
  readonly "actor": Actor
  readonly "authorizedScopes": ReadonlyArray<Scope>
  readonly "expiresAt": Instant
  readonly "schemaVersion": SchemaVersion
}

export type Candidate = {
  readonly "attempt": number
  readonly "baseCommit": string
  readonly "candidateCommit": string
  readonly "candidateDigest": CandidateDigest
  readonly "candidateRef": string
  readonly "candidateTree": string
  readonly "contractRevision": Revision
  readonly "diffDigest": Sha256Digest
  readonly "id": CandidateId
  readonly "producerWorkerSessionId": WorkerSessionId
  readonly "schemaVersion": SchemaVersion
  readonly "workContractId": WorkContractId
  readonly "workItemId": WorkItemId
  readonly "workRunId": WorkRunId
}

export enum CandidateAvailability {
  Available = "available",
  Released = "released",
}

export type CandidateDiffChunkProjection = {
  readonly "binary": boolean
  readonly "candidate": FrozenCandidateSummaryProjection
  readonly "contentEncoding": CandidateFileEncoding
  readonly "dataBase64": string
  readonly "encoding": "base64"
  readonly "fileDiffSha256": Sha256Digest
  readonly "kind": "candidate_diff_chunk"
  readonly "mediaType": "application/vnd.winwincode.git-diff"
  readonly "nextOffset": number | null
  readonly "offset": number
  readonly "oldPath": string | null
  readonly "path": string
  readonly "readCursor": StrongFlowReadCursor
  readonly "returnedBytes": number
  readonly "status": CandidateFileStatus
  readonly "totalBytes": number
}

export type CandidateDiffGetParameters = {
  readonly "atCursor": StrongFlowReadCursor
  readonly "candidateRef": string
  readonly "candidateTreeId": string
  readonly "deliveryId": DeliveryId
  readonly "diffSha256": Sha256Digest
  readonly "length": number
  readonly "offset": number
  readonly "path": string
  readonly "readPageLimit": number
}

export type CandidateDiffGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CandidateDiffGetParameters
  readonly "query": "candidate.diff.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type CandidateDiffGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "candidate.diff.get"
  readonly "requestId": RequestId
  readonly "result": CandidateDiffChunkProjection
  readonly "schemaVersion": SchemaVersion
}

export type CandidateDigest = string

export type CandidateFileContentChunkProjection = {
  readonly "candidate": FrozenCandidateSummaryProjection
  readonly "contentEncoding": CandidateFileEncoding
  readonly "dataBase64": string
  readonly "encoding": "base64"
  readonly "kind": "candidate_file_content_chunk"
  readonly "mediaType": string
  readonly "nextOffset": number | null
  readonly "offset": number
  readonly "path": string
  readonly "readCursor": StrongFlowReadCursor
  readonly "returnedBytes": number
  readonly "totalBytes": number
}

export type CandidateFileContentGetParameters = {
  readonly "atCursor": StrongFlowReadCursor
  readonly "candidateRef": string
  readonly "candidateTreeId": string
  readonly "deliveryId": DeliveryId
  readonly "diffSha256": Sha256Digest
  readonly "length": number
  readonly "offset": number
  readonly "path": string
  readonly "readPageLimit": number
}

export type CandidateFileContentGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CandidateFileContentGetParameters
  readonly "query": "candidate.file.content.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type CandidateFileContentGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "candidate.file.content.get"
  readonly "requestId": RequestId
  readonly "result": CandidateFileContentChunkProjection
  readonly "schemaVersion": SchemaVersion
}

export enum CandidateFileEncoding {
  Utf8 = "utf-8",
  Binary = "binary",
  Unknown8bit = "unknown-8bit",
}

export type CandidateFilePage = {
  readonly "candidate": FrozenCandidateSummaryProjection
  readonly "items": ReadonlyArray<CandidateFileProjection>
  readonly "kind": "candidate_file_page"
  readonly "readCursor": StrongFlowReadCursor
}

export type CandidateFileProjection = {
  readonly "additions": Count | null
  readonly "binary": boolean
  readonly "deletions": Count | null
  readonly "encoding": CandidateFileEncoding
  readonly "oldPath": string | null
  readonly "path": string
  readonly "status": CandidateFileStatus
}

export type CandidateFilesListParameters = {
  readonly "atCursor": StrongFlowReadCursor
  readonly "candidateRef": string
  readonly "candidateTreeId": string
  readonly "deliveryId": DeliveryId
  readonly "diffSha256": Sha256Digest
  readonly "pathPrefix": string | null
  readonly "readPageLimit": number
  readonly "statuses": ReadonlyArray<CandidateFileStatus>
}

export type CandidateFilesListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CandidateFilesListParameters
  readonly "query": "candidate.files.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type CandidateFilesListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "candidate.files.list"
  readonly "requestId": RequestId
  readonly "result": CandidateFilePage
  readonly "schemaVersion": SchemaVersion
}

export enum CandidateFileStatus {
  Added = "added",
  Modified = "modified",
  Deleted = "deleted",
  Renamed = "renamed",
  Copied = "copied",
  TypeChanged = "type_changed",
}

export type CandidateHistoricalReviewGetParameters = {
  readonly "atCursor": StrongFlowReadCursor
  readonly "candidateRef": string
  readonly "candidateTreeId": string
  readonly "deliveryId": DeliveryId
  readonly "diffSha256": Sha256Digest
  readonly "readPageLimit": number
}

export type CandidateHistoricalReviewGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CandidateHistoricalReviewGetParameters
  readonly "query": "candidate.review.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type CandidateHistoricalReviewGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "candidate.review.get"
  readonly "requestId": RequestId
  readonly "result": CandidateHistoricalReviewProjection
  readonly "schemaVersion": SchemaVersion
}

export type CandidateHistoricalReviewProjection = {
  readonly "availability": CandidateAvailability
  readonly "candidate": FrozenCandidateSummaryProjection
  readonly "currentAuthorization": false
  readonly "displayOnly": true
  readonly "evidence": ReadonlyArray<DeliveryEvidenceProjection>
  readonly "firstSeenDeliveryRevision": Revision
  readonly "kind": "candidate_historical_review"
  readonly "lastSeenDeliveryRevision": Revision
  readonly "readCursor": StrongFlowReadCursor
  readonly "reviewDeliveryRevision": Revision | null
  readonly "verdict": DeliveryVerdictProjection | null
}

export type CandidateHistoryItemProjection = {
  readonly "availability": CandidateAvailability
  readonly "candidate": FrozenCandidateSummaryProjection
  readonly "firstSeenDeliveryRevision": Revision
  readonly "isCurrentAtReadCursor": boolean
  readonly "lastSeenDeliveryRevision": Revision
  readonly "reviewDeliveryRevision": Revision | null
}

export type CandidateHistoryListParameters = {
  readonly "atCursor": StrongFlowReadCursor
  readonly "deliveryId": DeliveryId
  readonly "readPageLimit": number
}

export type CandidateHistoryListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CandidateHistoryListParameters
  readonly "query": "candidate.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type CandidateHistoryListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "candidate.list"
  readonly "requestId": RequestId
  readonly "result": CandidateHistoryPage
  readonly "schemaVersion": SchemaVersion
}

export type CandidateHistoryPage = {
  readonly "items": ReadonlyArray<CandidateHistoryItemProjection>
  readonly "kind": "candidate_history_page"
  readonly "readCursor": StrongFlowReadCursor
}

export type CandidateId = string & { readonly __brand: "CandidateId" }

export type CandidateRef = string

export type ChangeBatchId = string & { readonly __brand: "ChangeBatchId" }

export type ChangeBatchIdentity = {
  readonly "attempt": number
  readonly "batchId": ChangeBatchId
  readonly "callId"?: string
  readonly "fencingToken": FencingToken
  readonly "jobId": ExecutionJobId
  readonly "leaseId": LeaseId
  readonly "patchDigest": Sha256Digest
  readonly "repositoryId": RepositoryId
  readonly "runKey": string
  readonly "sessionIdentity": SessionIdentity
  readonly "turnId": string
  readonly "workspaceRevision": WorkspaceRevision
} & ({
  readonly "attempt": number
  readonly "batchId": string
  readonly "callId"?: string
  readonly "runKey": string
  readonly "turnId": string
  readonly "workspaceRevision": WorkspaceRevision
})

export type ChangeBatchProgressEvent = {
  readonly "artifactRefs": ReadonlyArray<ArtifactReference>
  readonly "identity": ChangeBatchIdentity
  readonly "occurredAt": Instant
  readonly "sequence": number
  readonly "state": ChangeBatchProgressState
  readonly "summary": string
} & ({
  readonly "artifactRefs": ReadonlyArray<unknown>
  readonly "sequence": number
  readonly "summary": string
})

export enum ChangeBatchProgressState {
  Proposed = "proposed",
  Authorized = "authorized",
  ApplyStarted = "apply_started",
  Applied = "applied",
  RollbackStarted = "rollback_started",
  RolledBack = "rolled_back",
  ValidationStarted = "validation_started",
  ValidationCompleted = "validation_completed",
  ObservationRequested = "observation_requested",
  ObservationCompleted = "observation_completed",
  Accepted = "accepted",
  RepairRequired = "repair_required",
  InfrastructureFailed = "infrastructure_failed",
}

export type ChangeBatchProposal = {
  readonly "acceptanceCriteriaIds": ReadonlyArray<string>
  readonly "disposition": ChangeBatchProposalDisposition
  readonly "patch": string
  readonly "schemaVersion": number
  readonly "validationProfile": ValidationProfileName
} & ({
  readonly "acceptanceCriteriaIds": ReadonlyArray<string>
  readonly "patch": string
  readonly "schemaVersion": 1
})

export enum ChangeBatchProposalDisposition {
  Final = "final",
  Continue = "continue",
  Probe = "probe",
}

export type ChangeBatchProposalEvent = {
  readonly "identity": ChangeBatchIdentity
  readonly "occurredAt": Instant
  readonly "proposal": ChangeBatchProposal
}

export type ChangeBatchReceipt = {
  readonly "artifactRef": ArtifactReference | null
  readonly "baseRevision": WorkspaceRevision
  readonly "deltaDigest"?: Sha256Digest
  readonly "deltaExact": boolean
  readonly "files": ReadonlyArray<AppliedFileSummary>
  readonly "identity": ChangeBatchIdentity
  readonly "normalizer": NormalizerReceipt | null
  readonly "observation": ObservationReceipt | null
  readonly "resultRevision"?: WorkspaceRevision
  readonly "status": ChangeBatchReceiptStatus
  readonly "validation": ValidationReceipt | null
} & ({
  readonly "baseRevision": WorkspaceRevision
  readonly "deltaDigest": Sha256Digest
  readonly "deltaExact": true
  readonly "files": ReadonlyArray<unknown>
  readonly "resultRevision": WorkspaceRevision
  readonly "status": ChangeBatchReceiptStatus.Applied
} | {
  readonly "baseRevision": WorkspaceRevision
  readonly "deltaDigest": Sha256Digest
  readonly "deltaExact": true
  readonly "files": ReadonlyArray<unknown>
  readonly "resultRevision": WorkspaceRevision
  readonly "status": ChangeBatchReceiptStatus.Rejected
} | {
  readonly "baseRevision": WorkspaceRevision
  readonly "deltaDigest": Sha256Digest
  readonly "deltaExact": true
  readonly "files": ReadonlyArray<unknown>
  readonly "resultRevision": WorkspaceRevision
  readonly "status": ChangeBatchReceiptStatus.PartiallyApplied
} | {
  readonly "baseRevision": WorkspaceRevision
  readonly "deltaExact": false
  readonly "files": ReadonlyArray<unknown>
  readonly "status": ChangeBatchReceiptStatus.StateUncertain
})

export enum ChangeBatchReceiptStatus {
  Rejected = "rejected",
  Applied = "applied",
  PartiallyApplied = "partially_applied",
  StateUncertain = "state_uncertain",
}

export type ChatApprovalInteractionProjection = {
  readonly "approval": ApprovalProjection
  readonly "kind": "approval"
}

export type ChatArtifactProjection = {
  readonly "artifactId": ArtifactId
  readonly "digest": Sha256Digest
}

export type ChatAttachment = {
  readonly "content": string
  readonly "mediaType": string
  readonly "name": string
}

export type ChatInputInteractionProjection = {
  readonly "allowEmpty": boolean
  readonly "allowOther"?: boolean
  readonly "binding": ChatInteractionBindingProjection
  readonly "expiresAt": Instant
  readonly "inputRequestId": InputRequestId
  readonly "kind": "input"
  readonly "mode": InteractiveInputMode
  readonly "options": ReadonlyArray<ChatInteractionOptionProjection>
  readonly "prompt": string
  readonly "revision": Revision
  readonly "state": "pending" | "provided" | "cancelled" | "expired"
}

export type ChatInteractionBindingProjection = {
  readonly "executionJobId": ExecutionJobId
  readonly "productSessionId": ProductSessionId
  readonly "sessionIdentity": SessionIdentity
  readonly "workerSessionId": WorkerSessionId
}

export type ChatInteractionListParameters = {
  readonly "productSessionId": ProductSessionId
  readonly "states": ReadonlyArray<"pending" | "resolved" | "expired" | "cancelled">
}

export type ChatInteractionListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": ChatInteractionListParameters
  readonly "query": "session.interactions.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type ChatInteractionListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "session.interactions.list"
  readonly "requestId": RequestId
  readonly "result": ChatInteractionPage
  readonly "schemaVersion": SchemaVersion
}

export type ChatInteractionOptionProjection = {
  readonly "id": InteractiveInputChoiceId
  readonly "label": string
  readonly "value": string
}

export type ChatInteractionPage = {
  readonly "items": ReadonlyArray<ChatInteractionProjection>
  readonly "kind": "chat_interaction_page"
}

export type ChatInteractionProjection = ChatInputInteractionProjection | ChatApprovalInteractionProjection

export type ChatMessageId = string & { readonly __brand: "ChatMessageId" }

export type ChatMessagePage = {
  readonly "items": ReadonlyArray<ChatMessageProjection>
  readonly "kind": "chat_message_page"
}

export type ChatMessageProjection = {
  readonly "artifactRefs"?: ReadonlyArray<ChatArtifactProjection>
  readonly "attachments"?: ReadonlyArray<ChatAttachment>
  readonly "content": string
  readonly "createdAt": Instant
  readonly "id": ChatMessageId
  readonly "outputPreparation"?: "preparing"
  readonly "productSessionId": ProductSessionId
  readonly "role": "user" | "assistant"
  readonly "sequence": number
  readonly "state": "streaming" | "completed" | "cancelled" | "failed"
  readonly "updatedAt": Instant
}

export type ChatSubmitCommand = {
  readonly "actor": Actor
  readonly "command": "chat.submit"
  readonly "expectedRevision": Revision
  readonly "payload": ChatSubmitPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type ChatSubmitCompletedResponse = {
  readonly "command": "chat.submit"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": ProductSessionProjection
  readonly "schemaVersion": SchemaVersion
}

export type ChatSubmitPayload = {
  readonly "attachments"?: ReadonlyArray<ChatAttachment>
  readonly "message": string
  readonly "productSessionId": ProductSessionId
}

export type ClientAccessGrant = {
  readonly "clientAccessGrantId": ClientAccessGrantId
  readonly "clientNodeId": ClientNodeId
  readonly "createdAt": Instant
  readonly "expiresAt": Instant | null
  readonly "grantedByUserId": UserId
  readonly "grantSource": ClientAccessGrantSource
  readonly "permissions": ReadonlyArray<ClientAccessGrantPermission>
  readonly "revision": Revision
  readonly "state": ClientAccessGrantState
  readonly "trustMode": ClientTrustMode
  readonly "userId": UserId
}

export type ClientAccessGrantId = string & { readonly __brand: "ClientAccessGrantId" }

export enum ClientAccessGrantPermission {
  Use = "use",
  Manage = "manage",
  Share = "share",
}

export enum ClientAccessGrantSource {
  ConnectCode = "connect_code",
  Administrator = "administrator",
  LocalConfirmation = "local_confirmation",
}

export enum ClientAccessGrantState {
  Active = "active",
  Revoked = "revoked",
  Expired = "expired",
}

export enum ClientArchitecture {
  Aarch64 = "aarch64",
  X8664 = "x86_64",
}

export type ClientCandidateApplyMessage = {
  readonly "candidateRef": CandidateRef
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedHead": GitCommitSha
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.candidate.apply"
  readonly "messageId": ClientControlMessageId
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "requesterUserId": UserId
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "strategy": LocalApplyStrategy
  readonly "targetBranch": GitRefName
}

export type ClientCandidateApplyResultMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.candidate.apply_result"
  readonly "messageId": ClientControlMessageId
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "receipt": LocalApplyReceipt
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientCandidateRetainedMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.candidate.retained"
  readonly "messageId": ClientControlMessageId
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "receipt": LocalCandidateReceipt
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "workerSessionId": WorkerSessionId
}

export type ClientCapacityReport = {
  readonly "drainingWorkerSessions": number
  readonly "maxConcurrentWorkerSessions": number
  readonly "reservedWorkerSessions": number
  readonly "runningWorkerSessions": number
}

export type ClientCommandAckMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "commandKind": ClientControlMessageKind
  readonly "commandMessageId": ClientControlMessageId
  readonly "currentRevision"?: Revision
  readonly "error"?: ClientControlError
  readonly "kind": "client.command_ack"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "status": CommandAckStatus
}

export type ClientConnectCode = {
  readonly "clientNodeId": ClientNodeId
  readonly "codeDigest": Sha256Digest
  readonly "connectCodeId": ClientConnectCodeId
  readonly "createdAt": Instant
  readonly "expiresAt": Instant
  readonly "issuedByInstanceId": ClientInstanceId
  readonly "remainingAttempts": number
  readonly "revision": Revision
  readonly "state": ClientConnectCodeState
}

export type ClientConnectCodeId = string & { readonly __brand: "ClientConnectCodeId" }

export type ClientConnectCodePublishedMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "codeDigest": Sha256Digest
  readonly "connectCodeId": ClientConnectCodeId
  readonly "expectedRevision": Revision
  readonly "expiresAt": Instant
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.connect_code.published"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export enum ClientConnectCodeState {
  Active = "active",
  Consumed = "consumed",
  Expired = "expired",
  Revoked = "revoked",
}

export type ClientControlError = {
  readonly "code": ClientControlErrorCode
  readonly "details"?: ErrorDetails
  readonly "message": string
  readonly "retryable": boolean
}

export enum ClientControlErrorCode {
  ProtocolVersionUnsupported = "PROTOCOL_VERSION_UNSUPPORTED",
  DeviceNotEnrolled = "DEVICE_NOT_ENROLLED",
  DeviceInstanceChanged = "DEVICE_INSTANCE_CHANGED",
  UnknownLease = "UNKNOWN_LEASE",
  StaleFencingToken = "STALE_FENCING_TOKEN",
  RevisionConflict = "REVISION_CONFLICT",
  IdempotencyConflict = "IDEMPOTENCY_CONFLICT",
  CapacityExhausted = "CAPACITY_EXHAUSTED",
  RepositoryUnavailable = "REPOSITORY_UNAVAILABLE",
  GrantInvalid = "GRANT_INVALID",
  WrongState = "WRONG_STATE",
  RateLimited = "RATE_LIMITED",
  InternalError = "INTERNAL_ERROR",
}

export type ClientControlMessage = ClientEnrollMessage | ClientHelloMessage | ClientHeartbeatMessage | ClientConnectCodePublishedMessage | ClientOccupancyAckMessage | ClientOccupancyRejectedMessage | ClientRepositoryUpsertMessage | ClientRepositoryRemovedMessage | ClientRepositoryStatusMessage | ClientWorkerLaunchAckMessage | ClientWorkerStateMessage | ClientWorkerReconcileMessage | ClientCandidateRetainedMessage | ClientCandidateApplyResultMessage | ClientCommandAckMessage | ClientOccupancyOfferMessage | ClientOccupancyReleaseMessage | ClientOccupancyForceFenceMessage | ClientRepositoryRescanMessage | ClientWorkerLaunchMessage | ClientWorkerStopMessage | ClientCandidateApplyMessage | ClientLockMessage | ClientProviderReportMessage | ClientProviderApplyMessage | ClientExtensionReportMessage | ClientExtensionApplyMessage | ClientRepositoryRegisterMessage | ClientRepositoryRegisteredMessage | ClientManagedAppCommandMessage | ClientManagedAppStatusMessage

export type ClientControlMessageId = string & { readonly __brand: "ClientControlMessageId" }

export enum ClientControlMessageKind {
  ClientEnroll = "client.enroll",
  ClientHello = "client.hello",
  ClientHeartbeat = "client.heartbeat",
  ClientConnectCodePublished = "client.connect_code.published",
  ClientOccupancyAck = "client.occupancy.ack",
  ClientOccupancyRejected = "client.occupancy.rejected",
  ClientRepositoryUpsert = "client.repository.upsert",
  ClientRepositoryRemoved = "client.repository.removed",
  ClientRepositoryStatus = "client.repository.status",
  ClientWorkerLaunchAck = "client.worker.launch_ack",
  ClientWorkerState = "client.worker.state",
  ClientWorkerReconcile = "client.worker.reconcile",
  ClientCandidateRetained = "client.candidate.retained",
  ClientCandidateApplyResult = "client.candidate.apply_result",
  ClientCommandAck = "client.command_ack",
  ClientOccupancyOffer = "client.occupancy.offer",
  ClientOccupancyRelease = "client.occupancy.release",
  ClientOccupancyForceFence = "client.occupancy.force_fence",
  ClientRepositoryRescan = "client.repository.rescan",
  ClientWorkerLaunch = "client.worker.launch",
  ClientWorkerStop = "client.worker.stop",
  ClientCandidateApply = "client.candidate.apply",
  ClientClientLock = "client.client_lock",
  ClientProviderReport = "client.provider.report",
  ClientProviderApply = "client.provider.apply",
  ClientExtensionApply = "client.extension.apply",
  ClientExtensionReport = "client.extension.report",
  ClientRepositoryRegister = "client.repository.register",
  ClientRepositoryRegistered = "client.repository.registered",
  ClientManagedAppCommand = "client.managed_app.command",
  ClientManagedAppStatus = "client.managed_app.status",
}

export type ClientDisplayName = string

export type ClientEnrollMessage = {
  readonly "architecture": ClientArchitecture
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "clientVersion": ClientVersion
  readonly "displayName": ClientDisplayName
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.enroll"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "platform": ClientPlatformTarget
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientExchangeSequence = number

export type ClientExtensionApplyMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "encrypted": DeviceConfigurationEnvelope
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.extension.apply"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientExtensionReportMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "kind": "client.extension.report"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "receipt": DeviceExtensionReceipt | null
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "snapshot": DeviceExtensionSnapshot
}

export type ClientHeartbeatMessage = {
  readonly "acceptingConnections": boolean
  readonly "capacity": ClientCapacityReport
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "kind": "client.heartbeat"
  readonly "lockState": ClientLockState
  readonly "messageId": ClientControlMessageId
  readonly "occupancyLeaseId": ClientOccupancyLeaseId | null
  readonly "occurredAt": Instant
  readonly "presenceState": ClientNodePresenceState
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientHelloMessage = {
  readonly "acceptingConnections": boolean
  readonly "capacity": ClientCapacityReport
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "clientVersion": ClientVersion
  readonly "kind": "client.hello"
  readonly "lockState": ClientLockState
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "presenceState": ClientNodePresenceState
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientInstanceId = string & { readonly __brand: "ClientInstanceId" }

export type ClientLockMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.client_lock"
  readonly "lockState": ClientLockState
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export enum ClientLockState {
  Unlocked = "unlocked",
  Locked = "locked",
}

export type ClientManagedAppCommandMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "command": ManagedAppCommand
  readonly "kind": "client.managed_app.command"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientManagedAppStatusMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "commandMessageId": ClientControlMessageId
  readonly "kind": "client.managed_app.status"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "status": ManagedAppStatus
}

export type ClientNode = {
  readonly "acceptingConnections": boolean
  readonly "architecture": ClientArchitecture
  readonly "clientNodeId": ClientNodeId
  readonly "clientVersion": ClientVersion
  readonly "createdAt": Instant
  readonly "currentInstanceId": ClientInstanceId
  readonly "displayName": ClientDisplayName
  readonly "lastHeartbeatAt": Instant
  readonly "lockState": ClientLockState
  readonly "maxConcurrentWorkerSessions": number
  readonly "platform": ClientPlatformTarget
  readonly "presenceState": ClientNodePresenceState
  readonly "publicClientId": PublicClientId
  readonly "reportedRunningWorkerSessions": number
  readonly "revision": Revision
}

export type ClientNodeId = string & { readonly __brand: "ClientNodeId" }

export enum ClientNodePresenceState {
  PendingEnrollment = "pending_enrollment",
  Online = "online",
  Degraded = "degraded",
  Offline = "offline",
  Locked = "locked",
  Revoked = "revoked",
}

export type ClientOccupancyAckMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.occupancy.ack"
  readonly "messageId": ClientControlMessageId
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientOccupancyForceFenceMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.occupancy.force_fence"
  readonly "messageId": ClientControlMessageId
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "reason": ClientOccupancyForceFenceReason
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "supersededLeaseId": ClientOccupancyLeaseId | null
}

export enum ClientOccupancyForceFenceReason {
  RecoveryDeadlineExceeded = "recovery_deadline_exceeded",
  AdministratorForceClean = "administrator_force_clean",
}

export type ClientOccupancyLease = {
  readonly "acknowledgedAt": Instant | null
  readonly "claimedAt": Instant
  readonly "claimRequestId": OccupancyClaimId
  readonly "clientNodeId": ClientNodeId
  readonly "clientOccupancyLeaseId": ClientOccupancyLeaseId
  readonly "fencingToken": OccupancyFencingToken
  readonly "holderUserId": UserId
  readonly "idleExpiresAt": Instant | null
  readonly "lastRenewedAt": Instant | null
  readonly "recoveryDeadlineAt": Instant | null
  readonly "releasedAt": Instant | null
  readonly "releaseReason": ClientOccupancyReleaseReason | null
  readonly "releaseRequestedAt": Instant | null
  readonly "revision": Revision
  readonly "state": ClientOccupancyLeaseState
}

export type ClientOccupancyLeaseId = string & { readonly __brand: "ClientOccupancyLeaseId" }

export enum ClientOccupancyLeaseState {
  Available = "available",
  Reserving = "reserving",
  Occupied = "occupied",
  Draining = "draining",
  RecoveryPending = "recovery_pending",
  Released = "released",
  Expired = "expired",
}

export type ClientOccupancyOfferMessage = {
  readonly "claimedAt": Instant
  readonly "claimRequestId": OccupancyClaimId
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "holderUserId": UserId
  readonly "idempotencyKey": IdempotencyKey
  readonly "idleExpiresAt": Instant | null
  readonly "kind": "client.occupancy.offer"
  readonly "messageId": ClientControlMessageId
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientOccupancyRejectedMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.occupancy.rejected"
  readonly "messageId": ClientControlMessageId
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "reason": OccupancyRejectReason
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientOccupancyReleaseMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.occupancy.release"
  readonly "messageId": ClientControlMessageId
  readonly "mode": ClientOccupancyReleaseMode
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export enum ClientOccupancyReleaseMode {
  Immediate = "immediate",
  DrainThenRelease = "drain_then_release",
  CancelTasksAndRelease = "cancel_tasks_and_release",
}

export enum ClientOccupancyReleaseReason {
  UserRequested = "user_requested",
  CancelTasksAndRelease = "cancel_tasks_and_release",
  Drained = "drained",
  IdleTimeout = "idle_timeout",
  Revoked = "revoked",
  ForceFenced = "force_fenced",
}

export enum ClientPlatformTarget {
  Aarch64AppleDarwin = "aarch64-apple-darwin",
  X8664AppleDarwin = "x86_64-apple-darwin",
  Aarch64UnknownLinuxGnu = "aarch64-unknown-linux-gnu",
  X8664UnknownLinuxGnu = "x86_64-unknown-linux-gnu",
}

export type ClientProviderApplyMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "encrypted": DeviceConfigurationEnvelope
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.provider.apply"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientProviderReportMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "kind": "client.provider.report"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "receipt": DeviceProviderReceipt | null
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "snapshot": DeviceProviderSnapshot
}

export type ClientRepositoryRegisteredMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "kind": "client.repository.registered"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "receipt": DeviceRepositoryRegistrationReceipt
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientRepositoryRegisterMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "encrypted": DeviceConfigurationEnvelope
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.repository.register"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientRepositoryRemovedMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.repository.removed"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientRepositoryRescanMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.repository.rescan"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "reason": ClientRepositoryRescanReason
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export enum ClientRepositoryRescanReason {
  OccupantRequested = "occupant_requested",
  OccupancyRecovered = "occupancy_recovered",
  Policy = "policy",
}

export type ClientRepositoryStatusMessage = {
  readonly "availability": RepositoryAvailability
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "dirtyState": RepositoryDirtyState
  readonly "headCommit": GitCommitSha
  readonly "kind": "client.repository.status"
  readonly "lastScannedAt": Instant
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientRepositoryUpsertMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.repository.upsert"
  readonly "messageId": ClientControlMessageId
  readonly "occurredAt": Instant
  readonly "repository": RepositoryBindingProjection
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export enum ClientTrustMode {
  Temporary = "temporary",
  Trusted = "trusted",
}

export type ClientVersion = string

export type ClientWorkerLaunchAckMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "error"?: ClientControlError
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.worker.launch_ack"
  readonly "messageId": ClientControlMessageId
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "processClosure"?: ClientWorkerProcessClosure | null
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "status": WorkerLaunchAckStatus
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
  readonly "workerLaunchGrantId": WorkerLaunchGrantId
  readonly "workerSessionId": WorkerSessionId
}

export type ClientWorkerLaunchMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.worker.launch"
  readonly "launchGrant": WorkerLaunchGrant
  readonly "messageId": ClientControlMessageId
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
}

export type ClientWorkerProcessClosure = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "neverStarted": boolean
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "processBootDigest": Sha256Digest | null
  readonly "reportingClientInstanceId": ClientInstanceId
  readonly "workerLaunchGrantId": WorkerLaunchGrantId
}

export type ClientWorkerReconcileMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "kind": "client.worker.reconcile"
  readonly "messageId": ClientControlMessageId
  readonly "occupancyLeaseId": ClientOccupancyLeaseId | null
  readonly "occurredAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "workers": ReadonlyArray<ClientWorkerReconciliation>
}

export type ClientWorkerReconciliation = {
  readonly "observedAt": Instant
  readonly "reconcileState": WorkerReconcileState
  readonly "workerInstanceId": WorkerInstanceId
  readonly "workerSessionId": WorkerSessionId
}

export enum ClientWorkerRunState {
  Starting = "starting",
  Running = "running",
  Draining = "draining",
  Stopping = "stopping",
  Stopped = "stopped",
  Crashed = "crashed",
  Missing = "missing",
  Unknown = "unknown",
}

export type ClientWorkerStateMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "exitCode"?: number | null
  readonly "kind": "client.worker.state"
  readonly "messageId": ClientControlMessageId
  readonly "observedAt": Instant
  readonly "occupancyLeaseId": ClientOccupancyLeaseId | null
  readonly "occurredAt": Instant
  readonly "processClosure"?: ClientWorkerProcessClosure | null
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "state": ClientWorkerRunState
  readonly "workerInstanceId": WorkerInstanceId
  readonly "workerSessionId": WorkerSessionId
}

export type ClientWorkerStopMessage = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expectedRevision": Revision
  readonly "idempotencyKey": IdempotencyKey
  readonly "kind": "client.worker.stop"
  readonly "messageId": ClientControlMessageId
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "occurredAt": Instant
  readonly "reason": ClientWorkerStopReason
  readonly "schemaVersion": SchemaVersion
  readonly "sequence": ClientExchangeSequence
  readonly "workerId": WorkerId
  readonly "workerSessionId": WorkerSessionId
}

export enum ClientWorkerStopReason {
  OccupantRequested = "occupant_requested",
  DrainingComplete = "draining_complete",
  LeaseRecovered = "lease_recovered",
  GrantRevoked = "grant_revoked",
  Superseded = "superseded",
}

export type CodexThreadId = string & { readonly __brand: "CodexThreadId" }

export enum CollaborationActivityCategory {
  Product = "product",
  Runtime = "runtime",
  Approval = "approval",
  Publication = "publication",
  Collaboration = "collaboration",
  Security = "security",
}

export type CollaborationActivityListParameters = {
  readonly "categories": ReadonlyArray<CollaborationActivityCategory>
  readonly "deliveryId": DeliveryId | null
  readonly "productSessionId": ProductSessionId | null
}

export type CollaborationActivityListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CollaborationActivityListParameters
  readonly "query": "collaboration.activity.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CollaborationActivityListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "collaboration.activity.list"
  readonly "requestId": RequestId
  readonly "result": CollaborationActivityPage
  readonly "schemaVersion": SchemaVersion
}

export type CollaborationActivityPage = {
  readonly "items": ReadonlyArray<CollaborationActivityProjection>
  readonly "kind": "collaboration_activity_page"
  readonly "snapshotRevision": Revision
}

export type CollaborationActivityProjection = {
  readonly "actor": Actor
  readonly "category": CollaborationActivityCategory
  readonly "deliveryId"?: DeliveryId | null
  readonly "occurredAt": Instant
  readonly "productSessionId"?: ProductSessionId | null
  readonly "sequence": number
  readonly "summary": string
}

export type CollaborationNotificationAckCommand = {
  readonly "actor": Actor
  readonly "command": "collaboration.notification.ack"
  readonly "expectedRevision": Revision
  readonly "payload": CollaborationNotificationAckPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CollaborationNotificationAckCompletedResponse = {
  readonly "command": "collaboration.notification.ack"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": CollaborationNotificationReceiptProjection
  readonly "schemaVersion": SchemaVersion
}

export type CollaborationNotificationAckPayload = {
  readonly "throughSequence": number
}

export type CollaborationNotificationListParameters = {
  readonly "categories": ReadonlyArray<CollaborationActivityCategory>
  readonly "states": ReadonlyArray<CollaborationNotificationState>
}

export type CollaborationNotificationListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CollaborationNotificationListParameters
  readonly "query": "collaboration.notification.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CollaborationNotificationListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "collaboration.notification.list"
  readonly "requestId": RequestId
  readonly "result": CollaborationNotificationPage
  readonly "schemaVersion": SchemaVersion
}

export type CollaborationNotificationPage = {
  readonly "items": ReadonlyArray<CollaborationNotificationProjection>
  readonly "kind": "collaboration_notification_page"
  readonly "snapshotRevision": Revision
}

export type CollaborationNotificationProjection = {
  readonly "acknowledgedAt": Instant | null
  readonly "activity": CollaborationActivityProjection
  readonly "state": CollaborationNotificationState
}

export type CollaborationNotificationReceiptProjection = {
  readonly "acknowledgedAt": Instant
  readonly "revision": Revision
  readonly "throughSequence": number
  readonly "userId": UserId
}

export enum CollaborationNotificationState {
  Unread = "unread",
  Read = "read",
}

export type CollaborationPageAnnotationArtifact = {
  readonly "artifactId": ArtifactId
  readonly "digest": Sha256Digest
} | null

export type CollaborationPageAnnotationCandidate = {
  readonly "attempt": number
  readonly "candidateDigest": Sha256Digest
  readonly "candidateId": CandidateId
  readonly "candidateRef": string
  readonly "candidateTreeId": string
  readonly "deliveryId": DeliveryId
  readonly "deliverySpecId": string
  readonly "deliverySpecRevision": number
  readonly "diffSha256": Sha256Digest
  readonly "sessionBindingId": string
  readonly "snapshotId": SnapshotId
  readonly "workRunId": WorkRunId
}

export type CollaborationPageAnnotationItemId = {
  readonly "id": string
  readonly "kind": "approval" | "gate_attention" | "delivery_attention"
}

export type CollaborationPageAnnotationListParameters = {
  readonly "deliveryId": DeliveryId
}

export type CollaborationPageAnnotationListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CollaborationPageAnnotationListParameters
  readonly "query": "collaboration.page_annotation.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type CollaborationPageAnnotationListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "collaboration.page_annotation.list"
  readonly "requestId": RequestId
  readonly "result": CollaborationPageAnnotationPage
  readonly "schemaVersion": SchemaVersion
}

export type CollaborationPageAnnotationPage = {
  readonly "catalogRevision": Revision
  readonly "items": ReadonlyArray<CollaborationPageAnnotationProjection>
  readonly "kind": "collaboration_page_annotation_page"
}

export type CollaborationPageAnnotationProjection = {
  readonly "authorUserId": UserId
  readonly "body": string
  readonly "candidate": CollaborationPageAnnotationCandidate
  readonly "derivedAttentionId": AttentionItemId
  readonly "evidenceId": EvidenceId
  readonly "id": string
  readonly "itemId": CollaborationPageAnnotationItemId
  readonly "revision": number
  readonly "screenshotArtifact": CollaborationPageAnnotationArtifact
  readonly "sourceRevision": number
  readonly "sourceSha256": Sha256Digest
  readonly "state": "active" | "revoked"
  readonly "target": CollaborationPageAnnotationTarget
  readonly "updatedAt": number
}

export type CollaborationPageAnnotationSubmitCommand = {
  readonly "actor": Actor
  readonly "command": "collaboration.page_annotation.submit"
  readonly "expectedRevision": Revision
  readonly "payload": CollaborationPageAnnotationSubmitPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type CollaborationPageAnnotationSubmitCompletedResponse = {
  readonly "command": "collaboration.page_annotation.submit"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": CollaborationPageAnnotationSubmitResult
  readonly "schemaVersion": SchemaVersion
}

export type CollaborationPageAnnotationSubmitPayload = {
  readonly "annotationId": string
  readonly "body": string
  readonly "candidate": CollaborationPageAnnotationCandidate
  readonly "itemId": CollaborationPageAnnotationItemId
  readonly "screenshotArtifact": CollaborationPageAnnotationArtifact
  readonly "target": CollaborationPageAnnotationTarget
}

export type CollaborationPageAnnotationSubmitResult = {
  readonly "annotation": CollaborationPageAnnotationProjection
  readonly "catalogRevision": number
  readonly "replayed": boolean
}

export type CollaborationPageAnnotationTarget = {
  readonly "element"?: {
  readonly "accessibleName"?: string | null
  readonly "height": number
  readonly "locator": string
  readonly "role"?: string | null
  readonly "tagName": string
  readonly "width": number
  readonly "x": number
  readonly "y": number
} | null
  readonly "pagePath": string
  readonly "region": {
  readonly "height": number
  readonly "width": number
  readonly "x": number
  readonly "y": number
}
  readonly "viewport": {
  readonly "devicePixelRatio": number
  readonly "height": number
  readonly "width": number
}
}

export type CollaborationPresenceListParameters = {
  readonly "productSessionId": ProductSessionId | null
  readonly "states": ReadonlyArray<CollaborationPresenceState>
}

export type CollaborationPresenceListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CollaborationPresenceListParameters
  readonly "query": "collaboration.presence.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CollaborationPresenceListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "collaboration.presence.list"
  readonly "requestId": RequestId
  readonly "result": CollaborationPresencePage
  readonly "schemaVersion": SchemaVersion
}

export type CollaborationPresencePage = {
  readonly "items": ReadonlyArray<CollaborationPresenceProjection>
  readonly "kind": "collaboration_presence_page"
  readonly "snapshotRevision": Revision
}

export type CollaborationPresenceProjection = {
  readonly "expiresAt": Instant | null
  readonly "observedAt": Instant
  readonly "productSessionId": ProductSessionId | null
  readonly "revision": Revision
  readonly "state": CollaborationPresenceState
  readonly "userId": UserId
}

export enum CollaborationPresenceState {
  Online = "online",
  Away = "away",
  Offline = "offline",
}

export type CollaborationPresenceUpdateCommand = {
  readonly "actor": Actor
  readonly "command": "collaboration.presence.update"
  readonly "expectedRevision": Revision
  readonly "payload": CollaborationPresenceUpdatePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CollaborationPresenceUpdateCompletedResponse = {
  readonly "command": "collaboration.presence.update"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": CollaborationPresenceProjection
  readonly "schemaVersion": SchemaVersion
}

export type CollaborationPresenceUpdatePayload = {
  readonly "leaseDurationMillis": number | null
  readonly "productSessionId": ProductSessionId | null
  readonly "state": CollaborationPresenceState
}

export type CommandAcceptedResponse = {
  readonly "acceptedAt": Instant
  readonly "command": CommandName
  readonly "currentRevision": Revision
  readonly "outcome": "accepted"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
}

export enum CommandAckStatus {
  Accepted = "accepted",
  Duplicate = "duplicate",
  RejectedUnknownCommand = "rejected_unknown_command",
  RejectedRevisionConflict = "rejected_revision_conflict",
  RejectedStaleFencingToken = "rejected_stale_fencing_token",
  RejectedLeaseMismatch = "rejected_lease_mismatch",
  RejectedWrongState = "rejected_wrong_state",
  RejectedUnauthorized = "rejected_unauthorized",
  RejectedCapacityExhausted = "rejected_capacity_exhausted",
}

export type CommandCompletedEnvelope = {
  readonly "command": CommandName
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": unknown
  readonly "schemaVersion": SchemaVersion
}

export type CommandCompletedResponse = SessionCreateCompletedResponse | ChatSubmitCompletedResponse | InputRespondCompletedResponse | SessionCancelCompletedResponse | SessionCloseCompletedResponse | SessionUpdateCompletedResponse | DeliveryCreateCompletedResponse | DeliveryUpdateSpecCompletedResponse | WorkRunStartCompletedResponse | WorkRunCancelCompletedResponse | DeliveryResolveAttentionCompletedResponse | DeliverySubmitVerdictCompletedResponse | SettingsUpdateCompletedResponse | CredentialReferenceCreateCompletedResponse | CredentialReferenceRotateCompletedResponse | CredentialReferenceRevokeCompletedResponse | CredentialReferenceDeleteCompletedResponse | ApprovalDecideCompletedResponse | WorkerDrainCompletedResponse | WorkerEnableCompletedResponse | PublicationPublishCompletedResponse | PublicationCancelCompletedResponse | CollaborationNotificationAckCompletedResponse | CollaborationPresenceUpdateCompletedResponse | CollaborationPageAnnotationSubmitCompletedResponse | WorkItemsCreateCompletedResponse

export type CommandEnvelope = {
  readonly "actor": Actor
  readonly "command": CommandName
  readonly "expectedRevision": Revision
  readonly "payload": Readonly<Record<string, unknown>> & {}
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export enum CommandName {
  SessionCreate = "session.create",
  ChatSubmit = "chat.submit",
  InputRespond = "input.respond",
  SessionCancel = "session.cancel",
  SessionClose = "session.close",
  SessionUpdate = "session.update",
  DeliveryCreate = "delivery.create",
  DeliveryUpdateSpec = "delivery.update_spec",
  WorkRunStart = "workrun.start",
  WorkRunCancel = "workrun.cancel",
  DeliveryResolveAttention = "delivery.resolve_attention",
  DeliverySubmitVerdict = "delivery.submit_verdict",
  SettingsUpdate = "settings.update",
  CredentialReferenceCreate = "credential.reference.create",
  CredentialReferenceRotate = "credential.reference.rotate",
  CredentialReferenceRevoke = "credential.reference.revoke",
  CredentialReferenceDelete = "credential.reference.delete",
  ApprovalDecide = "approval.decide",
  WorkerDrain = "worker.drain",
  WorkerEnable = "worker.enable",
  PublicationPublish = "publication.publish",
  PublicationCancel = "publication.cancel",
  CollaborationNotificationAck = "collaboration.notification.ack",
  CollaborationPresenceUpdate = "collaboration.presence.update",
  CollaborationPageAnnotationSubmit = "collaboration.page_annotation.submit",
  WorkitemsCreate = "workitems.create",
}

export type CommandRequest = SessionCreateCommand | ChatSubmitCommand | InputRespondCommand | SessionCancelCommand | SessionCloseCommand | SessionUpdateCommand | DeliveryCreateCommand | DeliveryUpdateSpecCommand | WorkRunStartCommand | WorkRunCancelCommand | DeliveryResolveAttentionCommand | DeliverySubmitVerdictCommand | SettingsUpdateCommand | CredentialReferenceCreateCommand | CredentialReferenceRotateCommand | CredentialReferenceRevokeCommand | CredentialReferenceDeleteCommand | ApprovalDecideCommand | WorkerDrainCommand | WorkerEnableCommand | PublicationPublishCommand | PublicationCancelCommand | CollaborationNotificationAckCommand | CollaborationPresenceUpdateCommand | CollaborationPageAnnotationSubmitCommand | WorkItemsCreateCommand

export type ControlPlaneEventId = string & { readonly __brand: "ControlPlaneEventId" }

export type ControlPlaneWebSocketAckFrame = {
  readonly "cursor": ControlPlaneWebSocketAcknowledgedCursor
  readonly "subscriptionId": ControlPlaneWebSocketSubscriptionId
  readonly "type": "transport.ack.v1"
}

export type ControlPlaneWebSocketAcknowledgedCursor = {
  readonly "eventId": ControlPlaneEventId
  readonly "scope": Scope
  readonly "sequence": ControlPlaneWebSocketEventSequence
  readonly "stream": EventReadStream
}

export type ControlPlaneWebSocketActivityRecordedEvent = {
  readonly "actor": Actor
  readonly "category": "product" | "runtime" | "approval" | "publication" | "collaboration" | "security"
  readonly "deliveryId"?: DeliveryId
  readonly "productSessionId"?: ProductSessionId
  readonly "summary": string
  readonly "type": "activity.recorded.v1"
}

export type ControlPlaneWebSocketApprovalChangedEvent = {
  readonly "approvalId": ApprovalId
  readonly "decidedBy": Actor | null
  readonly "decisionReason"?: string
  readonly "productSessionId": ProductSessionId
  readonly "requestedBy": Actor
  readonly "state": "pending" | "approved" | "denied" | "expired" | "cancelled"
  readonly "subject": string
  readonly "type": "approval.changed.v1"
}

export type ControlPlaneWebSocketApprovalListReloadQuery = "approval.list"

export type ControlPlaneWebSocketAttentionChangedEvent = {
  readonly "assignedTo": ActorId | null
  readonly "attentionItemId": AttentionItemId
  readonly "category": "credential" | "execution-approval" | "user-input" | "provider" | "verification" | "business"
  readonly "deliveryId": DeliveryId
  readonly "revision": Revision
  readonly "status": "blocking" | "resolved" | "obsolete"
  readonly "summary": string
  readonly "type": "attention.changed.v1"
  readonly "workRunId"?: WorkRunId
}

export type ControlPlaneWebSocketAuthorizationEpoch = number

export type ControlPlaneWebSocketAuthorizationRevokedFrame = {
  readonly "authorizationEpoch": ControlPlaneWebSocketAuthorizationEpoch
  readonly "closeCode": 4403
  readonly "subscriptionId": ControlPlaneWebSocketSubscriptionId
  readonly "type": "transport.authorization-revoked.v1"
}

export type ControlPlaneWebSocketBackpressureFrame = {
  readonly "ackRequiredThrough": ControlPlaneWebSocketAcknowledgedCursor
  readonly "closeCode": 4408
  readonly "disconnectAt": Instant
  readonly "pendingEventCount": number
  readonly "subscriptionId": ControlPlaneWebSocketSubscriptionId
  readonly "type": "transport.backpressure.v1"
}

export type ControlPlaneWebSocketChatInteractionListReloadQuery = "session.interactions.list"

export type ControlPlaneWebSocketChatInteractionsInvalidatedEvent = {
  readonly "productSessionId": ProductSessionId
  readonly "reloadQueries": readonly [ControlPlaneWebSocketChatInteractionListReloadQuery, ControlPlaneWebSocketApprovalListReloadQuery]
  readonly "revision": Revision
  readonly "type": "chat-interactions.invalidated.v1"
}

export type ControlPlaneWebSocketClientFrame = ControlPlaneWebSocketSubscribeFrame | ControlPlaneWebSocketResumeFrame | ControlPlaneWebSocketAckFrame | ControlPlaneWebSocketPongFrame

export type ControlPlaneWebSocketControlPlaneSource = {
  readonly "actor": Actor
  readonly "component": string
  readonly "kind": "control-plane"
}

export type ControlPlaneWebSocketDeliveryChangedEvent = {
  readonly "changeKind": "created" | "advanced" | "reworked" | "cancelled" | "published"
  readonly "deliveryId": DeliveryId
  readonly "revision": Revision
  readonly "type": "delivery.changed.v1"
}

export type ControlPlaneWebSocketDeliveryGetReloadQuery = "delivery.get"

export type ControlPlaneWebSocketEventFrame = {
  readonly "authorizationEpoch": ControlPlaneWebSocketAuthorizationEpoch
  readonly "event": ControlPlaneWebSocketEventPayload
  readonly "eventId": ControlPlaneEventId
  readonly "occurredAt": Instant
  readonly "scope": Scope
  readonly "sequence": ControlPlaneWebSocketEventSequence
  readonly "source": ControlPlaneWebSocketEventSource
  readonly "stream": EventReadStream
  readonly "subscriptionId": ControlPlaneWebSocketSubscriptionId
  readonly "type": "event.v1"
}

export type ControlPlaneWebSocketEventPayload = ControlPlaneWebSocketProductSessionChangedEvent | ControlPlaneWebSocketProductSessionMessageAppendedEvent | ControlPlaneWebSocketRuntimeProjectionInvalidatedEvent | ControlPlaneWebSocketApprovalChangedEvent | ControlPlaneWebSocketChatInteractionsInvalidatedEvent | ControlPlaneWebSocketAttentionChangedEvent | ControlPlaneWebSocketDeliveryChangedEvent | ControlPlaneWebSocketWorkItemChangedEvent | ControlPlaneWebSocketPresenceChangedEvent | ControlPlaneWebSocketWorkerHealthChangedEvent | ControlPlaneWebSocketModelRouteAvailabilityInvalidatedEvent | ControlPlaneWebSocketActivityRecordedEvent

export type ControlPlaneWebSocketEventSequence = number

export type ControlPlaneWebSocketEventSource = ControlPlaneWebSocketControlPlaneSource | ControlPlaneWebSocketExecutionWorkerSource | ControlPlaneWebSocketSessionExecutionWorkerSource

export enum ControlPlaneWebSocketEventType {
  ActivityRecordedV1 = "activity.recorded.v1",
  ApprovalChangedV1 = "approval.changed.v1",
  ChatInteractionsInvalidatedV1 = "chat-interactions.invalidated.v1",
  AttentionChangedV1 = "attention.changed.v1",
  DeliveryChangedV1 = "delivery.changed.v1",
  WorkItemChangedV1 = "work-item.changed.v1",
  PresenceChangedV1 = "presence.changed.v1",
  ProductSessionChangedV1 = "product-session.changed.v1",
  ProductSessionMessageAppendedV1 = "product-session.message.appended.v1",
  RuntimeProjectionInvalidatedV1 = "runtime-projection.invalidated.v1",
  WorkerHealthChangedV1 = "worker-health.changed.v1",
  ModelRouteAvailabilityInvalidatedV1 = "model-route-availability.invalidated.v1",
}

export type ControlPlaneWebSocketExecutionWorkerSource = {
  readonly "codexThreadId": CodexThreadId
  readonly "kind": "execution-worker"
  readonly "leaseId": LeaseId
  readonly "workerId": WorkerId
  readonly "workerSessionId": WorkerSessionId
}

export type ControlPlaneWebSocketModelRouteAvailabilityInvalidatedEvent = {
  readonly "reloadQueries": readonly [ControlPlaneWebSocketModelRouteAvailabilityListReloadQuery]
  readonly "source": ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource
  readonly "sourceRevision": Revision
  readonly "type": "model-route-availability.invalidated.v1"
}

export enum ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource {
  Settings = "settings",
  ProviderCatalog = "provider_catalog",
  CredentialReference = "credential_reference",
  RequestPool = "request_pool",
  RuntimeStatus = "runtime_status",
}

export type ControlPlaneWebSocketModelRouteAvailabilityListReloadQuery = "model.route.availability.list"

export type ControlPlaneWebSocketPingFrame = {
  readonly "nonce": string
  readonly "sentAt": Instant
  readonly "type": "transport.ping.v1"
}

export type ControlPlaneWebSocketPongFrame = {
  readonly "nonce": string
  readonly "type": "transport.pong.v1"
}

export type ControlPlaneWebSocketPresenceChangedEvent = {
  readonly "observedAt": Instant
  readonly "productSessionId"?: ProductSessionId
  readonly "state": "online" | "away" | "offline"
  readonly "type": "presence.changed.v1"
  readonly "userId": UserId
}

export type ControlPlaneWebSocketProductSessionChangedEvent = {
  readonly "productSessionId": ProductSessionId
  readonly "revision": Revision
  readonly "status": "active" | "waiting-input" | "waiting-approval" | "completed" | "failed" | "cancelled"
  readonly "title"?: string
  readonly "type": "product-session.changed.v1"
}

export type ControlPlaneWebSocketProductSessionMessageAppendedEvent = {
  readonly "message": ChatMessageProjection
  readonly "productSessionId": ProductSessionId
  readonly "type": "product-session.message.appended.v1"
}

export type ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent = {
  readonly "lastProjectionSequence": number
  readonly "productSessionId": ProductSessionId
  readonly "projectionRevision": Revision
  readonly "reloadQueries": readonly [ControlPlaneWebSocketRuntimeProjectionGetReloadQuery]
  readonly "scopeKind": "product-session"
  readonly "type": "runtime-projection.invalidated.v1"
}

export type ControlPlaneWebSocketProtocolErrorFrame = {
  readonly "error": ErrorEnvelope
  readonly "type": "transport.error.v1"
}

export type ControlPlaneWebSocketResetRequiredFrame = {
  readonly "closeCode": 4409
  readonly "earliestAvailable": EventReadCursor
  readonly "reason": "cursor-expired" | "stream-rebuilt" | "authorization-boundary"
  readonly "subscriptionId": ControlPlaneWebSocketSubscriptionId
  readonly "type": "transport.reset-required.v1"
}

export type ControlPlaneWebSocketResumeAcceptedFrame = {
  readonly "after": ControlPlaneWebSocketAcknowledgedCursor
  readonly "authorizationEpoch": ControlPlaneWebSocketAuthorizationEpoch
  readonly "replayThrough": EventReadCursor
  readonly "subscriptionId": ControlPlaneWebSocketSubscriptionId
  readonly "type": "transport.resume-accepted.v1"
}

export type ControlPlaneWebSocketResumeFrame = {
  readonly "after": ControlPlaneWebSocketAcknowledgedCursor
  readonly "subscription": ControlPlaneWebSocketSubscription
  readonly "subscriptionId": ControlPlaneWebSocketSubscriptionId
  readonly "type": "transport.resume.v1"
}

export type ControlPlaneWebSocketRuntimeProjectionGetReloadQuery = "runtime.projection.get"

export type ControlPlaneWebSocketRuntimeProjectionInvalidatedEvent = ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent | ControlPlaneWebSocketWorkRunRuntimeProjectionInvalidatedEvent

export type ControlPlaneWebSocketServerFrame = ControlPlaneWebSocketEventFrame | ControlPlaneWebSocketSubscriptionAcceptedFrame | ControlPlaneWebSocketResumeAcceptedFrame | ControlPlaneWebSocketBackpressureFrame | ControlPlaneWebSocketAuthorizationRevokedFrame | ControlPlaneWebSocketResetRequiredFrame | ControlPlaneWebSocketPingFrame | ControlPlaneWebSocketProtocolErrorFrame

export type ControlPlaneWebSocketSessionExecutionWorkerSource = {
  readonly "codexThreadId": CodexThreadId
  readonly "kind": "execution-worker"
  readonly "leaseId": LeaseId
  readonly "sessionIdentity": SessionIdentity
  readonly "workerId": WorkerId
  readonly "workerSessionId": WorkerSessionId
}

export type ControlPlaneWebSocketSubscribeFrame = {
  readonly "startAt": ControlPlaneWebSocketSubscribeStartAt
  readonly "subscription": ControlPlaneWebSocketSubscription
  readonly "subscriptionId": ControlPlaneWebSocketSubscriptionId
  readonly "type": "transport.subscribe.v1"
}

export enum ControlPlaneWebSocketSubscribeOrigin {
  Latest = "latest",
  EarliestAvailable = "earliest-available",
}

export type ControlPlaneWebSocketSubscribeStartAt = ControlPlaneWebSocketSubscribeOrigin | EventReadCursor

export type ControlPlaneWebSocketSubscription = {
  readonly "eventTypes": ReadonlyArray<ControlPlaneWebSocketEventType>
  readonly "scope": Scope
  readonly "stream": EventReadStream
}

export type ControlPlaneWebSocketSubscriptionAcceptedFrame = {
  readonly "authorizationEpoch": ControlPlaneWebSocketAuthorizationEpoch
  readonly "cursor": EventReadCursor
  readonly "limits": ControlPlaneWebSocketTransportLimits
  readonly "subscriptionId": ControlPlaneWebSocketSubscriptionId
  readonly "type": "transport.subscription-accepted.v1"
}

export type ControlPlaneWebSocketSubscriptionId = string & { readonly __brand: "ControlPlaneWebSocketSubscriptionId" }

export type ControlPlaneWebSocketTransportLimits = {
  readonly "ackDeadlineMillis": 30000
  readonly "backpressureCloseCode": 4408
  readonly "hardUnackedEvents": 1024
  readonly "maxUnackedEvents": 256
}

export type ControlPlaneWebSocketWorkerHealthChangedEvent = {
  readonly "activeLeaseCount": number
  readonly "availableCapacity": number
  readonly "capabilityLabels"?: ReadonlyArray<string>
  readonly "observedAt": Instant
  readonly "status": "registering" | "healthy" | "degraded" | "draining" | "offline"
  readonly "type": "worker-health.changed.v1"
  readonly "workerId": WorkerId
}

export type ControlPlaneWebSocketWorkItemChangedEvent = {
  readonly "changeKind": "created" | "assigned" | "blocked" | "started" | "completed" | "cancelled"
  readonly "deliveryId": DeliveryId
  readonly "revision": Revision
  readonly "type": "work-item.changed.v1"
  readonly "workItemId": WorkItemId
}

export type ControlPlaneWebSocketWorkRunRuntimeProjectionInvalidatedEvent = {
  readonly "deliveryId": DeliveryId
  readonly "lastProjectionSequence": number
  readonly "productSessionId": ProductSessionId
  readonly "projectionRevision": Revision
  readonly "reloadQueries": readonly [ControlPlaneWebSocketDeliveryGetReloadQuery, ControlPlaneWebSocketRuntimeProjectionGetReloadQuery]
  readonly "scopeKind": "work-run"
  readonly "sessionIdentity": SessionIdentity
  readonly "type": "runtime-projection.invalidated.v1"
  readonly "workRunId": WorkRunId
}

export type CoreToolAgentWaitProjection = {
  readonly "edge": CoreToolWaitEdgeProjection
  readonly "state": CoreToolWaitState
}

export type CoreToolCallProjection = {
  readonly "attemptId": string | null
  readonly "cancelled": boolean
  readonly "cellId": string | null
  readonly "delivery": CoreToolOutputDelivery | null
  readonly "disposition": CoreToolOutputDisposition | null
  readonly "execution": CoreToolExecutionState | null
  readonly "inputValidation": CoreToolInputValidation | null
  readonly "logicalId": string
  readonly "parentCallId": string | null
  readonly "parentRequestSequence": ExecutionSequence | null
  readonly "requestSequence": ExecutionSequence
  readonly "toolName": string
}

export type CoreToolCallReference = {
  readonly "cellId": string | null
  readonly "logicalId": string
  readonly "parentCallId": string | null
  readonly "requestSequence": ExecutionSequence
  readonly "toolName": string
}

export enum CoreToolCellLifecycle {
  Live = "live",
  Closed = "closed",
}

export type CoreToolCellProjection = {
  readonly "cellId": string
  readonly "lifecycle": CoreToolCellLifecycle
  readonly "parentRequestSequence": ExecutionSequence
  readonly "sequence": ExecutionSequence
}

export enum CoreToolDiagnosticDelivery {
  Queued = "queued",
  Offered = "offered",
}

export enum CoreToolDiagnosticKind {
  RepeatedOperation = "repeated_operation",
  AlternatingCycle = "alternating_cycle",
  BranchExpansion = "branch_expansion",
  WaitCycle = "wait_cycle",
  UnavailableWait = "unavailable_wait",
}

export type CoreToolDiagnosticProjection = {
  readonly "delivery": CoreToolDiagnosticDelivery | null
  readonly "diagnosticId": string
  readonly "evidence": ReadonlyArray<CoreToolCallReference>
  readonly "evidenceVersion": ExecutionSequence
  readonly "kind": CoreToolDiagnosticKind | null
  readonly "question": string | null
  readonly "waitGraph"?: ReadonlyArray<CoreToolWaitEdgeProjection>
}

export enum CoreToolExecutionState {
  Running = "running",
  Completed = "completed",
  Uncertain = "uncertain",
}

export enum CoreToolFactKind {
  Request = "request",
  Cell = "cell",
  Wait = "wait",
  Reconciliation = "reconciliation",
  Diagnostic = "diagnostic",
  Progress = "progress",
  DiagnosticResponse = "diagnostic_response",
  Sharing = "sharing",
  WaiterCancellation = "waiter_cancellation",
  InputBinding = "input_binding",
  InputValidation = "input_validation",
  AgentWait = "agent_wait",
}

export enum CoreToolInputValidation {
  Verified = "verified",
  Mismatch = "mismatch",
  Unknown = "unknown",
}

export enum CoreToolOutputDelivery {
  Pending = "pending",
  Offered = "offered",
}

export enum CoreToolOutputDisposition {
  Pending = "pending",
  Accepted = "accepted",
  Rejected = "rejected",
}

export type CoreToolRecoveryProjection = {
  readonly "businessId": string | null
  readonly "exitCode": number | null
  readonly "requestSequence": ExecutionSequence
  readonly "state": CoreToolRecoveryState
}

export enum CoreToolRecoveryState {
  Unavailable = "unavailable",
  Unconfirmed = "unconfirmed",
  Running = "running",
  Exited = "exited",
}

export type CoreToolRuntimeFactPayload = {
  readonly "factJson": string
  readonly "schemaVersion": "winwincode.core-tool-fact.v1"
  readonly "sourceSequence": ExecutionSequence
  readonly "sourceThreadId": string
}

export type CoreToolRuntimeProjection = {
  readonly "agentWait"?: CoreToolAgentWaitProjection | null
  readonly "call": CoreToolCallProjection | null
  readonly "cell": CoreToolCellProjection | null
  readonly "diagnosis": CoreToolDiagnosticProjection | null
  readonly "kind": CoreToolFactKind
  readonly "recovery": CoreToolRecoveryProjection | null
  readonly "sharing": CoreToolSharingProjection | null
  readonly "sourceSequence": ExecutionSequence
  readonly "sourceThreadId": string
  readonly "wait": CoreToolWaitProjection | null
}

export enum CoreToolSharingKind {
  Reuse = "reuse",
  Merged = "merged",
}

export type CoreToolSharingProjection = {
  readonly "kind": CoreToolSharingKind
  readonly "sourceAttemptId": string
  readonly "sourceRequestSequence": ExecutionSequence
}

export type CoreToolWaitEdgeProjection = {
  readonly "deadlineUnixMs": number
  readonly "logicalId": string
  readonly "requestSequence": ExecutionSequence
  readonly "source": CoreToolWaitNodeProjection
  readonly "targetCount": number
  readonly "targets": ReadonlyArray<CoreToolWaitNodeProjection>
  readonly "treeId": string
}

export enum CoreToolWaitNodeKind {
  Thread = "thread",
  Cell = "cell",
}

export type CoreToolWaitNodeProjection = {
  readonly "cellId": string | null
  readonly "kind": CoreToolWaitNodeKind
  readonly "ownerId": string
  readonly "scopeId": string | null
  readonly "threadId": string
}

export type CoreToolWaitProjection = {
  readonly "requestSequence": ExecutionSequence
  readonly "state": CoreToolWaitState
  readonly "targetCellSequence": ExecutionSequence
}

export enum CoreToolWaitState {
  Waiting = "waiting",
  Settled = "settled",
}

export type Count = number

export type CredentialReferenceCreateCommand = {
  readonly "actor": Actor
  readonly "command": "credential.reference.create"
  readonly "expectedRevision": Revision
  readonly "payload": CredentialReferenceCreatePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CredentialReferenceCreateCompletedResponse = {
  readonly "command": "credential.reference.create"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": CredentialReferenceProjection
  readonly "schemaVersion": SchemaVersion
}

export type CredentialReferenceCreatePayload = {
  readonly "credentialReferenceId": CredentialReferenceId
  readonly "displayName": string
  readonly "providerId": string
  readonly "vaultLocator": string
}

export type CredentialReferenceDeleteCommand = {
  readonly "actor": Actor
  readonly "command": "credential.reference.delete"
  readonly "expectedRevision": Revision
  readonly "payload": CredentialReferenceDeletePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CredentialReferenceDeleteCompletedResponse = {
  readonly "command": "credential.reference.delete"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": MutationReceipt
  readonly "schemaVersion": SchemaVersion
}

export type CredentialReferenceDeletePayload = {
  readonly "credentialReferenceId": CredentialReferenceId
}

export type CredentialReferenceGetParameters = {
  readonly "credentialReferenceId": CredentialReferenceId
}

export type CredentialReferenceGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CredentialReferenceGetParameters
  readonly "query": "credential.reference.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CredentialReferenceGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "credential.reference.get"
  readonly "requestId": RequestId
  readonly "result": CredentialReferenceProjection
  readonly "schemaVersion": SchemaVersion
}

export type CredentialReferenceId = string & { readonly __brand: "CredentialReferenceId" }

export type CredentialReferenceListParameters = {
  readonly "providerId": string | null
}

export type CredentialReferenceListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": CredentialReferenceListParameters
  readonly "query": "credential.reference.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CredentialReferenceListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "credential.reference.list"
  readonly "requestId": RequestId
  readonly "result": CredentialReferencePage
  readonly "schemaVersion": SchemaVersion
}

export type CredentialReferencePage = {
  readonly "items": ReadonlyArray<CredentialReferenceProjection>
  readonly "kind": "credential_reference_page"
}

export type CredentialReferenceProjection = {
  readonly "displayName": string
  readonly "id": CredentialReferenceId
  readonly "lastRotatedAt": Instant | null
  readonly "providerId": string
  readonly "revision": Revision
  readonly "revokedAt": Instant | null
  readonly "rotationVersion": number
  readonly "secretState": "available" | "missing" | "revoked"
  readonly "updatedAt": Instant
}

export type CredentialReferenceRevokeCommand = {
  readonly "actor": Actor
  readonly "command": "credential.reference.revoke"
  readonly "expectedRevision": Revision
  readonly "payload": CredentialReferenceRevokePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CredentialReferenceRevokeCompletedResponse = {
  readonly "command": "credential.reference.revoke"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": CredentialReferenceProjection
  readonly "schemaVersion": SchemaVersion
}

export type CredentialReferenceRevokePayload = {
  readonly "credentialReferenceId": CredentialReferenceId
}

export type CredentialReferenceRotateCommand = {
  readonly "actor": Actor
  readonly "command": "credential.reference.rotate"
  readonly "expectedRevision": Revision
  readonly "payload": CredentialReferenceRotatePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type CredentialReferenceRotateCompletedResponse = {
  readonly "command": "credential.reference.rotate"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": CredentialReferenceProjection
  readonly "schemaVersion": SchemaVersion
}

export type CredentialReferenceRotatePayload = {
  readonly "credentialReferenceId": CredentialReferenceId
  readonly "vaultLocator": string
}

export type Criterion = {
  readonly "description": string
  readonly "id": CriterionId
  readonly "required": boolean
  readonly "requiredEvidenceClass": "machine"
  readonly "verificationMethod": string | null
}

export type CriterionId = string & { readonly __brand: "CriterionId" }

export type DebugConfirmedFact = {
  readonly "confirmedRoundId": ProbeRoundId
  readonly "evidence": ReadonlyArray<HypothesisEvidenceCandidate>
  readonly "factDigest": Sha256Digest
  readonly "summary": string
}

export type DebugContextSafetyProfile = {
  readonly "scannerPolicyDigest": Sha256Digest
  readonly "scannerVersion": DebugContextSafetyScannerVersion
}

export enum DebugContextSafetyScannerVersion {
  WorkspaceSecretScanV1 = "workspace_secret_scan_v1",
}

export type DebugContextSnippet = {
  readonly "content": string
  readonly "contentDigest": Sha256Digest
  readonly "endLine": number | null
  readonly "path": string | null
  readonly "safetyProfile": DebugContextSafetyProfile
  readonly "sourceArtifactRef": ArtifactReference
  readonly "sourceEvidenceDigest": Sha256Digest
  readonly "startLine": number | null
}

export type DebugDeltaContextBudget = {
  readonly "estimatorVersion": DebugDeltaContextEstimatorVersion
  readonly "maxEstimatedTokens": 8192
  readonly "maxSerializedBytes": 32768
  readonly "maxSnippetBytes": 2000
  readonly "maxSnippets": 8
  readonly "maxTotalSnippetBytes": 16000
  readonly "policyDigest": Sha256Digest
}

export enum DebugDeltaContextEstimatorVersion {
  Utf8BytesV1 = "utf8_bytes_v1",
}

export type DebugExperiment = {
  readonly "baseRevision": WorkspaceRevision
  readonly "cleanupReceipt": DebugExperimentCleanupReceipt | null
  readonly "createdAt": Instant
  readonly "experimentId": DebugExperimentId
  readonly "experimentRevision": WorkspaceRevision | null
  readonly "expiresAt": Instant
  readonly "identity": DebugProbeIdentity
  readonly "instrumentationDigest": Sha256Digest
  readonly "schemaVersion": number
  readonly "status": DebugExperimentStatus
} & ({
  readonly "baseRevision": WorkspaceRevision
  readonly "createdAt": Instant
  readonly "experimentId": string
  readonly "expiresAt": Instant
  readonly "identity": DebugProbeIdentity
  readonly "instrumentationDigest": string
  readonly "schemaVersion": number
  readonly "status": DebugExperimentStatus
})

export type DebugExperimentCleanupReceipt = {
  readonly "cleanedAt": Instant
  readonly "error": DebugProbeError | null
  readonly "experimentId": DebugExperimentId
  readonly "identity": DebugProbeIdentity
  readonly "residualResourceCount": number
  readonly "status": DebugExperimentCleanupStatus
} & ({
  readonly "cleanedAt": Instant
  readonly "experimentId": string
  readonly "identity": DebugProbeIdentity
  readonly "residualResourceCount": number
  readonly "status": DebugExperimentCleanupStatus
})

export enum DebugExperimentCleanupStatus {
  CleanedUp = "cleaned_up",
  CleanupFailed = "cleanup_failed",
}

export type DebugExperimentId = string & { readonly __brand: "DebugExperimentId" }

export enum DebugExperimentStatus {
  Planned = "planned",
  Preparing = "preparing",
  Ready = "ready",
  Running = "running",
  CleanupPending = "cleanup_pending",
  CleanedUp = "cleaned_up",
  CleanupFailed = "cleanup_failed",
  Expired = "expired",
  Cancelled = "cancelled",
}

export type DebugHypothesis = {
  readonly "confidenceBps": number
  readonly "contradictingEvidence": ReadonlyArray<DebugHypothesisEvidence>
  readonly "createdRoundId": ProbeRoundId
  readonly "hypothesisId": DebugHypothesisId
  readonly "lastUpdatedRoundId": ProbeRoundId
  readonly "status": DebugHypothesisStatus
  readonly "summary": string
  readonly "supportingEvidence": ReadonlyArray<DebugHypothesisEvidence>
}

export type DebugHypothesisChange = {
  readonly "addedContradictingEvidence": ReadonlyArray<HypothesisEvidenceCandidate>
  readonly "addedSupportingEvidence": ReadonlyArray<HypothesisEvidenceCandidate>
  readonly "confidenceBps": number
  readonly "hypothesisId": DebugHypothesisId
  readonly "previousConfidenceBps": number | null
  readonly "previousStatus": DebugHypothesisStatus | null
  readonly "status": DebugHypothesisStatus
  readonly "summary": string
}

export type DebugHypothesisEvidence = {
  readonly "artifactRef": ArtifactReference
  readonly "evidenceDigest": Sha256Digest
  readonly "identity": DebugProbeIdentity
  readonly "recordedAt": Instant
} & ({
  readonly "artifactRef": ArtifactReference
  readonly "evidenceDigest": string
  readonly "identity": DebugProbeIdentity
  readonly "recordedAt": Instant
})

export type DebugHypothesisEvidenceAssessment = {
  readonly "candidate": HypothesisEvidenceCandidate
  readonly "polarity": DebugHypothesisEvidencePolarity
}

export enum DebugHypothesisEvidencePolarity {
  Supports = "supports",
  Contradicts = "contradicts",
}

export type DebugHypothesisId = string & { readonly __brand: "DebugHypothesisId" }

export type DebugHypothesisLedger = {
  readonly "authority": DebugProbeRoundAuthority
  readonly "confirmedFacts": ReadonlyArray<DebugConfirmedFact>
  readonly "eventSequence": ExecutionSequence
  readonly "hypotheses": ReadonlyArray<DebugHypothesis>
  readonly "lastEventDigest": Sha256Digest
  readonly "latestAppliedRoundReceipt": ProbeRoundReceiptReference | null
  readonly "ledgerDigest": Sha256Digest
  readonly "reproductionRecipe": DebugReproductionRecipe | null
  readonly "schemaVersion": number
  readonly "sessionStatus": DebugSessionStatus
  readonly "unresolvedQuestions": ReadonlyArray<DebugUnresolvedQuestion>
  readonly "updatedAt": Instant
}

export type DebugHypothesisLedgerEvent = {
  readonly "authority": DebugProbeRoundAuthority
  readonly "confirmedFacts": ReadonlyArray<DebugConfirmedFact>
  readonly "eventDigest": Sha256Digest
  readonly "kind": DebugHypothesisLedgerEventKind
  readonly "mutations": ReadonlyArray<DebugHypothesisMutation>
  readonly "occurredAt": Instant
  readonly "openedQuestions": ReadonlyArray<DebugUnresolvedQuestion>
  readonly "previousEventDigest": Sha256Digest | null
  readonly "previousLedgerDigest": Sha256Digest | null
  readonly "reproductionRecipeUpdate": DebugReproductionRecipeUpdate | null
  readonly "resolvedQuestionDigests": ReadonlyArray<Sha256Digest>
  readonly "resultingLedgerDigest": Sha256Digest
  readonly "schemaVersion": number
  readonly "sequence": ExecutionSequence
  readonly "sessionStatus": DebugSessionStatus
  readonly "sourceContextDigest": Sha256Digest | null
  readonly "sourceRequestDigest": Sha256Digest | null
  readonly "sourceRoundReceipt": ProbeRoundReceiptReference | null
}

export enum DebugHypothesisLedgerEventKind {
  Initialized = "initialized",
  RoundApplied = "round_applied",
}

export type DebugHypothesisLedgerSeed = {
  readonly "authority": DebugProbeRoundAuthority
  readonly "createdAt": Instant
  readonly "hypotheses": ReadonlyArray<DebugHypothesis>
  readonly "unresolvedQuestions": ReadonlyArray<DebugUnresolvedQuestion>
}

export type DebugHypothesisLedgerUpdate = {
  readonly "confirmedFacts": ReadonlyArray<DebugConfirmedFact>
  readonly "mutations": ReadonlyArray<DebugHypothesisMutation>
  readonly "occurredAt": Instant
  readonly "openedQuestions": ReadonlyArray<DebugUnresolvedQuestion>
  readonly "previousLedgerDigest": Sha256Digest
  readonly "reproductionRecipeUpdate": DebugReproductionRecipeUpdate | null
  readonly "resolvedQuestionDigests": ReadonlyArray<Sha256Digest>
  readonly "sessionStatus": DebugSessionStatus
  readonly "sourceContextDigest": Sha256Digest
  readonly "sourceRequestDigest": Sha256Digest
  readonly "sourceRoundReceipt": ProbeRoundReceiptReference
}

export type DebugHypothesisMutation = {
  readonly "assessments": ReadonlyArray<DebugHypothesisEvidenceAssessment>
  readonly "confidenceBps": number
  readonly "hypothesisId": DebugHypothesisId
  readonly "kind": DebugHypothesisMutationKind
  readonly "previousConfidenceBps": number | null
  readonly "previousStatus": DebugHypothesisStatus | null
  readonly "status": DebugHypothesisStatus
  readonly "summary": string
}

export enum DebugHypothesisMutationKind {
  Create = "create",
  Update = "update",
}

export type DebugHypothesisRoundEvidence = {
  readonly "evidenceCandidates": ReadonlyArray<HypothesisEvidenceCandidate>
  readonly "evidenceCutDigest": Sha256Digest
  readonly "evidenceSummaries": ReadonlyArray<ProbeEvidenceSummary>
  readonly "receipt": ProbeRoundReceipt
  readonly "schemaVersion": number
  readonly "sourceRoundReceipt": ProbeRoundReceiptReference
}

export enum DebugHypothesisStatus {
  Active = "active",
  Rejected = "rejected",
  Confirmed = "confirmed",
}

export type DebugProbeDeltaContext = {
  readonly "authority": DebugProbeRoundAuthority
  readonly "budget": DebugDeltaContextBudget
  readonly "confirmedFacts": ReadonlyArray<DebugConfirmedFact>
  readonly "contextDigest": Sha256Digest
  readonly "estimatedTokenCount": number
  readonly "hypothesisChanges": ReadonlyArray<DebugHypothesisChange>
  readonly "ledgerDigest": Sha256Digest
  readonly "newEvidenceCandidates": ReadonlyArray<HypothesisEvidenceCandidate>
  readonly "newEvidenceSummaries": ReadonlyArray<ProbeEvidenceSummary>
  readonly "omittedSnippetCount": number
  readonly "previousContextDigest": Sha256Digest | null
  readonly "previousLedgerDigest": Sha256Digest | null
  readonly "questionChanges": ReadonlyArray<DebugQuestionChange>
  readonly "reproductionRecipe": DebugReproductionRecipe | null
  readonly "safetyProfile": DebugContextSafetyProfile
  readonly "schemaVersion": number
  readonly "serializedByteCount": number
  readonly "snippets": ReadonlyArray<DebugContextSnippet>
  readonly "sourceEventDigest": Sha256Digest
  readonly "sourceRequestDigest": Sha256Digest
  readonly "sourceRoundReceipt": ProbeRoundReceiptReference
}

export type DebugProbeError = {
  readonly "code": DebugProbeErrorCode
  readonly "message": string
  readonly "retryable": boolean
} & ({
  readonly "code": DebugProbeErrorCode
  readonly "message": string
})

export enum DebugProbeErrorCode {
  InvalidPlan = "invalid_plan",
  InvalidProbe = "invalid_probe",
  BudgetExceeded = "budget_exceeded",
  WorkspaceWriteForbidden = "workspace_write_forbidden",
  UndeclaredResource = "undeclared_resource",
  ResourceConflict = "resource_conflict",
  TimedOut = "timed_out",
  Cancelled = "cancelled",
  StaleAuthority = "stale_authority",
  ArtifactDigestMismatch = "artifact_digest_mismatch",
  ProcessCleanupFailed = "process_cleanup_failed",
  InfrastructureError = "infrastructure_error",
}

export type DebugProbeIdentity = {
  readonly "attempt": number
  readonly "debugSessionId": DebugSessionId
  readonly "environmentDigest": Sha256Digest
  readonly "fencingToken": FencingToken
  readonly "jobId": ExecutionJobId
  readonly "leaseId": LeaseId
  readonly "probeExecutionId": ProbeExecutionId
  readonly "probeId": ProbeId
  readonly "repositoryId": RepositoryId
  readonly "roundId": ProbeRoundId
  readonly "sessionIdentity": SessionIdentity
  readonly "workspaceRevision": WorkspaceRevision
} & ({
  readonly "attempt": number
  readonly "debugSessionId": string
  readonly "environmentDigest": string
  readonly "fencingToken": string
  readonly "jobId": string
  readonly "leaseId": string
  readonly "probeExecutionId": string
  readonly "probeId": string
  readonly "repositoryId": string
  readonly "roundId": string
  readonly "sessionIdentity": SessionIdentity
  readonly "workspaceRevision": WorkspaceRevision
})

export enum DebugProbeKind {
  Search = "search",
  Read = "read",
  StaticAnalysis = "static_analysis",
  Test = "test",
  Build = "build",
  RuntimeTrace = "runtime_trace",
  DatabaseQuery = "database_query",
  NetworkObservation = "network_observation",
}

export type DebugProbePlan = {
  readonly "authority": DebugProbeRoundAuthority
  readonly "budget": ProbeRoundBudget
  readonly "completionRule": ProbeCompletionRule
  readonly "createdAt": Instant
  readonly "planDigest": Sha256Digest
  readonly "probes": ReadonlyArray<ProbeSpec>
  readonly "schemaVersion": number
} & ({
  readonly "authority": DebugProbeRoundAuthority
  readonly "budget": ProbeRoundBudget
  readonly "completionRule": ProbeCompletionRule
  readonly "createdAt": Instant
  readonly "planDigest": string
  readonly "probes": ReadonlyArray<ProbeSpec>
  readonly "schemaVersion": number
})

export type DebugProbeRoundAuthority = {
  readonly "attempt": number
  readonly "debugSessionId": DebugSessionId
  readonly "environmentDigest": Sha256Digest
  readonly "fencingToken": FencingToken
  readonly "jobId": ExecutionJobId
  readonly "leaseId": LeaseId
  readonly "repositoryId": RepositoryId
  readonly "roundId": ProbeRoundId
  readonly "sessionIdentity": SessionIdentity
  readonly "workspaceRevision": WorkspaceRevision
} & ({
  readonly "attempt": number
  readonly "debugSessionId": string
  readonly "environmentDigest": string
  readonly "fencingToken": string
  readonly "jobId": string
  readonly "leaseId": string
  readonly "repositoryId": string
  readonly "roundId": string
  readonly "sessionIdentity": SessionIdentity
  readonly "workspaceRevision": WorkspaceRevision
})

export type DebugQuestionChange = {
  readonly "kind": DebugQuestionChangeKind
  readonly "questionDigest": Sha256Digest
  readonly "summary": string
}

export enum DebugQuestionChangeKind {
  Opened = "opened",
  Resolved = "resolved",
}

export type DebugReproductionRecipe = {
  readonly "evidence": ReadonlyArray<HypothesisEvidenceCandidate>
  readonly "lastUpdatedRoundId": ProbeRoundId
  readonly "recipeDigest": Sha256Digest
  readonly "steps": ReadonlyArray<DebugReproductionRecipeStep>
}

export type DebugReproductionRecipeStep = {
  readonly "probeDefinitionDigest": Sha256Digest | null
  readonly "stepDigest": Sha256Digest
  readonly "summary": string
}

export type DebugReproductionRecipeUpdate = {
  readonly "previousRecipeDigest": Sha256Digest | null
  readonly "recipe": DebugReproductionRecipe
}

export type DebugSessionId = string & { readonly __brand: "DebugSessionId" }

export enum DebugSessionStatus {
  Active = "active",
  RootCauseIdentified = "root_cause_identified",
  TransitionedToDelegatedBatch = "transitioned_to_delegated_batch",
  Completed = "completed",
  Cancelled = "cancelled",
  Failed = "failed",
}

export type DebugUnresolvedQuestion = {
  readonly "lastUpdatedRoundId": ProbeRoundId
  readonly "openedRoundId": ProbeRoundId
  readonly "questionDigest": Sha256Digest
  readonly "summary": string
}

export type DeliveryAcceptanceCriterionProjection = {
  readonly "description": string
  readonly "id": string
  readonly "required": boolean
  readonly "verificationMethod": string | null
}

export type DeliveryAcceptanceProgressProjection = {
  readonly "passed": Count
  readonly "total": Count
}

export type DeliveryAttentionOptionProjection = {
  readonly "description": string
  readonly "id": string
  readonly "label": string
}

export type DeliveryAttentionProjection = {
  readonly "assignedTo": ActorId | null
  readonly "blocking": boolean
  readonly "createdAt": Instant
  readonly "deliverySpecId": string
  readonly "id": AttentionItemId
  readonly "options": ReadonlyArray<DeliveryAttentionOptionProjection>
  readonly "resolutionSummary": string | null
  readonly "resolvedAt": Instant | null
  readonly "resolvedBy": ActorId | null
  readonly "status": "open" | "resolved" | "dismissed"
  readonly "title": string
  readonly "type": "requirement_question" | "decision_required" | "verification_blocked" | "scope_change" | "delivery_approval"
  readonly "workRunId": WorkRunId | null
}

export type DeliveryCreateCommand = {
  readonly "actor": Actor
  readonly "command": "delivery.create"
  readonly "expectedRevision": Revision
  readonly "payload": DeliveryCreatePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type DeliveryCreateCompletedResponse = {
  readonly "command": "delivery.create"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": DeliveryProjection
  readonly "schemaVersion": SchemaVersion
}

export type DeliveryCreatePayload = {
  readonly "deliveryId": DeliveryId
  readonly "spec": DeliverySpecInput
}

export type DeliveryCriterionResultProjection = {
  readonly "criterionId": string
  readonly "evaluatedAt": Instant
  readonly "evidenceRefs": ReadonlyArray<EvidenceId>
  readonly "explanation": string
  readonly "resultId": string
  readonly "verdict": "pass" | "fail" | "inconclusive" | "infra_error"
}

export type DeliveryDetailProjection = {
  readonly "attention": ReadonlyArray<DeliveryAttentionProjection>
  readonly "currentCandidate": FrozenCandidateSummaryProjection | null
  readonly "deliveryId": DeliveryId
  readonly "deliveryRevision": Revision
  readonly "diagramExecution": StrongFlowDiagramExecutionProjection | null
  readonly "evidence": ReadonlyArray<DeliveryEvidenceProjection>
  readonly "kind": "delivery_detail"
  readonly "ownership": DeliveryOwnershipProjection
  readonly "publication": PublicationProjection | null
  readonly "readCursor": StrongFlowReadCursor
  readonly "requirements": DeliveryRequirementsProjection
  readonly "reworkAttemptsUsed"?: Count
  readonly "schemaVersion": SchemaVersion
  readonly "solutionReview": SolutionReviewProjection | null
  readonly "status": WorkItemState
  readonly "verdict": DeliveryVerdictProjection | null
}

export type DeliveryEventReadCursor = {
  readonly "eventId": ControlPlaneEventId | null
  readonly "scope": RepositoryScope
  readonly "sequence": EventReadPosition
  readonly "stream": DeliveryEventReadStream
} & ({
  readonly "eventId": null
  readonly "sequence": 0
} | {
  readonly "eventId": ControlPlaneEventId
  readonly "sequence": number
})

export type DeliveryEventReadStream = {
  readonly "deliveryId": DeliveryId
  readonly "kind": "delivery"
}

export type DeliveryEvidenceProjection = {
  readonly "candidateId": CandidateId
  readonly "createdAt": Instant
  readonly "deliverySpecId": string
  readonly "deliverySpecRevision": Revision
  readonly "id": EvidenceId
  readonly "sessionBindingId": string
  readonly "snapshotId": SnapshotId
  readonly "sourceRef": string
  readonly "type": "test" | "command" | "diff" | "file" | "commit" | "pull_request" | "runtime_event" | "review_finding"
  readonly "workRunId": WorkRunId
}

export type DeliveryGetParameters = {
  readonly "atCursor"?: StrongFlowReadCursor
  readonly "deliveryId": DeliveryId
}

export type DeliveryGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": DeliveryGetParameters
  readonly "query": "delivery.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type DeliveryGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "delivery.get"
  readonly "requestId": RequestId
  readonly "result": DeliveryDetailProjection
  readonly "schemaVersion": SchemaVersion
}

export type DeliveryId = string & { readonly __brand: "DeliveryId" }

export type DeliveryListParameters = {
  readonly "states": ReadonlyArray<string>
}

export type DeliveryListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": DeliveryListParameters
  readonly "query": "delivery.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type DeliveryListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "delivery.list"
  readonly "requestId": RequestId
  readonly "result": DeliveryPage
  readonly "schemaVersion": SchemaVersion
}

export type DeliveryOwnershipProjection = {
  readonly "organizationId": OrganizationId
  readonly "projectId": ProjectId
  readonly "repositoryId": RepositoryId
  readonly "workspaceId": WorkspaceId
}

export type DeliveryPage = {
  readonly "items": ReadonlyArray<DeliveryProjection>
  readonly "kind": "delivery_page"
}

export type DeliveryProjection = {
  readonly "acceptance"?: DeliveryAcceptanceProgressProjection
  readonly "activeWorkRunId"?: WorkRunId | null
  readonly "deliveryId": DeliveryId
  readonly "openAttentionCount": Count
  readonly "ownership": DeliveryOwnershipProjection
  readonly "revision": Revision
  readonly "reworkAttemptsUsed"?: Count
  readonly "schemaVersion": SchemaVersion
  readonly "sourceProductSessionId"?: ProductSessionId
  readonly "status": WorkItemState
  readonly "title": string
  readonly "updatedAt": Instant
  readonly "workItemCounts": WorkItemCountsProjection
}

export type DeliveryRepositoryProjection = {
  readonly "kind": "local-git" | "github"
  readonly "locator": string
}

export type DeliveryRequirementsProjection = {
  readonly "acceptanceCriteria": ReadonlyArray<DeliveryAcceptanceCriterionProjection>
  readonly "baseRevision": string
  readonly "constraints": ReadonlyArray<string>
  readonly "deliverySpecId": string
  readonly "deliverySpecRevision": Revision
  readonly "goal": string
  readonly "maxReworkAttempts": number
  readonly "outOfScope": ReadonlyArray<string>
  readonly "publicationTarget": PublicationTarget | null
  readonly "repository": DeliveryRepositoryProjection
  readonly "scope": ReadonlyArray<string>
  readonly "sourceProductSessionId": ProductSessionId | null
  readonly "sourceRef": DeliverySourceIssueProjection | null
  readonly "title": string
}

export type DeliveryResolveAttentionCommand = {
  readonly "actor": Actor
  readonly "command": "delivery.resolve_attention"
  readonly "expectedRevision": Revision
  readonly "payload": DeliveryResolveAttentionPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type DeliveryResolveAttentionCompletedResponse = {
  readonly "command": "delivery.resolve_attention"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": DeliveryProjection
  readonly "schemaVersion": SchemaVersion
}

export type DeliveryResolveAttentionPayload = {
  readonly "attentionItemId": AttentionItemId
  readonly "decision": "resolve" | "dismiss"
  readonly "deliveryId": DeliveryId
  readonly "remediation": RemediationInput | null
  readonly "resolution": string
}

export type DeliveryReworkAuthorizationScope = {
  readonly "authorizationDigest": Sha256Digest
  readonly "candidateRef": string
  readonly "diffSha256": string
  readonly "requiresFullReverification": true
  readonly "sourceCandidateCommitId": string
  readonly "sourceCandidateTreeId": string
  readonly "targets": ReadonlyArray<DeliveryReworkTargetScope>
}

export type DeliveryReworkInput = {
  readonly "candidateRef": string
  readonly "diffSha256": string
  readonly "targets": ReadonlyArray<DeliveryReworkTargetInput>
}

export type DeliveryReworkTargetInput = {
  readonly "evidenceRefIds": ReadonlyArray<EvidenceId>
  readonly "filePath": string
  readonly "sourceHunkSha256": string
  readonly "workItemId": WorkItemId
}

export type DeliveryReworkTargetScope = {
  readonly "evidenceRefIds": ReadonlyArray<EvidenceId>
  readonly "filePath": string
  readonly "sourceHunkSha256": string
  readonly "workItemId": WorkItemId
}

export type DeliverySourceIssueProjection = {
  readonly "kind": "issue"
  readonly "number": number
  readonly "provider": "github"
  readonly "repository": string
}

export type DeliverySpecInput = {
  readonly "acceptanceCriteria": ReadonlyArray<AcceptanceCriterionInput>
  readonly "baseRevision": string
  readonly "constraints": ReadonlyArray<string>
  readonly "goal": string
  readonly "outOfScope": ReadonlyArray<string>
  readonly "publicationTarget": PublicationTarget | null
  readonly "repositoryId": RepositoryId
  readonly "scope": ReadonlyArray<string>
  readonly "sourceProductSessionId": ProductSessionId | null
  readonly "title": string
  readonly "verificationCommand"?: string
}

export type DeliverySubmitVerdictCommand = {
  readonly "actor": Actor
  readonly "command": "delivery.submit_verdict"
  readonly "expectedRevision": Revision
  readonly "payload": DeliverySubmitVerdictPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type DeliverySubmitVerdictCompletedResponse = {
  readonly "command": "delivery.submit_verdict"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": DeliveryProjection
  readonly "schemaVersion": SchemaVersion
}

export type DeliverySubmitVerdictPayload = {
  readonly "candidateDigest": Sha256Digest
  readonly "deliveryId": DeliveryId
}

export type DeliveryUpdateSpecCommand = {
  readonly "actor": Actor
  readonly "command": "delivery.update_spec"
  readonly "expectedRevision": Revision
  readonly "payload": DeliveryUpdateSpecPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type DeliveryUpdateSpecCompletedResponse = {
  readonly "command": "delivery.update_spec"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": DeliveryProjection
  readonly "schemaVersion": SchemaVersion
}

export type DeliveryUpdateSpecPayload = {
  readonly "deliveryId": DeliveryId
  readonly "spec": DeliverySpecInput
}

export type DeliveryVerdictProjection = {
  readonly "candidateId": CandidateId
  readonly "candidateRef": string
  readonly "criteria": ReadonlyArray<DeliveryCriterionResultProjection>
  readonly "deliverySpecId": string
  readonly "deliverySpecRevision": Revision
  readonly "id": string
  readonly "producedAt": Instant
  readonly "snapshotId": SnapshotId
  readonly "status": "pass" | "fail" | "inconclusive" | "infra_error"
  readonly "unresolvedFindings": ReadonlyArray<string>
}

export type DeviceConfigurationEnvelope = {
  readonly "ciphertext": string
  readonly "clientNodeId": string
  readonly "expectedRevision": number
  readonly "nonce": string
  readonly "publicKey": string
  readonly "requestId": string
}

export type DeviceExtensionMcp = {
  readonly "connectionStatus": DeviceExtensionMcpConnectionStatus
  readonly "digest": string
  readonly "enabled": boolean
  readonly "id": string
  readonly "toolNames": ReadonlyArray<string>
  readonly "transport": DeviceExtensionMcpTransport
}

export enum DeviceExtensionMcpConnectionStatus {
  Untested = "untested",
  Ready = "ready",
  Failed = "failed",
}

export enum DeviceExtensionMcpTransport {
  Stdio = "stdio",
  StreamableHttp = "streamable_http",
}

export enum DeviceExtensionOutcome {
  Saved = "saved",
  Deleted = "deleted",
  Tested = "tested",
  InvalidRequest = "invalid_request",
  RevisionConflict = "revision_conflict",
  ConnectionFailed = "connection_failed",
  Interrupted = "interrupted",
}

export type DeviceExtensionReceipt = {
  readonly "outcome": DeviceExtensionOutcome
  readonly "requestId": string
  readonly "revision": number
}

export type DeviceExtensionReport = {
  readonly "receipt": DeviceExtensionReceipt | null
  readonly "snapshot": DeviceExtensionSnapshot
}

export type DeviceExtensionSkill = {
  readonly "description": string
  readonly "digest": string
  readonly "enabled": boolean
  readonly "fileCount": number
  readonly "id": string
  readonly "name": string
  readonly "source": string
}

export type DeviceExtensionSnapshot = {
  readonly "clientNodeId": string
  readonly "encryptionPublicKey": string
  readonly "mcpServers": ReadonlyArray<DeviceExtensionMcp>
  readonly "revision": number
  readonly "skills": ReadonlyArray<DeviceExtensionSkill>
}

export type DeviceExtensionView = {
  readonly "online": boolean
  readonly "receipt": DeviceExtensionReceipt | null
  readonly "schemaVersion": "winwincode/v1"
  readonly "snapshot": DeviceExtensionSnapshot | null
}

export type DeviceModelRouteParameters = {
  readonly "productSessionId"?: ProductSessionId
}

export type DeviceProviderConfig = {
  readonly "displayName": string
  readonly "enabled": boolean
  readonly "endpoint": string
  readonly "modelIds": ReadonlyArray<string>
  readonly "protocol": DeviceProviderProtocol
  readonly "providerId": string
  readonly "responsesStructuredOutput"?: DeviceResponsesStructuredOutput
}

export enum DeviceProviderOutcome {
  Saved = "saved",
  Deleted = "deleted",
  Tested = "tested",
  InvalidRequest = "invalid_request",
  RevisionConflict = "revision_conflict",
  ProviderUnavailable = "provider_unavailable",
  Interrupted = "interrupted",
}

export type DeviceProviderProjection = {
  readonly "config": DeviceProviderConfig
  readonly "credentialConfigured": boolean
}

export enum DeviceProviderProtocol {
  AnthropicMessages = "anthropic_messages",
  OpenaiChatCompletions = "openai_chat_completions",
  OpenaiResponses = "openai_responses",
  Canonical = "canonical",
  CodexChatgpt = "codex_chatgpt",
  ChatgptPlan = "chatgpt_plan",
}

export type DeviceProviderReceipt = {
  readonly "outcome": DeviceProviderOutcome
  readonly "requestId": string
  readonly "revision": number
}

export type DeviceProviderReport = {
  readonly "receipt": DeviceProviderReceipt | null
  readonly "snapshot": DeviceProviderSnapshot
}

export type DeviceProviderSnapshot = {
  readonly "clientNodeId": string
  readonly "encryptionPublicKey": string
  readonly "providers": ReadonlyArray<DeviceProviderProjection>
  readonly "revision": number
}

export type DeviceProviderView = {
  readonly "online": boolean
  readonly "receipt": DeviceProviderReceipt | null
  readonly "schemaVersion": SchemaVersion
  readonly "snapshot": DeviceProviderSnapshot | null
}

export type DeviceRepositoryRegistrationReceipt = {
  readonly "outcome": "registered" | "invalid_request" | "invalid_git" | "permission_denied" | "moved" | "scan_failed" | "unavailable"
  readonly "repositoryBindingId": RepositoryBindingId | null
  readonly "requestId": string
}

export enum DeviceResponsesStructuredOutput {
  JsonSchema = "json_schema",
  JsonObject = "json_object",
  Text = "text",
}

export type DiagnosticBaseline = {
  readonly "diagnostics": ReadonlyArray<NormalizedDiagnostic>
  readonly "diagnosticSetDigest": Sha256Digest
  readonly "parserVersions": ReadonlyArray<DiagnosticParserVersion>
  readonly "workspaceRevision": WorkspaceRevision
}

export type DiagnosticBaselineComparison = {
  readonly "baselineDigest": Sha256Digest
  readonly "baseRevision": WorkspaceRevision
  readonly "entries": ReadonlyArray<DiagnosticComparisonEntry>
  readonly "newCount": number
  readonly "resolvedCount": number
  readonly "resultDigest": Sha256Digest
  readonly "resultRevision": WorkspaceRevision
  readonly "unchangedCount": number
}

export enum DiagnosticCategory {
  MissingSymbol = "missing_symbol",
  MissingModule = "missing_module",
  TypeMismatch = "type_mismatch",
  LintFailure = "lint_failure",
  TestFailure = "test_failure",
  Infrastructure = "infrastructure",
  Unclassified = "unclassified",
}

export enum DiagnosticChangeStatus {
  New = "new",
  Resolved = "resolved",
  Unchanged = "unchanged",
}

export type DiagnosticComparisonEntry = {
  readonly "diagnostic": NormalizedDiagnostic
  readonly "status": DiagnosticChangeStatus
}

export enum DiagnosticParserVersion {
  EslintJsonV1 = "eslint_json_v1",
  TypescriptV1 = "typescript_v1",
  CargoJsonV1 = "cargo_json_v1",
  GoTestJsonV1 = "go_test_json_v1",
  JunitXmlV1 = "junit_xml_v1",
  PytestJsonV1 = "pytest_json_v1",
}

export enum DiagnosticSeverity {
  Error = "error",
  Warning = "warning",
  Info = "info",
}

export type EmptyParameters = {}

export type EncodedPayload = {
  readonly "contentType": string
  readonly "dataBase64": string
  readonly "payloadDigest": Sha256Digest
}

export type Error = TerminalError | RetryableError

export enum ErrorCode {
  InvalidRequest = "INVALID_REQUEST",
  AuthenticationRequired = "AUTHENTICATION_REQUIRED",
  PermissionDenied = "PERMISSION_DENIED",
  ResourceNotFound = "RESOURCE_NOT_FOUND",
  IdempotencyConflict = "IDEMPOTENCY_CONFLICT",
  RevisionConflict = "REVISION_CONFLICT",
  ReadCursorExpired = "READ_CURSOR_EXPIRED",
  CandidateStale = "CANDIDATE_STALE",
  WrongState = "WRONG_STATE",
  DeviceSessionRequired = "DEVICE_SESSION_REQUIRED",
  DeviceModelUnavailable = "DEVICE_MODEL_UNAVAILABLE",
  RateLimited = "RATE_LIMITED",
  ServiceUnavailable = "SERVICE_UNAVAILABLE",
  TrustedFactsUnavailable = "TRUSTED_FACTS_UNAVAILABLE",
  InternalError = "INTERNAL_ERROR",
}

export interface ErrorDetails extends Readonly<Record<string, ErrorDetailValue>> {}

export type ErrorDetailValue = null | boolean | number | string | ReadonlyArray<ErrorDetailValue> | ErrorDetails

export type ErrorEnvelope = {
  readonly "error": Error
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
}

export type EventReadCursor = ScopeEventReadCursor | DeliveryEventReadCursor | ProductSessionEventReadCursor | LeaseEventReadCursor

export type EventReadPosition = number

export type EventReadStream = ScopeEventReadStream | DeliveryEventReadStream | ProductSessionEventReadStream | LeaseEventReadStream

export type Evidence = {
  readonly "attempt": number
  readonly "candidateId": CandidateId
  readonly "contractRevision": Revision
  readonly "createdAt": Instant
  readonly "criterionId": CriterionId
  readonly "id": EvidenceId
  readonly "outcome": "observed" | "succeeded" | "failed" | "timed_out" | "policy_denied" | "infrastructure_failed" | "cancelled"
  readonly "planRevision": Revision
  readonly "producer": "worker" | "verifier"
  readonly "producerExecutionIdentity": ExecutionFactIdentity
  readonly "schemaVersion": SchemaVersion
  readonly "sessionIdentity": SessionIdentity
  readonly "snapshotId": SnapshotId
  readonly "sourceEventId": string
  readonly "sourceSequence": number
  readonly "verificationPlanId": VerificationPlanId
  readonly "verificationSessionId": VerificationSessionId
  readonly "workContractId": WorkContractId
  readonly "workItemId": WorkItemId
  readonly "workRunId": WorkRunId
}

export type EvidenceArtifactAccessProjection = EvidenceArtifactAvailableProjection | EvidenceArtifactUnavailableProjection

export type EvidenceArtifactAvailableProjection = {
  readonly "items": ReadonlyArray<EvidenceArtifactDescriptorProjection>
  readonly "state": "available"
}

export type EvidenceArtifactContentChunkProjection = {
  readonly "artifact": EvidenceArtifactDescriptorProjection
  readonly "contentEncoding": EvidenceArtifactContentEncoding
  readonly "dataBase64": string
  readonly "encoding": "base64"
  readonly "evidence": DeliveryEvidenceProjection
  readonly "kind": "evidence_artifact_content_chunk"
  readonly "nextOffset": number | null
  readonly "offset": number
  readonly "previewMode": EvidenceArtifactPreviewMode
  readonly "readCursor": StrongFlowReadCursor
  readonly "returnedBytes": number
  readonly "state": "available"
  readonly "totalBytes": number
  readonly "truncated": boolean
}

export enum EvidenceArtifactContentEncoding {
  Utf8 = "utf-8",
  Binary = "binary",
  Unknown8bit = "unknown-8bit",
}

export type EvidenceArtifactContentGetParameters = {
  readonly "artifactDigest": Sha256Digest
  readonly "artifactId": string
  readonly "artifactKind": EvidenceArtifactKind
  readonly "artifactMediaType": string
  readonly "artifactSizeBytes": number
  readonly "evidence": EvidenceReadBinding
  readonly "length": number
  readonly "offset": number
}

export type EvidenceArtifactContentGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": EvidenceArtifactContentGetParameters
  readonly "query": "evidence.artifact.content.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type EvidenceArtifactContentGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "evidence.artifact.content.get"
  readonly "requestId": RequestId
  readonly "result": EvidenceArtifactContentResult
  readonly "schemaVersion": SchemaVersion
}

export type EvidenceArtifactContentResult = EvidenceArtifactContentChunkProjection | EvidenceArtifactContentUnavailableProjection

export type EvidenceArtifactContentUnavailableProjection = {
  readonly "artifactId": string
  readonly "evidenceId": EvidenceId
  readonly "kind": "evidence_artifact_content_unavailable"
  readonly "readCursor": StrongFlowReadCursor
  readonly "reason": "no_authoritative_link"
  readonly "state": "unavailable"
}

export type EvidenceArtifactDescriptorProjection = {
  readonly "artifactId": string
  readonly "digest": Sha256Digest
  readonly "fileName": string | null
  readonly "kind": EvidenceArtifactKind
  readonly "mediaType": string
  readonly "previewMode": EvidenceArtifactPreviewMode
  readonly "provenance": EvidenceArtifactProvenanceProjection
  readonly "sizeBytes": number
}

export enum EvidenceArtifactKind {
  CommandOutput = "command_output",
  Diff = "diff",
  Log = "log",
  Report = "report",
  TestOutput = "test_output",
}

export enum EvidenceArtifactPreviewMode {
  InlineText = "inline_text",
  DownloadOnly = "download_only",
}

export type EvidenceArtifactProvenanceProjection = {
  readonly "candidateId": CandidateId
  readonly "deliveryId": DeliveryId
  readonly "deliveryRevision": Revision
  readonly "evidenceId": EvidenceId
  readonly "sessionBindingId": string
  readonly "snapshotId": SnapshotId
  readonly "workRunId": WorkRunId
}

export type EvidenceArtifactUnavailableProjection = {
  readonly "reason": "no_authoritative_link"
  readonly "state": "unavailable"
}

export type EvidenceDetailProjection = {
  readonly "artifactAccess": EvidenceArtifactAccessProjection
  readonly "evidence": DeliveryEvidenceProjection
  readonly "kind": "evidence_detail"
  readonly "outcome": EvidenceOutcome
  readonly "pageAnnotation"?: EvidencePageAnnotationProjection | null
  readonly "readCursor": StrongFlowReadCursor
}

export type EvidenceGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": EvidenceReadBinding
  readonly "query": "evidence.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type EvidenceGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "evidence.get"
  readonly "requestId": RequestId
  readonly "result": EvidenceDetailProjection
  readonly "schemaVersion": SchemaVersion
}

export type EvidenceId = string & { readonly __brand: "EvidenceId" }

export enum EvidenceOutcome {
  Observed = "observed",
  Succeeded = "succeeded",
  Failed = "failed",
  TimedOut = "timed_out",
  PolicyDenied = "policy_denied",
  InfrastructureFailed = "infrastructure_failed",
  Cancelled = "cancelled",
}

export type EvidencePageAnnotationProjection = {
  readonly "body": string
  readonly "screenshotArtifact": CollaborationPageAnnotationArtifact
  readonly "target": CollaborationPageAnnotationTarget
}

export type EvidenceReadBinding = {
  readonly "atCursor": StrongFlowReadCursor
  readonly "candidateId": CandidateId
  readonly "deliveryId": DeliveryId
  readonly "evidenceId": EvidenceId
  readonly "readPageLimit": number
  readonly "sessionBindingId": string
  readonly "snapshotId": SnapshotId
  readonly "sourceRef": string
  readonly "type": "test" | "command" | "diff" | "file" | "commit" | "pull_request" | "runtime_event" | "review_finding"
  readonly "workRunId": WorkRunId
}

export type ExecutionAckSequence = number

export type ExecutionDeviceTarget = {
  readonly "clientId": string
  readonly "clientNodeId": string
  readonly "repositoryBindingId": string
  readonly "userId": string
}

export enum ExecutionEventCategory {
  Lifecycle = "lifecycle",
  Activity = "activity",
  Command = "command",
  Test = "test",
  Diff = "diff",
  Usage = "usage",
  Attention = "attention",
  Diagnostic = "diagnostic",
}

export type ExecutionEventId = string & { readonly __brand: "ExecutionEventId" }

export type ExecutionEventRecord = {
  readonly "category": ExecutionEventCategory
  readonly "eventId": ExecutionEventId
  readonly "occurredAt": Instant
  readonly "payload"?: EncodedPayload
  readonly "sequence": ExecutionSequence
  readonly "summary": string
}

export type ExecutionFactIdentity = {
  readonly "attempt": number
  readonly "codexThreadId"?: CodexThreadId | null
  readonly "executionJobId": ExecutionJobId
  readonly "fencingToken": string
  readonly "leaseId": LeaseId
  readonly "productSessionId"?: ProductSessionId | null
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
  readonly "workerSessionId": WorkerSessionId
}

export type ExecutionJob = {
  readonly "attachments"?: ReadonlyArray<ChatAttachment>
  readonly "attempt": number
  readonly "executionProfile": string
  readonly "goal": string
  readonly "jobId": ExecutionJobId
  readonly "limits": ExecutionLimits
  readonly "modelSelection"?: ExecutionModelSelection
  readonly "payloadDigest": Sha256Digest
  readonly "scope": ExecutionScope
  readonly "workInput"?: WorkRunInput
  readonly "workspace": ExecutionWorkspace
}

export type ExecutionJobId = string & { readonly __brand: "ExecutionJobId" }

export type ExecutionJobReplacementAuthority = {
  readonly "createdAt": Instant
  readonly "logicalJobDigest": Sha256Digest
  readonly "predecessorLease": ExecutionLeaseStamp
  readonly "predecessorSessionIdentity": SessionIdentity | null
  readonly "receiptDigest": Sha256Digest
  readonly "receiptId": RequestId
  readonly "scope": ExecutionScope
  readonly "successorLease": ExecutionLeaseStamp
}

export type ExecutionLeaseStamp = {
  readonly "attempt": number
  readonly "expiresAt": Instant
  readonly "fencingToken": FencingToken
  readonly "issuedAt": Instant
  readonly "jobId": ExecutionJobId
  readonly "leaseId": LeaseId
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
}

export type ExecutionLimits = {
  readonly "deadlineAt": Instant | null
  readonly "maxArtifactBytes": number
  readonly "maxRuntimeSeconds": number | null
}

export type ExecutionMessageId = string & { readonly __brand: "ExecutionMessageId" }

export enum ExecutionMode {
  React = "react",
  DelegatedPatchShadow = "delegated_patch_shadow",
  DelegatedPatch = "delegated_patch",
  DebugProbe = "debug_probe",
}

export type ExecutionModelSelection = {
  readonly "modelId": string
  readonly "providerId": string
}

export type ExecutionOutcome = {
  readonly "artifacts": ReadonlyArray<ArtifactReference>
  readonly "codexThreadId"?: CodexThreadId
  readonly "error"?: ExecutionPortError
  readonly "finishedAt": Instant
  readonly "lastEventSequence": ExecutionAckSequence
  readonly "status": ExecutionOutcomeStatus
  readonly "summary": string
  readonly "usage"?: ExecutionOutcomeUsage
}

export enum ExecutionOutcomeStatus {
  Cancelled = "cancelled",
  Failed = "failed",
  InfrastructureError = "infrastructure_error",
  Succeeded = "succeeded",
}

export type ExecutionOutcomeUsage = {
  readonly "accountingStatus": "known" | "unknown"
  readonly "costMicrounits": number | null
  readonly "knownTokens": number
  readonly "runtimeMillis": number
  readonly "tokens": number | null
}

export type ExecutionPortError = {
  readonly "code": ExecutionPortErrorCode
  readonly "message": string
  readonly "retryable": boolean
}

export enum ExecutionPortErrorCode {
  ProtocolVersionUnsupported = "PROTOCOL_VERSION_UNSUPPORTED",
  WorkerNotRegistered = "WORKER_NOT_REGISTERED",
  CapabilityMismatch = "CAPABILITY_MISMATCH",
  JobDispatchConflict = "JOB_DISPATCH_CONFLICT",
  LeaseExpired = "LEASE_EXPIRED",
  StaleFencingToken = "STALE_FENCING_TOKEN",
  WorkerInstanceChanged = "WORKER_INSTANCE_CHANGED",
  SequenceGap = "SEQUENCE_GAP",
  MessageConflict = "MESSAGE_CONFLICT",
  ArtifactDigestMismatch = "ARTIFACT_DIGEST_MISMATCH",
  SnapshotBindingMissing = "SNAPSHOT_BINDING_MISSING",
  SnapshotBindingMismatch = "SNAPSHOT_BINDING_MISMATCH",
  ModelStreamFailed = "MODEL_STREAM_FAILED",
  DeviceProviderRequestInvalid = "DEVICE_PROVIDER_REQUEST_INVALID",
  DeviceProviderRequestTranslationFailed = "DEVICE_PROVIDER_REQUEST_TRANSLATION_FAILED",
  DeviceProviderRequestTooLarge = "DEVICE_PROVIDER_REQUEST_TOO_LARGE",
  DeviceProviderResponseContentTypeInvalid = "DEVICE_PROVIDER_RESPONSE_CONTENT_TYPE_INVALID",
  DeviceProviderSseFramingInvalid = "DEVICE_PROVIDER_SSE_FRAMING_INVALID",
  DeviceProviderSseEventInvalid = "DEVICE_PROVIDER_SSE_EVENT_INVALID",
  DeviceProviderResponseIncomplete = "DEVICE_PROVIDER_RESPONSE_INCOMPLETE",
  DeviceProviderStreamConversionFailed = "DEVICE_PROVIDER_STREAM_CONVERSION_FAILED",
  DeviceProviderConnectionFailed = "DEVICE_PROVIDER_CONNECTION_FAILED",
  DeviceProviderUpstreamFailed = "DEVICE_PROVIDER_UPSTREAM_FAILED",
  DeviceProviderRequestRejected = "DEVICE_PROVIDER_REQUEST_REJECTED",
  DeviceProviderRateLimited = "DEVICE_PROVIDER_RATE_LIMITED",
  DeviceProviderUnavailable = "DEVICE_PROVIDER_UNAVAILABLE",
  DeviceProviderInvalidConfiguration = "DEVICE_PROVIDER_INVALID_CONFIGURATION",
  DeviceProviderAdapterProtocolFailed = "DEVICE_PROVIDER_ADAPTER_PROTOCOL_FAILED",
  DeviceProviderTransportFailed = "DEVICE_PROVIDER_TRANSPORT_FAILED",
  DeviceProviderResponseTooLarge = "DEVICE_PROVIDER_RESPONSE_TOO_LARGE",
  DeviceProviderCredentialLeakBlocked = "DEVICE_PROVIDER_CREDENTIAL_LEAK_BLOCKED",
  DeviceModelIdentityConflict = "DEVICE_MODEL_IDENTITY_CONFLICT",
  DeviceModelPaused = "DEVICE_MODEL_PAUSED",
  DeviceModelInterrupted = "DEVICE_MODEL_INTERRUPTED",
  DeviceJevUnavailable = "DEVICE_JEV_UNAVAILABLE",
  InputClosed = "INPUT_CLOSED",
  ApprovalDenied = "APPROVAL_DENIED",
  Cancelled = "CANCELLED",
  ExecutionFailed = "EXECUTION_FAILED",
  InfrastructureError = "INFRASTRUCTURE_ERROR",
  CandidateUnchanged = "CANDIDATE_UNCHANGED",
  CandidatePreparationFailed = "CANDIDATE_PREPARATION_FAILED",
  DeviceProviderStateInvalid = "DEVICE_PROVIDER_STATE_INVALID",
  DeviceProviderStorageUnavailable = "DEVICE_PROVIDER_STORAGE_UNAVAILABLE",
  DeviceProviderFilesystemUnavailable = "DEVICE_PROVIDER_FILESYSTEM_UNAVAILABLE",
  DeviceProviderEncodingInvalid = "DEVICE_PROVIDER_ENCODING_INVALID",
}

export type ExecutionPortMessage = WorkerRegisterMessage | WorkerRegistrationResultMessage | WorkerHeartbeatMessage | WorkerHeartbeatAckMessage | JobDispatchMessage | JobDispatchResultMessage | SessionBindingMessage | LeaseRenewMessage | RuntimeEventMessage | RuntimeAckMessage | RuntimeReplayRequestMessage | ArtifactOpenMessage | ArtifactChunkMessage | ArtifactAckMessage | ModelOpenMessage | ModelChunkMessage | ModelAckMessage | InputRequestMessage | InputResponseMessage | ApprovalRequestMessage | ApprovalDecisionMessage | JobCancelMessage | JobCancelAckMessage | JobOutcomeMessage | JobOutcomeAckMessage | ActionEnforcementRequestMessage | ActionEnforcementReceiptMessage | SnapshotFreezeRequestMessage | SnapshotFreezeReceiptMessage | SnapshotVerificationDispatchMessage

export type ExecutionScope = ProductSessionExecutionScope | WorkRunExecutionScope

export type ExecutionSequence = number

export type ExecutionWorkspace = {
  readonly "checkoutRevision": string
  readonly "repositoryId": RepositoryId
  readonly "writeMode": "read-only" | "candidate"
}

export type ExternalIdentityId = string & { readonly __brand: "ExternalIdentityId" }

export type FencingToken = string

export type FinalCandidateFreezeFact = {
  readonly "candidateArtifactRef": ArtifactReference
  readonly "contextPackDigest": Sha256Digest
  readonly "counters": RepairLoopCounters
  readonly "deltaDigest": Sha256Digest
  readonly "finalObservation": ObservationReceipt | null
  readonly "finalReceipt": ChangeBatchReceipt
  readonly "frozenAt": Instant
  readonly "identity": ChangeBatchIdentity
  readonly "resultRevision": WorkspaceRevision
  readonly "schemaVersion": number
  readonly "stopReason": "accepted"
}

export type FrozenCandidateSummaryProjection = {
  readonly "candidateCommitId": string
  readonly "candidateDigest": Sha256Digest
  readonly "candidateId": CandidateId
  readonly "candidateRef": string
  readonly "candidateTreeId": string
  readonly "deliverySpecId": string
  readonly "deliverySpecRevision": Revision
  readonly "diffSha256": Sha256Digest
  readonly "frozenAt": Instant
  readonly "producerSessionBindingId": string
  readonly "producerWorkRunId": WorkRunId
}

export type GitCommitSha = string

export type GitHubRepositorySlug = string

export type GitObjectId = string & { readonly __brand: "GitObjectId" }

export type GitRefName = string

export type HypothesisEvidenceCandidate = {
  readonly "evidence": DebugHypothesisEvidence
  readonly "targetHypothesisId": DebugHypothesisId
} & ({
  readonly "evidence": DebugHypothesisEvidence
  readonly "targetHypothesisId": DebugHypothesisId
})

export type IdempotencyKey = string

export type InputRequestId = string & { readonly __brand: "InputRequestId" }

export type InputRequestMessage = {
  readonly "allowEmpty": boolean
  readonly "allowOther"?: boolean
  readonly "choices"?: ReadonlyArray<InteractiveInputChoice>
  readonly "expiresAt": Instant
  readonly "inputRequestId": InputRequestId
  readonly "kind": "input.request"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "mode": InteractiveInputMode
  readonly "prompt": string
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "workerSessionId": WorkerSessionId
}

export type InputRespondCommand = {
  readonly "actor": Actor
  readonly "command": "input.respond"
  readonly "expectedRevision": Revision
  readonly "payload": InputRespondPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type InputRespondCompletedResponse = {
  readonly "command": "input.respond"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": ProductSessionProjection
  readonly "schemaVersion": SchemaVersion
}

export type InputRespondPayload = {
  readonly "executionJobId": ExecutionJobId
  readonly "inputRequestId": InputRequestId
  readonly "productSessionId": ProductSessionId
  readonly "sessionIdentity": SessionIdentity
  readonly "status": "cancelled" | "expired" | "provided"
  readonly "value": InteractiveInputValue | null
  readonly "workerSessionId": WorkerSessionId
}

export type InputResponseMessage = {
  readonly "inputRequestId": InputRequestId
  readonly "kind": "input.response"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "respondedAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "status": "cancelled" | "expired" | "provided"
  readonly "value": InteractiveInputValue | null
  readonly "workerSessionId": WorkerSessionId
}

export type Instant = string

export type IntegrationId = string & { readonly __brand: "IntegrationId" }

export type InteractiveInputChoice = {
  readonly "id": InteractiveInputChoiceId
  readonly "label": string
  readonly "value": string
}

export type InteractiveInputChoiceId = string & { readonly __brand: "InteractiveInputChoiceId" }

export enum InteractiveInputMode {
  Confirmation = "confirmation",
  SingleChoice = "single_choice",
  Text = "text",
}

export type InteractiveInputValue = {
  readonly "mode": InteractiveInputMode
  readonly "value": string
}

export type JobCancelAckMessage = {
  readonly "error"?: ExecutionPortError
  readonly "kind": "job.cancel_ack"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "status": "accepted" | "already_cancelling" | "already_terminal" | "rejected_expired_lease" | "rejected_stale_fencing_token" | "rejected_worker_instance"
  readonly "workerSessionId": WorkerSessionId
}

export type JobCancelMessage = {
  readonly "kind": "job.cancel"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "reason": "deadline" | "policy" | "shutdown" | "superseded" | "user_requested"
  readonly "requestedAt": Instant
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "workerSessionId": WorkerSessionId
}

export type JobDispatchMessage = {
  readonly "job": ExecutionJob
  readonly "kind": "job.dispatch"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "replacementAuthority": ExecutionJobReplacementAuthority | null
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "snapshotId"?: SnapshotId
}

export type JobDispatchResultMessage = {
  readonly "error"?: ExecutionPortError
  readonly "jobId": ExecutionJobId
  readonly "kind": "job.dispatch_result"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "payloadDigest": Sha256Digest
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "status": "accepted" | "duplicate" | "conflict" | "rejected_capacity" | "rejected_capability" | "rejected_expired_lease" | "rejected_stale_fencing_token" | "rejected_worker_instance"
  readonly "workerSessionId"?: WorkerSessionId
}

export type JobOutcomeAckMessage = {
  readonly "error"?: ExecutionPortError
  readonly "kind": "job.outcome_ack"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "status": "accepted" | "duplicate" | "rejected_conflict" | "rejected_expired_lease" | "rejected_stale_fencing_token" | "rejected_worker_instance"
  readonly "workerSessionId": WorkerSessionId
}

export type JobOutcomeMessage = {
  readonly "kind": "job.outcome"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "outcome": ExecutionOutcome
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "workerSessionId": WorkerSessionId
}

export type LeaseEventReadCursor = {
  readonly "eventId": ControlPlaneEventId | null
  readonly "scope": Scope
  readonly "sequence": EventReadPosition
  readonly "stream": LeaseEventReadStream
} & ({
  readonly "eventId": null
  readonly "sequence": 0
} | {
  readonly "eventId": ControlPlaneEventId
  readonly "sequence": number
})

export type LeaseEventReadStream = {
  readonly "kind": "lease"
  readonly "leaseId": LeaseId
  readonly "workerId": WorkerId
}

export type LeaseId = string & { readonly __brand: "LeaseId" }

export type LeaseRenewMessage = {
  readonly "kind": "lease.renew"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "priorExpiresAt": Instant
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
}

export enum LeaseWriteStatus {
  Accepted = "accepted",
  Duplicate = "duplicate",
  Gap = "gap",
  RejectedConflict = "rejected_conflict",
  RejectedExpiredLease = "rejected_expired_lease",
  RejectedStaleFencingToken = "rejected_stale_fencing_token",
  RejectedWorkerInstance = "rejected_worker_instance",
}

export type LocalApplyReceipt = {
  readonly "candidateRef": CandidateRef
  readonly "conflictArtifactRef": string | null
  readonly "createdAt": Instant
  readonly "expectedHead": GitCommitSha
  readonly "localApplyReceiptId": LocalApplyReceiptId
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "result": LocalApplyResult
  readonly "resultingCommit": GitCommitSha | null
  readonly "revision": Revision
  readonly "strategy": LocalApplyStrategy
  readonly "targetBranch": GitRefName
}

export type LocalApplyReceiptId = string & { readonly __brand: "LocalApplyReceiptId" }

export enum LocalApplyResult {
  Retained = "retained",
  BranchCreated = "branch_created",
  Applied = "applied",
  BaseStale = "base_stale",
  WorkingTreeDirty = "working_tree_dirty",
  MergeConflict = "merge_conflict",
  CandidateMissing = "candidate_missing",
  PermissionDenied = "permission_denied",
  Discarded = "discarded",
  Failed = "failed",
}

export enum LocalApplyStrategy {
  CreateBranch = "create_branch",
  FastForward = "fast_forward",
  CherryPick = "cherry_pick",
  Merge = "merge",
}

export type LocalCandidateReceipt = {
  readonly "candidateCommit": GitCommitSha
  readonly "candidateRef": CandidateRef
  readonly "createdAt": Instant
  readonly "localCandidateReceiptId": LocalCandidateReceiptId
  readonly "localRefName": GitRefName
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "revision": Revision
  readonly "state": LocalCandidateReceiptState
}

export type LocalCandidateReceiptId = string & { readonly __brand: "LocalCandidateReceiptId" }

export enum LocalCandidateReceiptState {
  Retained = "retained",
  BranchCreated = "branch_created",
  Applied = "applied",
  Discarded = "discarded",
  Failed = "failed",
}

export type LocalDefaultScope = {
  readonly "kind": "repository"
  readonly "organizationId": "org_00000000000000000000000000"
  readonly "projectId": "prj_00000000000000000000000000"
  readonly "repositoryId": "rep_00000000000000000000000000"
  readonly "workspaceId": "wsp_00000000000000000000000000"
}

export type ManagedAppCommand = {
  readonly "config": ManagedAppRunConfig | null
  readonly "idempotencyKey": IdempotencyKey
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "operation": ManagedAppOperation
  readonly "runId": WorkRunId
  readonly "schemaVersion": "winwincode/managed-app-run-v1"
}

export type ManagedAppHealthCheck = {
  readonly "path": string
  readonly "timeoutMs": number
}

export enum ManagedAppMode {
  Live = "live",
  FrozenCandidate = "frozen-candidate",
}

export enum ManagedAppOperation {
  Start = "start",
  Stop = "stop",
  Restart = "restart",
  Query = "query",
}

export type ManagedAppRunConfig = {
  readonly "argv": ReadonlyArray<string>
  readonly "attempt": number
  readonly "candidateCommit": string | null
  readonly "cwd": string
  readonly "env": Readonly<Record<string, string>> & {}
  readonly "healthCheck": ManagedAppHealthCheck
  readonly "listenPort": number
  readonly "mode": ManagedAppMode
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "runId": WorkRunId
  readonly "schemaVersion": "winwincode/managed-app-run-v1"
  readonly "sourceId": ExecutionJobId
  readonly "templateRevision": number
}

export type ManagedAppRunControlProjection = {
  readonly "attempt": number
  readonly "candidateCommit": string | null
  readonly "mode": ManagedAppMode
  readonly "runId": WorkRunId
  readonly "schemaVersion": "winwincode/managed-app-run-v1"
}

export enum ManagedAppState {
  Starting = "starting",
  Healthy = "healthy",
  Unhealthy = "unhealthy",
  Stopped = "stopped",
  Exited = "exited",
  Missing = "missing",
}

export type ManagedAppStatus = {
  readonly "exitCode": number | null
  readonly "pid": number | null
  readonly "processStartIdentity": string | null
  readonly "runId": WorkRunId
  readonly "schemaVersion": "winwincode/managed-app-run-v1"
  readonly "sourceId": ExecutionJobId
  readonly "state": ManagedAppState
}

export type ModelAckMessage = {
  readonly "ackSequence": ExecutionAckSequence
  readonly "error"?: ExecutionPortError
  readonly "kind": "model.ack"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "modelExchangeId": ModelExchangeId
  readonly "replayFromSequence"?: ExecutionSequence
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "status": LeaseWriteStatus
  readonly "workerSessionId": WorkerSessionId
}

export type ModelChunkMessage = {
  readonly "error"?: ExecutionPortError
  readonly "isFinal": boolean
  readonly "kind": "model.chunk"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "modelExchangeId": ModelExchangeId
  readonly "payload"?: EncodedPayload
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sequence": ExecutionSequence
  readonly "sessionIdentity": SessionIdentity
  readonly "workerSessionId": WorkerSessionId
}

export type ModelExchangeId = string & { readonly __brand: "ModelExchangeId" }

export enum ModelFailureAcceptance {
  NotSent = "not_sent",
  Unknown = "unknown",
  ResponseReceived = "response_received",
}

export enum ModelFailureCause {
  ConnectionUnavailable = "connection_unavailable",
  TransportInterrupted = "transport_interrupted",
  Timeout = "timeout",
  RateLimited = "rate_limited",
  ServerTransient = "server_transient",
  Authentication = "authentication",
  Authorization = "authorization",
  RequestInvalid = "request_invalid",
  ProtocolInvalid = "protocol_invalid",
  IntegrityInvalid = "integrity_invalid",
  TlsInvalid = "tls_invalid",
  StreamIncomplete = "stream_incomplete",
  EmptyResponse = "empty_response",
  Cancelled = "cancelled",
  AuthorityExpired = "authority_expired",
  StorageUnavailable = "storage_unavailable",
}

export type ModelFailureFacts = {
  readonly "acceptance"?: ModelFailureAcceptance
  readonly "attemptId"?: string
  readonly "cause"?: ModelFailureCause
  readonly "networkDiagnosticCode"?: ModelNetworkDiagnosticCode
  readonly "parserDiagnostic"?: ProviderModelFailureDiagnostic
  readonly "phase"?: ModelFailurePhase
  readonly "scope": "model_attempt"
  readonly "sourceCode": ExecutionPortErrorCode
  readonly "stopReason"?: ModelFailureStopReason
}

export enum ModelFailurePhase {
  Connect = "connect",
  ResponseHeaders = "response_headers",
  ResponseBody = "response_body",
  Stream = "stream",
  Decode = "decode",
  Persist = "persist",
}

export enum ModelFailureStopReason {
  PermanentFailure = "permanent_failure",
  RecoveryDisabled = "recovery_disabled",
  ReconcileRequired = "reconcile_required",
  RetryBudgetExhausted = "retry_budget_exhausted",
}

export type ModelGatewayRoute = {
  readonly "capability": string
  readonly "route": string
}

export enum ModelNetworkDiagnosticCode {
  HttpStatus = "http_status",
  Dns = "dns",
  Connect = "connect",
  Io = "io",
  Timeout = "timeout",
  Tls = "tls",
  TlsCertificate = "tls_certificate",
  HttpProtocol = "http_protocol",
  RequestUri = "request_uri",
  RequestHeaders = "request_headers",
  Proxy = "proxy",
  Redirect = "redirect",
  ResponseHeadersTooLarge = "response_headers_too_large",
  BodyTooLarge = "body_too_large",
  ContentType = "content_type",
  TransportOther = "transport_other",
  JsonSyntax = "json_syntax",
  JsonSchema = "json_schema",
  ResponseInvariant = "response_invariant",
  SchemaVersion = "schema_version",
  SseFraming = "sse_framing",
  SseEvent = "sse_event",
  StreamConversion = "stream_conversion",
  StreamIncomplete = "stream_incomplete",
  EmptyResponse = "empty_response",
  IdentityConflict = "identity_conflict",
  CredentialBlocked = "credential_blocked",
  Storage = "storage",
  Configuration = "configuration",
  AuthorityEnded = "authority_ended",
}

export type ModelOpenMessage = {
  readonly "kind": "model.open"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "modelExchangeId": ModelExchangeId
  readonly "request": EncodedPayload
  readonly "requestId": RequestId
  readonly "route": ModelGatewayRoute
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "workerSessionId": WorkerSessionId
}

export type ModelRoute = {
  readonly "credentialReferenceId": CredentialReferenceId
  readonly "modelId": string
  readonly "providerId": string
}

export type ModelRouteAvailabilityListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": DeviceModelRouteParameters
  readonly "query": "model.route.availability.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type ModelRouteAvailabilityListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "model.route.availability.list"
  readonly "requestId": RequestId
  readonly "result": ModelRouteAvailabilityPage
  readonly "schemaVersion": SchemaVersion
}

export type ModelRouteAvailabilityPage = {
  readonly "defaultModelId": string | null
  readonly "defaultProviderId": string | null
  readonly "items": ReadonlyArray<ModelRouteAvailabilityProjection>
  readonly "kind": "model_route_availability_page"
  readonly "reason": ModelRouteAvailabilityReason
  readonly "requestPoolRevision": Revision
  readonly "requestPoolSource": ProjectScope
  readonly "scope": RepositoryScope
  readonly "settingsRevision": Revision | null
  readonly "settingsSource": Scope | null
  readonly "status": ModelRouteAvailabilityStatus
}

export type ModelRouteAvailabilityProjection = {
  readonly "catalogSource": Scope
  readonly "catalogVersion": Revision
  readonly "clientId"?: string
  readonly "contextWindowTokens": number
  readonly "credentialRotationVersion": number | null
  readonly "isDefault": boolean
  readonly "maxOutputTokens": number
  readonly "modelDisplayName": string
  readonly "modelVersion": Revision
  readonly "providerDisplayName": string
  readonly "providerVersion": Revision
  readonly "reason": ModelRouteAvailabilityReason
  readonly "reasoningEfforts": ReadonlyArray<string>
  readonly "route": ModelRoute
  readonly "status": ModelRouteAvailabilityStatus
  readonly "toolSupport": ModelRouteToolSupport
}

export enum ModelRouteAvailabilityReason {
  Ready = "ready",
  RateLimited = "rate_limited",
  WindowExhausted = "window_exhausted",
  WeeklyExhausted = "weekly_exhausted",
  AuthenticationError = "authentication_error",
  NoProvider = "no_provider",
  CredentialMissingOrRevoked = "credential_missing_or_revoked",
  DefaultRouteInvalid = "default_route_invalid",
  ProviderOrModelDisabled = "provider_or_model_disabled",
  RequestPoolUnavailable = "request_pool_unavailable",
  RuntimeStatusUnknown = "runtime_status_unknown",
}

export enum ModelRouteAvailabilityStatus {
  Available = "available",
  RateLimited = "rate_limited",
  WindowExhausted = "window_exhausted",
  WeeklyExhausted = "weekly_exhausted",
  AuthError = "auth_error",
  Disabled = "disabled",
  Unknown = "unknown",
}

export enum ModelRouteToolSupport {
  Unsupported = "unsupported",
  Serial = "serial",
  Parallel = "parallel",
}

export type MutationReceipt = {
  readonly "resourceKind": "product_session" | "delivery" | "settings" | "credential_reference" | "approval" | "worker" | "publication"
  readonly "revision": Revision
}

export type NormalizedDiagnostic = {
  readonly "category": DiagnosticCategory
  readonly "code": string
  readonly "column": number | null
  readonly "diagnosticId": Sha256Digest
  readonly "display": string
  readonly "line": number | null
  readonly "messageDigest": Sha256Digest
  readonly "parserVersion": DiagnosticParserVersion
  readonly "path": string
  readonly "severity": DiagnosticSeverity
}

export type NormalizerReceipt = {
  readonly "artifactRefs": ReadonlyArray<ArtifactReference>
  readonly "baseRevision": WorkspaceRevision
  readonly "changedFileDigests": ReadonlyArray<Sha256Digest>
  readonly "resultRevision"?: WorkspaceRevision
  readonly "status": NormalizerReceiptStatus
} & ({
  readonly "changedFileDigests": ReadonlyArray<unknown>
  readonly "resultRevision": WorkspaceRevision
  readonly "status": NormalizerReceiptStatus.Normalized
} | {
  readonly "changedFileDigests": ReadonlyArray<unknown>
  readonly "resultRevision": WorkspaceRevision
  readonly "status": NormalizerReceiptStatus.Unchanged
} | {
  readonly "changedFileDigests": ReadonlyArray<unknown>
  readonly "resultRevision"?: WorkspaceRevision
  readonly "status": NormalizerReceiptStatus.Rejected
} | {
  readonly "changedFileDigests": ReadonlyArray<unknown>
  readonly "resultRevision"?: WorkspaceRevision
  readonly "status": NormalizerReceiptStatus.InfrastructureError
} | {
  readonly "changedFileDigests": ReadonlyArray<unknown>
  readonly "resultRevision"?: WorkspaceRevision
  readonly "status": NormalizerReceiptStatus.Cancelled
})

export enum NormalizerReceiptStatus {
  Normalized = "normalized",
  Unchanged = "unchanged",
  Rejected = "rejected",
  InfrastructureError = "infrastructure_error",
  Cancelled = "cancelled",
}

export type ObservationAcceptanceCriterion = {
  readonly "id": string
  readonly "summary": string
}

export type ObservationDataEgressPolicy = {
  readonly "externalArtifactReadsAllowed": false
  readonly "networkAllowed": false
  readonly "providerFileUploadsAllowed": false
}

export enum ObservationDecision {
  Accept = "accept",
  RepairRequired = "repair_required",
  SemanticRisk = "semantic_risk",
  InfrastructureError = "infrastructure_error",
  Inconclusive = "inconclusive",
}

export type ObservationDeltaSummary = {
  readonly "deltaDigest": Sha256Digest
  readonly "deltaExact": true
  readonly "fileCount": number
  readonly "hunkCount": number
  readonly "summary": string
}

export type ObservationFailedTestSummary = {
  readonly "diagnosticDigest": Sha256Digest | null
  readonly "name": string
  readonly "summary": string
}

export type ObservationId = string & { readonly __brand: "ObservationId" }

export type ObservationIntent = {
  readonly "allChecksExecuted": true
  readonly "dataEgress": ObservationDataEgressPolicy
  readonly "deltaDigest": Sha256Digest
  readonly "deltaExact": true
  readonly "hardCheckFailed": false
  readonly "identity": ChangeBatchIdentity
  readonly "inputDigest": Sha256Digest
  readonly "observationId": ObservationId
  readonly "profileDigest": Sha256Digest
  readonly "promptInjectionScan": ObservationPromptInjectionScan
  readonly "resultRevision": WorkspaceRevision
  readonly "secretScan": ObservationSecretScan
  readonly "untrustedInput": ObservationUntrustedInput
  readonly "validationProfile": ValidationProfileName
}

export type ObservationPromptInjectionScan = {
  readonly "findingCount": number
  readonly "inputDigest": Sha256Digest
  readonly "rulesDigest": Sha256Digest
  readonly "scannerVersion": string
  readonly "status": ObservationPromptInjectionStatus
}

export enum ObservationPromptInjectionStatus {
  Clean = "clean",
  Suspected = "suspected",
}

export enum ObservationReasonCode {
  CriteriaSatisfied = "criteria_satisfied",
  TargetedRepairRequired = "targeted_repair_required",
  SemanticRiskDetected = "semantic_risk_detected",
  ObserverInfrastructureError = "observer_infrastructure_error",
  InsufficientEvidence = "insufficient_evidence",
}

export type ObservationReceipt = {
  readonly "identity": ChangeBatchIdentity
  readonly "inputDigest": Sha256Digest
  readonly "modelUsage": ExecutionOutcomeUsage | null
  readonly "outputDigest": Sha256Digest
  readonly "profileDigest": Sha256Digest
  readonly "response": ObservationResponse
  readonly "resultRevision": WorkspaceRevision
  readonly "source": ObservationSource
}

export type ObservationRequest = {
  readonly "intent": ObservationIntent
  readonly "oneShot": true
  readonly "schemaVersion": number
}

export type ObservationResponse = {
  readonly "confidenceBps": number
  readonly "decision": ObservationDecision
  readonly "observationId": ObservationId
  readonly "reasonCode": ObservationReasonCode
  readonly "repairClass": RepairClass | null
  readonly "rootCauses": ReadonlyArray<string>
  readonly "schemaVersion": number
  readonly "summary": string
}

export type ObservationSecretScan = {
  readonly "findingCount": number
  readonly "inputDigest": Sha256Digest
  readonly "outputDigest": Sha256Digest
  readonly "scannerVersion": string
  readonly "status": ObservationSecretScanStatus
}

export enum ObservationSecretScanStatus {
  Clean = "clean",
  Redacted = "redacted",
  Rejected = "rejected",
}

export type ObservationSnippet = {
  readonly "content": string
  readonly "contentDigest": Sha256Digest
  readonly "endLine": number
  readonly "path": string
  readonly "startLine": number
}

export enum ObservationSource {
  Model = "model",
  ObserverRuntime = "observer_runtime",
}

export type ObservationUntrustedInput = {
  readonly "acceptanceCriteria": ReadonlyArray<ObservationAcceptanceCriterion>
  readonly "batchSummary": string
  readonly "contentDigest": Sha256Digest
  readonly "delta": ObservationDeltaSummary
  readonly "failedTests": ReadonlyArray<ObservationFailedTestSummary>
  readonly "goalSummary": string
  readonly "newDiagnostics": ReadonlyArray<NormalizedDiagnostic>
  readonly "snippets": ReadonlyArray<ObservationSnippet>
  readonly "trustLevel": "untrusted"
}

export type OccupancyClaimId = string & { readonly __brand: "OccupancyClaimId" }

export type OccupancyFencingToken = string

export enum OccupancyRejectReason {
  UnknownLease = "unknown_lease",
  StaleFencingToken = "stale_fencing_token",
  LocalStateConflict = "local_state_conflict",
  ClientLocked = "client_locked",
  CapacityExhausted = "capacity_exhausted",
}

export type OpaqueCursor = string

export type OrganizationId = string & { readonly __brand: "OrganizationId" }

export type OrganizationScope = {
  readonly "kind": "organization"
  readonly "organizationId": OrganizationId
}

export type PageInfo = {
  readonly "hasMore": boolean
  readonly "nextCursor": OpaqueCursor | null
}

export type PageRequest = {
  readonly "cursor": OpaqueCursor | null
  readonly "limit": number
}

export type ProbeBaselineEvidence = {
  readonly "baselineBundleArtifactRef": ArtifactReference | null
  readonly "baselineBundleDigest": Sha256Digest | null
  readonly "baselineEnvironmentDigest": Sha256Digest | null
  readonly "baselineProbeDefinitionDigest": Sha256Digest | null
  readonly "baselineProfileDigest": Sha256Digest | null
  readonly "comparison": DiagnosticBaselineComparison | null
  readonly "state": ProbeBaselineState
} & ({
  readonly "state": ProbeBaselineState
})

export type ProbeBaselineSelection = {
  readonly "priorBundleArtifactRef": ArtifactReference | null
  readonly "priorBundleDigest": Sha256Digest | null
  readonly "priorEnvironmentDigest": Sha256Digest | null
  readonly "priorProbeDefinitionDigest": Sha256Digest | null
  readonly "priorProfileDigest": Sha256Digest | null
  readonly "state": ProbeBaselineSelectionState
} & ({
  readonly "state": ProbeBaselineSelectionState
})

export enum ProbeBaselineSelectionState {
  NotApplicable = "not_applicable",
  Unavailable = "unavailable",
  PriorBundle = "prior_bundle",
}

export enum ProbeBaselineState {
  NotApplicable = "not_applicable",
  Available = "available",
  Unavailable = "unavailable",
  Incompatible = "incompatible",
  ResultIncomplete = "result_incomplete",
}

export type ProbeCommandSpec = {
  readonly "argv": ReadonlyArray<string>
  readonly "commandArgBytes": number
  readonly "workingDirectory": string
} & ({
  readonly "argv": ReadonlyArray<string>
  readonly "commandArgBytes": number
  readonly "workingDirectory": string
})

export type ProbeCompletionRule = {
  readonly "kind": ProbeCompletionRuleKind
  readonly "minimumCompletedProbes": number
  readonly "minimumSuccessfulProbes": number
  readonly "stopOnRequiredProbeFailure": boolean
} & ({
  readonly "kind": ProbeCompletionRuleKind
  readonly "minimumCompletedProbes": number
  readonly "minimumSuccessfulProbes": number
})

export enum ProbeCompletionRuleKind {
  AllTerminal = "all_terminal",
  MinimumSuccesses = "minimum_successes",
  FirstConclusive = "first_conclusive",
}

export type ProbeDiagnosticOccurrence = {
  readonly "diagnostic": NormalizedDiagnostic
  readonly "occurrenceCount": number
} & ({
  readonly "diagnostic": NormalizedDiagnostic
  readonly "occurrenceCount": number
})

export type ProbeEvidenceBundle = {
  readonly "baseline": ProbeBaselineEvidence
  readonly "bundleDigest": Sha256Digest
  readonly "completeness": ProbeEvidenceCompleteness
  readonly "diagnostics": ReadonlyArray<ProbeDiagnosticOccurrence>
  readonly "failedTests": ReadonlyArray<ProbeFailedTest>
  readonly "identity": DebugProbeIdentity
  readonly "l0Receipt": ProbeExecutionReceipt
  readonly "normalizedAt": Instant
  readonly "planDigest": Sha256Digest
  readonly "probeDefinitionDigest": Sha256Digest
  readonly "profile": ProbeNormalizerProfile
  readonly "rawStreams": ReadonlyArray<ProbeRawStreamBinding>
  readonly "schemaVersion": number
  readonly "stackClusters": ReadonlyArray<ProbeStackCluster>
  readonly "targetHypothesisIds": ReadonlyArray<DebugHypothesisId>
  readonly "workspaceDelta": ProbeWorkspaceDeltaEvidence
} & ({
  readonly "baseline": ProbeBaselineEvidence
  readonly "bundleDigest": Sha256Digest
  readonly "completeness": ProbeEvidenceCompleteness
  readonly "diagnostics": ReadonlyArray<ProbeDiagnosticOccurrence>
  readonly "failedTests": ReadonlyArray<ProbeFailedTest>
  readonly "identity": DebugProbeIdentity
  readonly "l0Receipt": ProbeExecutionReceipt
  readonly "normalizedAt": Instant
  readonly "planDigest": Sha256Digest
  readonly "probeDefinitionDigest": Sha256Digest
  readonly "profile": ProbeNormalizerProfile
  readonly "rawStreams": ReadonlyArray<ProbeRawStreamBinding>
  readonly "schemaVersion": number
  readonly "stackClusters": ReadonlyArray<ProbeStackCluster>
  readonly "targetHypothesisIds": ReadonlyArray<DebugHypothesisId>
  readonly "workspaceDelta": ProbeWorkspaceDeltaEvidence
})

export type ProbeEvidenceCompleteness = {
  readonly "reasons": ReadonlyArray<ProbeEvidenceIncompleteReason>
  readonly "status": ProbeEvidenceCompletenessStatus
} & ({
  readonly "reasons": ReadonlyArray<ProbeEvidenceIncompleteReason>
  readonly "status": ProbeEvidenceCompletenessStatus
})

export enum ProbeEvidenceCompletenessStatus {
  Complete = "complete",
  Incomplete = "incomplete",
}

export enum ProbeEvidenceIncompleteReason {
  OutputTruncated = "output_truncated",
  InvalidUtf8 = "invalid_utf8",
  DiagnosticInputTooLarge = "diagnostic_input_too_large",
  InvalidPayload = "invalid_payload",
  TooManyDiagnostics = "too_many_diagnostics",
  InvalidPath = "invalid_path",
  TooManyStackFrames = "too_many_stack_frames",
  TooManyStackClusters = "too_many_stack_clusters",
  BaselineUnavailable = "baseline_unavailable",
  BaselineIncompatible = "baseline_incompatible",
}

export type ProbeEvidenceSummary = {
  readonly "bundleArtifactRef": ArtifactReference
  readonly "bundleDigest": Sha256Digest
  readonly "changedFileCount": number
  readonly "completeness": ProbeEvidenceCompleteness
  readonly "diagnosticOccurrenceCount": number
  readonly "failedTestCount": number
  readonly "identity": DebugProbeIdentity
  readonly "recordedAt": Instant
  readonly "stackClusterCount": number
  readonly "summary": string
  readonly "uniqueDiagnosticCount": number
} & ({
  readonly "bundleArtifactRef": ArtifactReference
  readonly "bundleDigest": Sha256Digest
  readonly "changedFileCount": number
  readonly "completeness": ProbeEvidenceCompleteness
  readonly "diagnosticOccurrenceCount": number
  readonly "failedTestCount": number
  readonly "identity": DebugProbeIdentity
  readonly "recordedAt": Instant
  readonly "stackClusterCount": number
  readonly "summary": string
  readonly "uniqueDiagnosticCount": number
})

export type ProbeExecutionEvent = {
  readonly "artifactRefs": ReadonlyArray<ArtifactReference>
  readonly "identity": DebugProbeIdentity
  readonly "kind": ProbeExecutionEventKind
  readonly "occurredAt": Instant
  readonly "sequence": ExecutionSequence
  readonly "status": ProbeExecutionStatus
  readonly "summary": string
} & ({
  readonly "artifactRefs": ReadonlyArray<ArtifactReference>
  readonly "identity": DebugProbeIdentity
  readonly "kind": ProbeExecutionEventKind
  readonly "occurredAt": Instant
  readonly "sequence": number
  readonly "status": ProbeExecutionStatus
  readonly "summary": string
})

export enum ProbeExecutionEventKind {
  Scheduled = "scheduled",
  Started = "started",
  Finished = "finished",
}

export type ProbeExecutionId = string & { readonly __brand: "ProbeExecutionId" }

export type ProbeExecutionIntent = {
  readonly "createdAt": Instant
  readonly "identity": DebugProbeIdentity
  readonly "planDigest": Sha256Digest
  readonly "schemaVersion": number
  readonly "spec": ProbeSpec
} & ({
  readonly "createdAt": Instant
  readonly "identity": DebugProbeIdentity
  readonly "planDigest": string
  readonly "schemaVersion": number
  readonly "spec": ProbeSpec
})

export type ProbeExecutionReceipt = {
  readonly "artifactRefs": ReadonlyArray<ArtifactReference>
  readonly "durationMillis": number
  readonly "error": DebugProbeError | null
  readonly "exitCode": number | null
  readonly "finishedAt": Instant
  readonly "identity": DebugProbeIdentity
  readonly "outputBytes": number
  readonly "outputTruncated": boolean
  readonly "planDigest": Sha256Digest
  readonly "schemaVersion": number
  readonly "signal": string | null
  readonly "startedAt": Instant
  readonly "status": ProbeReceiptStatus
  readonly "timedOut": boolean
} & ({
  readonly "artifactRefs": ReadonlyArray<ArtifactReference>
  readonly "durationMillis": number
  readonly "finishedAt": Instant
  readonly "identity": DebugProbeIdentity
  readonly "outputBytes": number
  readonly "planDigest": string
  readonly "schemaVersion": number
  readonly "startedAt": Instant
  readonly "status": ProbeReceiptStatus
})

export enum ProbeExecutionStatus {
  Scheduled = "scheduled",
  Running = "running",
  Succeeded = "succeeded",
  Failed = "failed",
  TimedOut = "timed_out",
  Cancelled = "cancelled",
  Skipped = "skipped",
  CacheHit = "cache_hit",
  Stale = "stale",
}

export type ProbeFailedTest = {
  readonly "diagnostic": NormalizedDiagnostic
  readonly "occurrenceCount": number
} & ({
  readonly "diagnostic": NormalizedDiagnostic
  readonly "occurrenceCount": number
})

export type ProbeId = string & { readonly __brand: "ProbeId" }

export enum ProbeNetworkAccess {
  None = "none",
  Loopback = "loopback",
  DeclaredReadOnly = "declared_read_only",
}

export type ProbeNormalizerProfile = {
  readonly "diagnosticParserVersion": DiagnosticParserVersion | null
  readonly "normalizerVersion": ProbeNormalizerVersion
  readonly "profileDigest": Sha256Digest
  readonly "stackParserVersion": ProbeStackParserVersion | null
} & ({
  readonly "normalizerVersion": ProbeNormalizerVersion
  readonly "profileDigest": Sha256Digest
})

export enum ProbeNormalizerVersion {
  L0L1V1 = "l0_l1_v1",
}

export enum ProbeRawStream {
  Stdout = "stdout",
  Stderr = "stderr",
}

export type ProbeRawStreamBinding = {
  readonly "artifactRef": ArtifactReference
  readonly "encoding": ProbeRawStreamEncoding
  readonly "retainedBytes": number
  readonly "stream": ProbeRawStream
} & ({
  readonly "artifactRef": ArtifactReference
  readonly "encoding": ProbeRawStreamEncoding
  readonly "retainedBytes": number
  readonly "stream": ProbeRawStream
})

export enum ProbeRawStreamEncoding {
  Utf8 = "utf8",
  InvalidUtf8 = "invalid_utf8",
}

export enum ProbeReceiptStatus {
  Succeeded = "succeeded",
  Failed = "failed",
  TimedOut = "timed_out",
  Cancelled = "cancelled",
  Skipped = "skipped",
  CacheHit = "cache_hit",
  Stale = "stale",
}

export enum ProbeReducerReasonCode {
  L0Sufficient = "l0_sufficient",
  L1Sufficient = "l1_sufficient",
  ProviderCompleted = "provider_completed",
  ProviderRateLimited = "provider_rate_limited",
  ProviderTimeout = "provider_timeout",
  ProviderInfrastructure = "provider_infrastructure",
  InvalidJson = "invalid_json",
  UnknownField = "unknown_field",
  PromptInjection = "prompt_injection",
  OutputBudgetExceeded = "output_budget_exceeded",
}

export enum ProbeReducerStatus {
  Completed = "completed",
  Inconclusive = "inconclusive",
}

export type ProbeResourceClaim = {
  readonly "cpuLimitMillis": number
  readonly "databaseKeys": ReadonlyArray<string>
  readonly "exclusiveKeys": ReadonlyArray<string>
  readonly "memoryLimitBytes": number
  readonly "networkAccess": ProbeNetworkAccess
  readonly "paths": ReadonlyArray<string>
  readonly "portNumbers": ReadonlyArray<number>
  readonly "serviceKeys": ReadonlyArray<string>
  readonly "sideEffectClass": ProbeSideEffectClass
  readonly "workspaceAccess": ProbeWorkspaceAccess
} & ({
  readonly "cpuLimitMillis": number
  readonly "databaseKeys": ReadonlyArray<string>
  readonly "exclusiveKeys": ReadonlyArray<string>
  readonly "memoryLimitBytes": number
  readonly "networkAccess": ProbeNetworkAccess
  readonly "paths": ReadonlyArray<string>
  readonly "portNumbers": ReadonlyArray<number>
  readonly "serviceKeys": ReadonlyArray<string>
  readonly "sideEffectClass": ProbeSideEffectClass
  readonly "workspaceAccess": ProbeWorkspaceAccess
})

export type ProbeRoundBudget = {
  readonly "budgetDigest": Sha256Digest
  readonly "parallelProbeLimit": number
  readonly "peakMemoryLimitBytes": number
  readonly "probeLimit": number
  readonly "totalCommandArgLimitBytes": number
  readonly "totalCpuLimitMillis": number
  readonly "totalOutputLimitBytes": number
  readonly "wallTimeLimitMillis": number
} & ({
  readonly "budgetDigest": string
  readonly "parallelProbeLimit": number
  readonly "peakMemoryLimitBytes": number
  readonly "probeLimit": number
  readonly "totalCommandArgLimitBytes": number
  readonly "totalCpuLimitMillis": number
  readonly "totalOutputLimitBytes": number
  readonly "wallTimeLimitMillis": number
})

export type ProbeRoundBudgetUsage = {
  readonly "budgetDigest": Sha256Digest
  readonly "elapsedMillis": number
  readonly "peakMemoryBytes": number
  readonly "peakParallelProbes": number
  readonly "probeCount": number
  readonly "totalCommandArgBytes": number
  readonly "totalCpuMillis": number
  readonly "totalOutputBytes": number
} & ({
  readonly "budgetDigest": string
  readonly "elapsedMillis": number
  readonly "peakMemoryBytes": number
  readonly "peakParallelProbes": number
  readonly "probeCount": number
  readonly "totalCommandArgBytes": number
  readonly "totalCpuMillis": number
  readonly "totalOutputBytes": number
})

export enum ProbeRoundCompletionReason {
  CompletionRuleSatisfied = "completion_rule_satisfied",
  AllProbesTerminal = "all_probes_terminal",
  BudgetExhausted = "budget_exhausted",
  Cancelled = "cancelled",
  StaleAuthority = "stale_authority",
  InfrastructureError = "infrastructure_error",
}

export type ProbeRoundEvent = {
  readonly "authority": DebugProbeRoundAuthority
  readonly "kind": ProbeRoundEventKind
  readonly "occurredAt": Instant
  readonly "sequence": ExecutionSequence
  readonly "status": ProbeRoundStatus
  readonly "summary": string
} & ({
  readonly "authority": DebugProbeRoundAuthority
  readonly "kind": ProbeRoundEventKind
  readonly "occurredAt": Instant
  readonly "sequence": number
  readonly "status": ProbeRoundStatus
  readonly "summary": string
})

export enum ProbeRoundEventKind {
  Planned = "planned",
  Started = "started",
  Finished = "finished",
}

export type ProbeRoundId = string & { readonly __brand: "ProbeRoundId" }

export type ProbeRoundReceipt = {
  readonly "authority": DebugProbeRoundAuthority
  readonly "completionReason": ProbeRoundCompletionReason
  readonly "error": DebugProbeError | null
  readonly "finishedAt": Instant
  readonly "planDigest": Sha256Digest
  readonly "probeReceipts": ReadonlyArray<ProbeExecutionReceipt>
  readonly "reducer"?: ProbeRoundReducerSupplement | null
  readonly "schemaVersion": number
  readonly "startedAt": Instant
  readonly "status": ProbeRoundReceiptStatus
  readonly "usage": ProbeRoundBudgetUsage
} & ({
  readonly "authority": DebugProbeRoundAuthority
  readonly "completionReason": ProbeRoundCompletionReason
  readonly "finishedAt": Instant
  readonly "planDigest": string
  readonly "probeReceipts": ReadonlyArray<ProbeExecutionReceipt>
  readonly "schemaVersion": number
  readonly "startedAt": Instant
  readonly "status": ProbeRoundReceiptStatus
  readonly "usage": ProbeRoundBudgetUsage
})

export type ProbeRoundReceiptReference = {
  readonly "authority": DebugProbeRoundAuthority
  readonly "planDigest": Sha256Digest
  readonly "receiptArtifactRef": ArtifactReference
  readonly "receiptDigest": Sha256Digest
}

export enum ProbeRoundReceiptStatus {
  Completed = "completed",
  Failed = "failed",
  Cancelled = "cancelled",
  Stale = "stale",
}

export type ProbeRoundReducerSupplement = {
  readonly "contradictingHypothesisIds": ReadonlyArray<DebugHypothesisId>
  readonly "evidenceArtifactRefs": ReadonlyArray<ArtifactReference>
  readonly "inputDigest": Sha256Digest
  readonly "operationId": string
  readonly "outputDigest": Sha256Digest
  readonly "providerCalls": number
  readonly "reasonCode": ProbeReducerReasonCode
  readonly "rootCauses": ReadonlyArray<string>
  readonly "schemaVersion": number
  readonly "status": ProbeReducerStatus
  readonly "summary": string
  readonly "supportingHypothesisIds": ReadonlyArray<DebugHypothesisId>
  readonly "usage": ExecutionOutcomeUsage | null
}

export enum ProbeRoundStatus {
  Planned = "planned",
  Running = "running",
  Completed = "completed",
  Failed = "failed",
  Cancelled = "cancelled",
  Stale = "stale",
}

export enum ProbeSideEffectClass {
  PureRead = "pure_read",
  IsolatedSideEffect = "isolated_side_effect",
  Exclusive = "exclusive",
}

export type ProbeSpec = {
  readonly "command": ProbeCommandSpec
  readonly "kind": DebugProbeKind
  readonly "outputLimitBytes": number
  readonly "probeDefinitionDigest": Sha256Digest
  readonly "probeId": ProbeId
  readonly "required": boolean
  readonly "resources": ProbeResourceClaim
  readonly "targetHypothesisIds": ReadonlyArray<DebugHypothesisId>
  readonly "timeoutMillis": number
} & ({
  readonly "command": ProbeCommandSpec
  readonly "kind": DebugProbeKind
  readonly "outputLimitBytes": number
  readonly "probeDefinitionDigest": string
  readonly "probeId": string
  readonly "resources": ProbeResourceClaim
  readonly "targetHypothesisIds": ReadonlyArray<DebugHypothesisId>
  readonly "timeoutMillis": number
})

export type ProbeStackCluster = {
  readonly "frames": ReadonlyArray<ProbeStackFrame>
  readonly "occurrenceCount": number
  readonly "sourceStreams": ReadonlyArray<ProbeRawStream>
  readonly "stackDigest": Sha256Digest
  readonly "summary": string
} & ({
  readonly "frames": ReadonlyArray<ProbeStackFrame>
  readonly "occurrenceCount": number
  readonly "sourceStreams": ReadonlyArray<ProbeRawStream>
  readonly "stackDigest": Sha256Digest
  readonly "summary": string
})

export type ProbeStackFrame = {
  readonly "column": number | null
  readonly "function": string | null
  readonly "line": number | null
  readonly "path": string | null
}

export enum ProbeStackParserVersion {
  RustV1 = "rust_v1",
  NodeV1 = "node_v1",
  PythonV1 = "python_v1",
}

export enum ProbeWorkspaceAccess {
  ReadOnly = "read_only",
}

export type ProbeWorkspaceDeltaEvidence = {
  readonly "changedFiles": ReadonlyArray<AppliedFileSummary>
  readonly "state": ProbeWorkspaceDeltaState
} & ({
  readonly "changedFiles": ReadonlyArray<AppliedFileSummary>
  readonly "state": ProbeWorkspaceDeltaState
})

export enum ProbeWorkspaceDeltaState {
  NotApplicable = "not_applicable",
  Available = "available",
}

export type ProductSessionEventReadCursor = {
  readonly "eventId": ControlPlaneEventId | null
  readonly "scope": RepositoryScope
  readonly "sequence": EventReadPosition
  readonly "stream": ProductSessionEventReadStream
} & ({
  readonly "eventId": null
  readonly "sequence": 0
} | {
  readonly "eventId": ControlPlaneEventId
  readonly "sequence": number
})

export type ProductSessionEventReadStream = {
  readonly "kind": "product-session"
  readonly "productSessionId": ProductSessionId
}

export type ProductSessionExecutionScope = {
  readonly "kind": "product-session"
  readonly "productSessionId": ProductSessionId
}

export type ProductSessionId = string & { readonly __brand: "ProductSessionId" }

export type ProductSessionPage = {
  readonly "items": ReadonlyArray<ProductSessionProjection>
  readonly "kind": "product_session_page"
}

export type ProductSessionProjection = {
  readonly "archived"?: boolean
  readonly "deviceContext"?: {
  readonly "branch": string
  readonly "clientId": string
  readonly "deviceName": string
  readonly "headCommit": string
  readonly "repositoryBindingId": string
  readonly "repositoryName": string
}
  readonly "id": ProductSessionId
  readonly "modelRoute"?: ModelRoute
  readonly "projectId": ProjectId
  readonly "repositoryId": RepositoryId
  readonly "revision": Revision
  readonly "state": "idle" | "running" | "waiting_for_input" | "waiting_for_approval" | "cancelled" | "closed" | "failed"
  readonly "title": string
  readonly "updatedAt": Instant
}

export type ProductSessionRuntimeProjectionGetParameters = {
  readonly "kind": "product-session"
  readonly "productSessionId": ProductSessionId
}

export type ProjectId = string & { readonly __brand: "ProjectId" }

export type ProjectScope = {
  readonly "kind": "project"
  readonly "organizationId": OrganizationId
  readonly "projectId": ProjectId
  readonly "workspaceId": WorkspaceId
}

export type ProviderModelFailureDiagnostic = {
  readonly "eventType"?: string
  readonly "fieldPath"?: string
  readonly "stage"?: string
}

export type PublicationCancelCommand = {
  readonly "actor": Actor
  readonly "command": "publication.cancel"
  readonly "expectedRevision": Revision
  readonly "payload": PublicationCancelPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type PublicationCancelCompletedResponse = {
  readonly "command": "publication.cancel"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": PublicationProjection
  readonly "schemaVersion": SchemaVersion
}

export type PublicationCancellationProjection = {
  readonly "cancelledAt": Instant
  readonly "reason": string
  readonly "revision": Revision
}

export type PublicationCancelPayload = {
  readonly "publicationId": PublicationId
  readonly "reason": string
}

export type PublicationDetailProjection = {
  readonly "cancellable": boolean
  readonly "cancellation": PublicationCancellationProjection | null
  readonly "history": ReadonlyArray<PublicationStatusHistoryProjection>
  readonly "historyTruncated": boolean
  readonly "kind": "publication_detail"
  readonly "retryable": boolean
  readonly "steps": ReadonlyArray<PublicationStepProjection>
  readonly "summary": PublicationProjection
}

export type PublicationGetParameters = {
  readonly "publicationId": PublicationId
}

export type PublicationGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": PublicationGetParameters
  readonly "query": "publication.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type PublicationGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "publication.get"
  readonly "requestId": RequestId
  readonly "result": PublicationDetailProjection
  readonly "schemaVersion": SchemaVersion
}

export type PublicationId = string & { readonly __brand: "PublicationId" }

export type PublicationListParameters = {
  readonly "deliveryId": DeliveryId | null
  readonly "states": ReadonlyArray<"pending" | "publishing" | "published" | "cancelled" | "failed">
}

export type PublicationListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": PublicationListParameters
  readonly "query": "publication.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type PublicationListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "publication.list"
  readonly "requestId": RequestId
  readonly "result": PublicationPage
  readonly "schemaVersion": SchemaVersion
}

export type PublicationPage = {
  readonly "items": ReadonlyArray<PublicationProjection>
  readonly "kind": "publication_page"
}

export type PublicationProjection = {
  readonly "approvalAttentionItemId": AttentionItemId
  readonly "approvedAt": Instant
  readonly "approvedBy": ActorId
  readonly "candidateRef": string
  readonly "deliveryId": DeliveryId
  readonly "deliverySpecId": string
  readonly "deliverySpecRevision": Revision
  readonly "deliveryVerdictId": string
  readonly "id": PublicationId
  readonly "publicationSetSha256": Sha256Digest
  readonly "resourceRef": PublicationResourceRef | null
  readonly "revision": Revision
  readonly "state": "pending" | "publishing" | "published" | "cancelled" | "failed"
  readonly "target": PublicationTarget
  readonly "updatedAt": Instant
  readonly "verdictStatus": "pass"
}

export type PublicationPublishCommand = {
  readonly "actor": Actor
  readonly "command": "publication.publish"
  readonly "expectedRevision": Revision
  readonly "payload": PublicationPublishPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type PublicationPublishCompletedResponse = {
  readonly "command": "publication.publish"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": PublicationProjection
  readonly "schemaVersion": SchemaVersion
}

export type PublicationPublishPayload = {
  readonly "candidateDigest": Sha256Digest
  readonly "deliveryId": DeliveryId
  readonly "publicationId": PublicationId
  readonly "target": PublicationTarget
}

export enum PublicationResourceKind {
  GithubIssue = "github_issue",
  GithubPullRequest = "github_pull_request",
}

export type PublicationResourceRef = {
  readonly "kind": PublicationResourceKind
  readonly "number": number
  readonly "repository": GitHubRepositorySlug
}

export type PublicationStatusHistoryProjection = {
  readonly "cancellable": boolean
  readonly "retryable": boolean
  readonly "revision": Revision
  readonly "state": "pending" | "publishing" | "published" | "cancelled" | "failed"
  readonly "stepStates": ReadonlyArray<PublicationStepStateProjection>
  readonly "updatedAt": Instant
}

export type PublicationStepProjection = {
  readonly "kind": "branch" | "pull_request" | "issue_comment" | "commit_status"
  readonly "outcomeCode": string | null
  readonly "remoteWritePerformed": boolean | null
  readonly "resourceRef": PublicationResourceRef | null
  readonly "retryable": boolean
  readonly "state": "pending" | "applying" | "unknown" | "succeeded" | "rejected"
}

export type PublicationStepStateProjection = {
  readonly "kind": "branch" | "pull_request" | "issue_comment" | "commit_status"
  readonly "state": "pending" | "applying" | "unknown" | "succeeded" | "rejected"
}

export type PublicationTarget = {
  readonly "baseBranch": string
  readonly "headBranch": string
  readonly "headRepository": GitHubRepositorySlug
  readonly "provider": "github"
  readonly "repository": GitHubRepositorySlug
}

export type PublicClientId = string & { readonly __brand: "PublicClientId" }

export type QueryEnvelope = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": {}
  readonly "query": QueryName
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export enum QueryName {
  SessionList = "session.list",
  SessionGet = "session.get",
  SessionMessagesList = "session.messages.list",
  SessionInteractionsList = "session.interactions.list",
  RuntimeProjectionGet = "runtime.projection.get",
  WorkRunGet = "workrun.get",
  DeliveryList = "delivery.list",
  DeliveryGet = "delivery.get",
  CandidateList = "candidate.list",
  CandidateReviewGet = "candidate.review.get",
  CandidateFilesList = "candidate.files.list",
  CandidateDiffGet = "candidate.diff.get",
  CandidateFileContentGet = "candidate.file.content.get",
  EvidenceGet = "evidence.get",
  EvidenceArtifactContentGet = "evidence.artifact.content.get",
  SettingsGet = "settings.get",
  ModelRouteAvailabilityList = "model.route.availability.list",
  CredentialReferenceList = "credential.reference.list",
  CredentialReferenceGet = "credential.reference.get",
  ApprovalList = "approval.list",
  ApprovalGet = "approval.get",
  WorkerList = "worker.list",
  WorkerGet = "worker.get",
  PublicationList = "publication.list",
  PublicationGet = "publication.get",
  CollaborationActivityList = "collaboration.activity.list",
  CollaborationNotificationList = "collaboration.notification.list",
  CollaborationPresenceList = "collaboration.presence.list",
  CollaborationPageAnnotationList = "collaboration.page_annotation.list",
  SessionArtifactGet = "session.artifact.get",
}

export type QueryRequest = SessionListQuery | SessionGetQuery | SessionMessagesListQuery | ChatInteractionListQuery | RuntimeProjectionGetQuery | WorkRunGetQuery | DeliveryListQuery | DeliveryGetQuery | CandidateHistoryListQuery | CandidateHistoricalReviewGetQuery | CandidateFilesListQuery | CandidateDiffGetQuery | CandidateFileContentGetQuery | EvidenceGetQuery | EvidenceArtifactContentGetQuery | SettingsGetQuery | ModelRouteAvailabilityListQuery | CredentialReferenceListQuery | CredentialReferenceGetQuery | ApprovalListQuery | ApprovalGetQuery | WorkerListQuery | WorkerGetQuery | PublicationListQuery | PublicationGetQuery | CollaborationActivityListQuery | CollaborationNotificationListQuery | CollaborationPresenceListQuery | CollaborationPageAnnotationListQuery | SessionArtifactGetQuery

export type QueryResultEnvelope = {
  readonly "page": PageInfo
  readonly "query": QueryName
  readonly "requestId": RequestId
  readonly "result": unknown
  readonly "schemaVersion": SchemaVersion
}

export type QueryResultResponse = SessionListResultResponse | SessionGetResultResponse | SessionMessagesListResultResponse | ChatInteractionListResultResponse | RuntimeProjectionGetResultResponse | WorkRunGetResultResponse | DeliveryListResultResponse | DeliveryGetResultResponse | CandidateHistoryListResultResponse | CandidateHistoricalReviewGetResultResponse | CandidateFilesListResultResponse | CandidateDiffGetResultResponse | CandidateFileContentGetResultResponse | EvidenceGetResultResponse | EvidenceArtifactContentGetResultResponse | SettingsGetResultResponse | ModelRouteAvailabilityListResultResponse | CredentialReferenceListResultResponse | CredentialReferenceGetResultResponse | ApprovalListResultResponse | ApprovalGetResultResponse | WorkerListResultResponse | WorkerGetResultResponse | PublicationListResultResponse | PublicationGetResultResponse | CollaborationActivityListResultResponse | CollaborationNotificationListResultResponse | CollaborationPresenceListResultResponse | CollaborationPageAnnotationListResultResponse | SessionArtifactGetResultResponse

export enum ReadOnlyRoleWorkspaceMode {
  SourceReadOnly = "source-read-only",
  CandidateReadOnly = "candidate-read-only",
}

export type RemediationInput = {
  readonly "candidateDigest": Sha256Digest
  readonly "instructions": string
  readonly "workItemId": WorkItemId | null
}

export enum RepairClass {
  TargetedPatch = "targeted_patch",
  RegenerateBatch = "regenerate_batch",
  Replan = "replan",
  HumanReview = "human_review",
}

export type RepairEnvelope = {
  readonly "deltaDigest": Sha256Digest
  readonly "diagnosticDigests": ReadonlyArray<Sha256Digest>
  readonly "identity": ChangeBatchIdentity
  readonly "observedRevision": WorkspaceRevision
  readonly "previousReceiptRef"?: ArtifactReference
  readonly "reasonCode": string
  readonly "repairRound": number
  readonly "rootCauseSummary": string
  readonly "snippetArtifactRefs": ReadonlyArray<ArtifactReference>
} & ({
  readonly "diagnosticDigests": ReadonlyArray<unknown>
  readonly "observedRevision": WorkspaceRevision
  readonly "reasonCode": string
  readonly "repairRound": number
  readonly "rootCauseSummary": string
  readonly "snippetArtifactRefs": ReadonlyArray<unknown>
})

export type RepairLoopBudget = {
  readonly "maxChangeBatches": number
  readonly "maxContextPackBytes": number
  readonly "maxObserverCalls": number | null
  readonly "maxPrimaryModelCalls": number | null
  readonly "maxRepairRounds": 3
  readonly "maxTotalCostMicrounits": number | null
  readonly "maxTotalTokens": number | null
  readonly "maxWallTimeMillis": number | null
}

export type RepairLoopContextPack = {
  readonly "artifactRefs": ReadonlyArray<ArtifactReference>
  readonly "completedAcceptanceCriteria": ReadonlyArray<ObservationAcceptanceCriterion>
  readonly "contextDigest": Sha256Digest
  readonly "goalSummary": string
  readonly "identity": ChangeBatchIdentity
  readonly "incompleteAcceptanceCriteria": ReadonlyArray<ObservationAcceptanceCriterion>
  readonly "latestObservation": ObservationReceipt | null
  readonly "latestReceipt": ChangeBatchReceipt
  readonly "observedRevision": WorkspaceRevision
  readonly "proposalDisposition": ChangeBatchProposalDisposition
  readonly "repairEnvelope": RepairEnvelope | null
  readonly "schemaVersion": number
  readonly "serializedByteCount": number
}

export type RepairLoopCounters = {
  readonly "changeBatches": number
  readonly "contextPackBytes": number
  readonly "elapsedMillis": number
  readonly "observerCalls": number
  readonly "primaryModelCalls": number
  readonly "repairRounds": number
  readonly "totalCostMicrounits": number
  readonly "totalTokens": number
}

export enum RepairLoopStopReason {
  Accepted = "accepted",
  RepairRoundLimitReached = "repair_round_limit_reached",
  ObserverCallLimitReached = "observer_call_limit_reached",
  PrimaryModelCallLimitReached = "primary_model_call_limit_reached",
  TotalTokenLimitReached = "total_token_limit_reached",
  TotalCostLimitReached = "total_cost_limit_reached",
  WallTimeLimitReached = "wall_time_limit_reached",
  ChangeBatchLimitReached = "change_batch_limit_reached",
  ContextPackLimitReached = "context_pack_limit_reached",
  SemanticRisk = "semantic_risk",
  InfrastructureError = "infrastructure_error",
  StateUncertain = "state_uncertain",
  HumanReviewRequired = "human_review_required",
}

export type RepositoryAccessGrant = {
  readonly "createdAt": Instant
  readonly "grantedByUserId": UserId
  readonly "permissions": ReadonlyArray<ClientAccessGrantPermission>
  readonly "repositoryAccessGrantId": RepositoryAccessGrantId
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "revision": Revision
  readonly "state": "active" | "revoked"
  readonly "userId": UserId
}

export type RepositoryAccessGrantId = string & { readonly __brand: "RepositoryAccessGrantId" }

export enum RepositoryAvailability {
  Available = "available",
  Dirty = "dirty",
  Unavailable = "unavailable",
  Moved = "moved",
  InvalidGit = "invalid_git",
  PermissionDenied = "permission_denied",
  ScanFailed = "scan_failed",
}

export type RepositoryBinding = {
  readonly "availability": RepositoryAvailability
  readonly "clientNodeId": ClientNodeId
  readonly "defaultBranch": GitRefName
  readonly "dirtyState": RepositoryDirtyState
  readonly "displayName": ClientDisplayName
  readonly "headCommit": GitCommitSha
  readonly "lastScannedAt": Instant | null
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "repositoryFingerprint": Sha256Digest
  readonly "repositoryKind": RepositoryKind
  readonly "revision": Revision
}

export type RepositoryBindingId = string & { readonly __brand: "RepositoryBindingId" }

export type RepositoryBindingProjection = {
  readonly "availability": RepositoryAvailability
  readonly "defaultBranch": GitRefName
  readonly "dirtyState": RepositoryDirtyState
  readonly "displayName": ClientDisplayName
  readonly "headCommit": GitCommitSha
  readonly "lastScannedAt": Instant
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "repositoryFingerprint": Sha256Digest
  readonly "repositoryKind": RepositoryKind
}

export enum RepositoryDirtyState {
  Clean = "clean",
  Dirty = "dirty",
}

export type RepositoryId = string & { readonly __brand: "RepositoryId" }

export enum RepositoryKind {
  Git = "git",
}

export type RepositoryScope = {
  readonly "kind": "repository"
  readonly "organizationId": OrganizationId
  readonly "projectId": ProjectId
  readonly "repositoryId": RepositoryId
  readonly "workspaceId": WorkspaceId
}

export type RequestId = string & { readonly __brand: "RequestId" }

export type RetryableError = {
  readonly "code": RetryableErrorCode
  readonly "details": ErrorDetails
  readonly "message": string
  readonly "retryable": true
}

export enum RetryableErrorCode {
  RateLimited = "RATE_LIMITED",
  ReadCursorExpired = "READ_CURSOR_EXPIRED",
  ServiceUnavailable = "SERVICE_UNAVAILABLE",
  TrustedFactsUnavailable = "TRUSTED_FACTS_UNAVAILABLE",
}

export type Revision = number

export enum RoleExecutionMode {
  React = "react",
  DelegatedBatch = "delegated_batch",
  DebugProbe = "debug_probe",
}

export type RoleSessionPolicy = {
  readonly "developerInstructions": string
  readonly "executionMode": RoleExecutionMode
  readonly "roleId": "requirements" | "solution" | "planner" | "executor" | "reviewer" | "verifier" | "adversarial-verifier" | "remediator"
  readonly "schemaVersion": number
  readonly "workspaceMode": "source-read-only" | "candidate-read-only" | "candidate-write"
} & ({
  readonly "developerInstructions": string
  readonly "executionMode": RoleExecutionMode.React
  readonly "schemaVersion": 2
} | {
  readonly "developerInstructions": string
  readonly "executionMode": RoleExecutionMode.DelegatedBatch
  readonly "schemaVersion": 2
  readonly "workspaceMode": ReadOnlyRoleWorkspaceMode
} | {
  readonly "developerInstructions": string
  readonly "executionMode": RoleExecutionMode.DebugProbe
  readonly "schemaVersion": 2
  readonly "workspaceMode": ReadOnlyRoleWorkspaceMode
})

export type RuntimeAckMessage = {
  readonly "ackSequence": ExecutionAckSequence
  readonly "error"?: ExecutionPortError
  readonly "kind": "runtime.ack"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "replayFromSequence"?: ExecutionSequence
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "status": LeaseWriteStatus
  readonly "workerSessionId": WorkerSessionId
}

export enum RuntimeActivityOutcome {
  Observed = "observed",
  Succeeded = "succeeded",
  TaskFailed = "task-failed",
  TimedOut = "timed-out",
  PolicyDenied = "policy-denied",
  InfrastructureFailed = "infrastructure-failed",
  Cancelled = "cancelled",
}

export type RuntimeActivityProjection = {
  readonly "activityType": RuntimeActivityType
  readonly "callId": string
  readonly "command": string | null
  readonly "coreTool"?: CoreToolRuntimeProjection
  readonly "exitCode": number | null
  readonly "outcome": RuntimeActivityOutcome
  readonly "sourceRef": string
  readonly "status": RuntimeActivityStatus
}

export enum RuntimeActivityStatus {
  Running = "running",
  Completed = "completed",
  Failed = "failed",
  Declined = "declined",
  Cancelled = "cancelled",
  Unknown = "unknown",
}

export enum RuntimeActivityType {
  Command = "command",
  Test = "test",
  Tool = "tool",
}

export type RuntimeAgentEdgeProjection = {
  readonly "childThreadId": CodexThreadId
  readonly "parentThreadId": CodexThreadId
}

export type RuntimeAgentProjection = {
  readonly "nickname": string | null
  readonly "parentThreadId": CodexThreadId | null
  readonly "path": string | null
  readonly "role": string | null
  readonly "sourceRef": string
  readonly "status": RuntimeAgentStatus
  readonly "threadId": CodexThreadId
}

export enum RuntimeAgentStatus {
  Unknown = "unknown",
  Waiting = "waiting",
  Running = "running",
  Completed = "completed",
  Interrupted = "interrupted",
  Failed = "failed",
  Closed = "closed",
}

export type RuntimeDiffSummaryProjection = {
  readonly "additions": Count
  readonly "changedFileCount": Count
  readonly "deletions": Count
  readonly "detailsVisible": false
  readonly "sourceRef": string
}

export type RuntimeEventMessage = {
  readonly "codexThreadId": CodexThreadId
  readonly "event": ExecutionEventRecord
  readonly "kind": "runtime.event"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "workerSessionId": WorkerSessionId
}

export type RuntimePlanItemProjection = {
  readonly "status": RuntimePlanItemStatus
  readonly "step": string
}

export enum RuntimePlanItemStatus {
  Pending = "pending",
  InProgress = "in_progress",
  Completed = "completed",
}

export type RuntimePlanProjection = {
  readonly "complete": boolean
  readonly "explanation": string | null
  readonly "itemId": string | null
  readonly "items": ReadonlyArray<RuntimePlanItemProjection>
  readonly "sourceRef": string
  readonly "text": string | null
}

export type RuntimeProjectionEventCursor = ProductSessionEventReadCursor | DeliveryEventReadCursor

export type RuntimeProjectionGetParameters = ProductSessionRuntimeProjectionGetParameters | WorkRunRuntimeProjectionGetParameters

export type RuntimeProjectionGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": RuntimeProjectionGetParameters
  readonly "query": "runtime.projection.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type RuntimeProjectionGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "runtime.projection.get"
  readonly "requestId": RequestId
  readonly "result": RuntimeProjectionSnapshot
  readonly "schemaVersion": SchemaVersion
}

export type RuntimeProjectionSnapshot = {
  readonly "deliveryId": DeliveryId | null
  readonly "eventCursor": RuntimeProjectionEventCursor
  readonly "kind": "runtime_projection"
  readonly "lastProjectionSequence": number
  readonly "productSessionId": ProductSessionId
  readonly "readCursor": StrongFlowReadCursor | null
  readonly "rebuiltAt": Instant
  readonly "revision": Revision
  readonly "sessions": ReadonlyArray<RuntimeSessionProjection>
  readonly "workRunId": WorkRunId | null
} & ({
  readonly "deliveryId": null
  readonly "eventCursor": ProductSessionEventReadCursor
  readonly "readCursor": null
  readonly "workRunId": null
} | {
  readonly "deliveryId": DeliveryId
  readonly "eventCursor": DeliveryEventReadCursor
  readonly "readCursor": StrongFlowReadCursor
  readonly "workRunId": WorkRunId
})

export type RuntimeRecoveryProjection = {
  readonly "failureCount": Count
  readonly "lastFailureSourceRef": string | null
  readonly "latestRecoverySourceRef": string | null
  readonly "recoveryCount": Count
  readonly "state": RuntimeRecoveryState
}

export enum RuntimeRecoveryState {
  None = "none",
  Required = "required",
  InProgress = "in-progress",
  Recovered = "recovered",
}

export type RuntimeReplayRequestMessage = {
  readonly "afterSequence": ExecutionAckSequence
  readonly "kind": "runtime.replay_request"
  readonly "lease": ExecutionLeaseStamp
  readonly "maxEvents": number
  readonly "messageId": ExecutionMessageId
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "workerSessionId": WorkerSessionId
}

export type RuntimeSessionAgentIdentity = {
  readonly "id": AgentIdentityId
  readonly "name": string
  readonly "role": string
  readonly "workerId": WorkerId
}

export type RuntimeSessionContext = {
  readonly "agentIdentity": RuntimeSessionAgentIdentity
  readonly "model": string
  readonly "provider": string
  readonly "workspace": RuntimeSessionWorkspace
}

export type RuntimeSessionProjection = {
  readonly "activities": ReadonlyArray<RuntimeActivityProjection>
  readonly "agentEdges": ReadonlyArray<RuntimeAgentEdgeProjection>
  readonly "agents": ReadonlyArray<RuntimeAgentProjection>
  readonly "asOfSequence": number
  readonly "attempt": number
  readonly "codexThreadId": CodexThreadId
  readonly "diffSummary": RuntimeDiffSummaryProjection | null
  readonly "executionJobId": ExecutionJobId
  readonly "fencingToken": string
  readonly "leaseId": LeaseId
  readonly "plan": RuntimePlanProjection | null
  readonly "productSessionId": ProductSessionId
  readonly "recovery": RuntimeRecoveryProjection
  readonly "runtimeContext": RuntimeSessionContext | null
  readonly "sessionBindingId": string
  readonly "usage": RuntimeUsageProjection | null
  readonly "workerSessionId": WorkerSessionId
  readonly "workItemId": WorkItemId | null
  readonly "workRunId": WorkRunId | null
}

export type RuntimeSessionWorkspace = {
  readonly "repositoryId": RepositoryId
  readonly "revision": WorkspaceRevision
  readonly "writeMode": "read-only" | "candidate"
}

export type RuntimeUsageMetricProjection = {
  readonly "name": string
  readonly "value": number
}

export type RuntimeUsageProjection = {
  readonly "sourceRef": string
  readonly "totals": ReadonlyArray<RuntimeUsageMetricProjection>
}

export type SchemaVersion = "winwincode/v1"

export type Scope = OrganizationScope | WorkspaceScope | ProjectScope | RepositoryScope

export type ScopedResourceLocator = {
  readonly "organizationId": OrganizationId
  readonly "projectId": ProjectId
  readonly "repositoryId": RepositoryId
  readonly "workspaceId": WorkspaceId
}

export type ScopeEventReadCursor = {
  readonly "eventId": ControlPlaneEventId | null
  readonly "scope": Scope
  readonly "sequence": EventReadPosition
  readonly "stream": ScopeEventReadStream
} & ({
  readonly "eventId": null
  readonly "sequence": 0
} | {
  readonly "eventId": ControlPlaneEventId
  readonly "sequence": number
})

export type ScopeEventReadStream = {
  readonly "kind": "scope"
}

export type ServiceAccountActor = {
  readonly "id": ServiceAccountId
  readonly "kind": "service_account"
}

export type ServiceAccountId = string & { readonly __brand: "ServiceAccountId" }

export type SessionArtifactGetParameters = {
  readonly "artifactId": ArtifactId
  readonly "length": number
  readonly "offset": number
  readonly "productSessionId": ProductSessionId
}

export type SessionArtifactGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": SessionArtifactGetParameters
  readonly "query": "session.artifact.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type SessionArtifactGetResult = {
  readonly "artifactId": ArtifactId
  readonly "dataBase64": string
  readonly "digest": Sha256Digest
  readonly "offset": number
  readonly "totalSize": number
}

export type SessionArtifactGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "session.artifact.get"
  readonly "requestId": RequestId
  readonly "result": SessionArtifactGetResult
  readonly "schemaVersion": SchemaVersion
}

export type SessionBindingMessage = {
  readonly "attempt": number
  readonly "boundAt": Instant
  readonly "codexThreadId": CodexThreadId
  readonly "fencingToken": FencingToken
  readonly "kind": "session.binding"
  readonly "lease": ExecutionLeaseStamp
  readonly "leaseId": LeaseId
  readonly "messageId": ExecutionMessageId
  readonly "productSessionId": ProductSessionId
  readonly "runtimeContext": RuntimeSessionContext
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "sessionIdentity": SessionIdentity
  readonly "snapshotId"?: SnapshotId
  readonly "sourceIdentity": SessionBindingSourceIdentity
  readonly "workerId": WorkerId
  readonly "workerSessionId": WorkerSessionId
  readonly "workRunId"?: WorkRunId
}

export type SessionBindingSourceIdentity = {
  readonly "kind": "execution-worker"
  readonly "leaseId": LeaseId
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
  readonly "workerSessionId": WorkerSessionId
}

export type SessionCancelCommand = {
  readonly "actor": Actor
  readonly "command": "session.cancel"
  readonly "expectedRevision": Revision
  readonly "payload": SessionCancelPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type SessionCancelCompletedResponse = {
  readonly "command": "session.cancel"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": ProductSessionProjection
  readonly "schemaVersion": SchemaVersion
}

export type SessionCancelPayload = {
  readonly "productSessionId": ProductSessionId
  readonly "reason": string
}

export type SessionCloseCommand = {
  readonly "actor": Actor
  readonly "command": "session.close"
  readonly "expectedRevision": Revision
  readonly "payload": SessionClosePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type SessionCloseCompletedResponse = {
  readonly "command": "session.close"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": ProductSessionProjection
  readonly "schemaVersion": SchemaVersion
}

export type SessionClosePayload = {
  readonly "productSessionId": ProductSessionId
}

export type SessionCreateCommand = {
  readonly "actor": Actor
  readonly "command": "session.create"
  readonly "expectedRevision": Revision
  readonly "payload": SessionCreatePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type SessionCreateCompletedResponse = {
  readonly "command": "session.create"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": ProductSessionProjection
  readonly "schemaVersion": SchemaVersion
}

export type SessionCreatePayload = {
  readonly "modelRoute": ModelRoute
  readonly "productSessionId": ProductSessionId
  readonly "projectId": ProjectId
  readonly "repositoryId": RepositoryId
  readonly "title": string
}

export type SessionGetParameters = {
  readonly "productSessionId": ProductSessionId
}

export type SessionGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": SessionGetParameters
  readonly "query": "session.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type SessionGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "session.get"
  readonly "requestId": RequestId
  readonly "result": ProductSessionProjection
  readonly "schemaVersion": SchemaVersion
}

export type SessionIdentity = {
  readonly "codexThreadId": CodexThreadId
  readonly "productSessionId": ProductSessionId
  readonly "workerSessionId": WorkerSessionId
  readonly "workRunId"?: WorkRunId
}

export type SessionListParameters = {
  readonly "states": ReadonlyArray<"idle" | "running" | "waiting_for_input" | "waiting_for_approval" | "cancelled" | "closed" | "failed">
}

export type SessionListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": SessionListParameters
  readonly "query": "session.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type SessionListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "session.list"
  readonly "requestId": RequestId
  readonly "result": ProductSessionPage
  readonly "schemaVersion": SchemaVersion
}

export type SessionMessagesListParameters = {
  readonly "productSessionId": ProductSessionId
}

export type SessionMessagesListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": SessionMessagesListParameters
  readonly "query": "session.messages.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type SessionMessagesListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "session.messages.list"
  readonly "requestId": RequestId
  readonly "result": ChatMessagePage
  readonly "schemaVersion": SchemaVersion
}

export type SessionUpdateCommand = {
  readonly "actor": Actor
  readonly "command": "session.update"
  readonly "expectedRevision": Revision
  readonly "payload": SessionUpdatePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type SessionUpdateCompletedResponse = {
  readonly "command": "session.update"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": ProductSessionProjection
  readonly "schemaVersion": SchemaVersion
}

export type SessionUpdatePayload = {
  readonly "archived": boolean
  readonly "productSessionId": ProductSessionId
  readonly "title": string
}

export type SettingsGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": EmptyParameters
  readonly "query": "settings.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type SettingsGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "settings.get"
  readonly "requestId": RequestId
  readonly "result": SettingsProjection
  readonly "schemaVersion": SchemaVersion
}

export type SettingsPatch = {
  readonly "defaultModelRoute": ModelRoute | null
  readonly "workerConcurrencyLimit": number
}

export type SettingsProjection = {
  readonly "defaultModelRoute": ModelRoute | null
  readonly "revision": Revision
  readonly "workerConcurrencyLimit": number
}

export type SettingsUpdateCommand = {
  readonly "actor": Actor
  readonly "command": "settings.update"
  readonly "expectedRevision": Revision
  readonly "payload": SettingsUpdatePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type SettingsUpdateCompletedResponse = {
  readonly "command": "settings.update"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": SettingsProjection
  readonly "schemaVersion": SchemaVersion
}

export type SettingsUpdatePayload = {
  readonly "patch": SettingsPatch
}

export type Sha256Digest = string

export type Snapshot = {
  readonly "baseCommitId": GitObjectId
  readonly "baseTreeId": GitObjectId
  readonly "candidateCommitId": GitObjectId
  readonly "candidateId": CandidateId
  readonly "candidateTreeId": GitObjectId
  readonly "contentDigest": Sha256Digest
  readonly "createdAtMillis": number
  readonly "diffSha256": Sha256Digest
  readonly "immutable": true
  readonly "repositoryId": RepositoryId
  readonly "schemaVersion": SchemaVersion
  readonly "snapshotId": SnapshotId
  readonly "validationSeal": Sha256Digest
  readonly "workRunId": WorkRunId
}

export type SnapshotFreezeReceipt = {
  readonly "attempt": number
  readonly "baseCommitId": GitObjectId
  readonly "baseTreeId": GitObjectId
  readonly "candidateCommitId": GitObjectId
  readonly "candidateId": CandidateId
  readonly "candidateTreeId": GitObjectId
  readonly "contentDigest": Sha256Digest
  readonly "diffSha256": Sha256Digest
  readonly "fencingToken": FencingToken
  readonly "frozenAt": Instant
  readonly "leaseId": LeaseId
  readonly "receiptId": RequestId
  readonly "repositoryId": RepositoryId
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "validationSeal": Sha256Digest
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
  readonly "workRunId": WorkRunId
}

export type SnapshotFreezeReceiptMessage = {
  readonly "kind": "snapshot.freeze_receipt"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "receipt": SnapshotFreezeReceipt
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
}

export type SnapshotFreezeRequestMessage = {
  readonly "baseTreeId": GitObjectId
  readonly "candidate": Candidate
  readonly "contentDigest": Sha256Digest
  readonly "dispatch": JobDispatchMessage
  readonly "kind": "snapshot.freeze_request"
  readonly "lease": ExecutionLeaseStamp
  readonly "messageId": ExecutionMessageId
  readonly "repositoryId": RepositoryId
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
}

export type SnapshotId = string & { readonly __brand: "SnapshotId" }

export type SnapshotVerificationDispatchMessage = {
  readonly "dispatch": JobDispatchMessage
  readonly "kind": "snapshot.verify"
  readonly "messageId": ExecutionMessageId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "snapshot": Snapshot
}

export type SolutionReviewComponentProjection = {
  readonly "id": string
  readonly "kind": "component" | "external" | "data-store"
  readonly "label": string
  readonly "repositoryPathPrefixes": ReadonlyArray<string>
  readonly "responsibility": string
  readonly "trustBoundary": string | null
  readonly "unresolved": boolean
}

export type SolutionReviewConnectionProjection = {
  readonly "from": string
  readonly "id": string
  readonly "label": string
  readonly "to": string
}

export enum SolutionReviewDecision {
  Approve = "approve",
  RequestChanges = "request_changes",
  Reject = "reject",
}

export type SolutionReviewDiagramEdgeProjection = {
  readonly "from": string
  readonly "id": string
  readonly "label": string
  readonly "to": string
}

export type SolutionReviewDiagramNodeProjection = {
  readonly "description": string
  readonly "id": string
  readonly "kind": "interaction" | "delivery-control" | "execution" | "repository" | "component" | "external" | "data-store" | "stage" | "decision"
  readonly "label": string
  readonly "trustBoundary": string | null
  readonly "unresolved": boolean
}

export type SolutionReviewDiagramProjection = {
  readonly "edges": ReadonlyArray<SolutionReviewDiagramEdgeProjection>
  readonly "id": string
  readonly "kind": "system-architecture" | "process-flow"
  readonly "nodes": ReadonlyArray<SolutionReviewDiagramNodeProjection>
  readonly "title": string
}

export type SolutionReviewProjection = {
  readonly "approach": ReadonlyArray<string>
  readonly "architectureDiagram": SolutionReviewDiagramProjection
  readonly "attentionItemId": AttentionItemId
  readonly "comments": string | null
  readonly "components": ReadonlyArray<SolutionReviewComponentProjection>
  readonly "connections": ReadonlyArray<SolutionReviewConnectionProjection>
  readonly "decision": SolutionReviewDecision | null
  readonly "deliveryId": DeliveryId
  readonly "deliverySpecId": string
  readonly "deliverySpecRevision": Revision
  readonly "planningSessionBindingId": string
  readonly "planningWorkRunId": WorkRunId
  readonly "processDiagram": SolutionReviewDiagramProjection
  readonly "requestedChanges": ReadonlyArray<string> | null
  readonly "reviewedAt": Instant | null
  readonly "reviewerId": ActorId | null
  readonly "reviewSetSha256": Sha256Digest
  readonly "reviewStatus": SolutionReviewStatus
  readonly "reviewWorkRunId": WorkRunId | null
  readonly "risks": ReadonlyArray<string>
  readonly "solutionId": string
  readonly "summary": string
  readonly "unresolvedItems": ReadonlyArray<string>
  readonly "workItemProposals": ReadonlyArray<WorkItemProposalProjection>
} & ({
  readonly "comments": null
  readonly "decision": null
  readonly "requestedChanges": null
  readonly "reviewedAt": null
  readonly "reviewerId": null
  readonly "reviewStatus": SolutionReviewStatus.Pending
} | {
  readonly "decision": SolutionReviewDecision.Approve
  readonly "requestedChanges": null
  readonly "reviewedAt": Instant
  readonly "reviewerId": ActorId
  readonly "reviewStatus": SolutionReviewStatus.Approved
} | {
  readonly "decision": SolutionReviewDecision.RequestChanges
  readonly "requestedChanges": ReadonlyArray<string>
  readonly "reviewedAt": Instant
  readonly "reviewerId": ActorId
  readonly "reviewStatus": SolutionReviewStatus.ChangesRequested
} | {
  readonly "decision": SolutionReviewDecision.Reject
  readonly "requestedChanges": null
  readonly "reviewedAt": Instant
  readonly "reviewerId": ActorId
  readonly "reviewStatus": SolutionReviewStatus.Rejected
})

export enum SolutionReviewStatus {
  Pending = "pending",
  Approved = "approved",
  ChangesRequested = "changes_requested",
  Rejected = "rejected",
}

export type StrongFlowDiagramDiffFileProjection = {
  readonly "id": string
  readonly "nodeIds": ReadonlyArray<string>
  readonly "path": string
  readonly "state": "present" | "deleted"
}

export type StrongFlowDiagramExecutionDetailsProjection = {
  readonly "candidate": FrozenCandidateSummaryProjection
  readonly "diffSha256": Sha256Digest
  readonly "files": ReadonlyArray<StrongFlowDiagramDiffFileProjection>
  readonly "provenance": StrongFlowDiagramExecutionProvenanceProjection
}

export type StrongFlowDiagramExecutionDiagramProjection = {
  readonly "diagramId": string
  readonly "kind": "system-architecture" | "process-flow"
  readonly "nodes": ReadonlyArray<StrongFlowDiagramExecutionNodeProjection>
}

export type StrongFlowDiagramExecutionNodeProjection = {
  readonly "affectedFileCount": Count
  readonly "fileIds": ReadonlyArray<string>
  readonly "nodeId": string
  readonly "state": "normal" | "affected-live" | "affected-finished"
}

export type StrongFlowDiagramExecutionProjection = {
  readonly "affectedFileCount": Count
  readonly "architecture": StrongFlowDiagramExecutionDiagramProjection
  readonly "deliveryId": DeliveryId
  readonly "deliveryRevision": Revision
  readonly "details": StrongFlowDiagramExecutionDetailsProjection | null
  readonly "process": StrongFlowDiagramExecutionDiagramProjection
  readonly "protocol": "winwincode.diagram-execution-projection.v1"
  readonly "reviewSetSha256": Sha256Digest
  readonly "schemaVersion": 1
  readonly "state": "before-execution" | "executing" | "execution-finished"
  readonly "updatedAt": Instant
}

export type StrongFlowDiagramExecutionProvenanceProjection = {
  readonly "evidenceRefIds": ReadonlyArray<EvidenceId>
  readonly "sessionBindingId": string
  readonly "workItemId": WorkItemId
  readonly "workRunId": WorkRunId
}

export type StrongFlowReadCursor = {
  readonly "deliveryId": DeliveryId
  readonly "deliveryRevision": Revision
  readonly "eventCursor": DeliveryEventReadCursor
  readonly "publicationRevision": Revision
  readonly "runtimeAcceptedSequence": number
  readonly "runtimeLedgerRevision": Revision
  readonly "scope": RepositoryScope
  readonly "token": string
}

export enum StructuredOutputSupport {
  Unsupported = "unsupported",
  JsonSchemaStrict = "json_schema_strict",
}

export type SystemActor = {
  readonly "id": SystemActorId
  readonly "kind": "system"
}

export type SystemActorId = string & { readonly __brand: "SystemActorId" }

export type TerminalError = {
  readonly "code": TerminalErrorCode
  readonly "details": ErrorDetails
  readonly "message": string
  readonly "retryable": false
}

export enum TerminalErrorCode {
  InvalidRequest = "INVALID_REQUEST",
  AuthenticationRequired = "AUTHENTICATION_REQUIRED",
  PermissionDenied = "PERMISSION_DENIED",
  ResourceNotFound = "RESOURCE_NOT_FOUND",
  IdempotencyConflict = "IDEMPOTENCY_CONFLICT",
  RevisionConflict = "REVISION_CONFLICT",
  CandidateStale = "CANDIDATE_STALE",
  WrongState = "WRONG_STATE",
  DeviceSessionRequired = "DEVICE_SESSION_REQUIRED",
  DeviceModelUnavailable = "DEVICE_MODEL_UNAVAILABLE",
  InternalError = "INTERNAL_ERROR",
}

export type UserActor = {
  readonly "id": UserId
  readonly "kind": "user"
}

export type UserId = string & { readonly __brand: "UserId" }

export enum ValidationCheckStatus {
  Passed = "passed",
  Failed = "failed",
  Skipped = "skipped",
  InfrastructureError = "infrastructure_error",
  Cancelled = "cancelled",
}

export type ValidationCheckSummary = {
  readonly "diagnosticDigest"?: Sha256Digest
  readonly "name": string
  readonly "status": ValidationCheckStatus
  readonly "summary": string
} & ({
  readonly "name": string
  readonly "summary": string
})

export enum ValidationCommandLanguage {
  Typescript = "typescript",
  Rust = "rust",
  Python = "python",
  Go = "go",
}

export enum ValidationCommandPhase {
  Formatter = "formatter",
  SafeLintFix = "safe_lint_fix",
  Codegen = "codegen",
  LockfileSync = "lockfile_sync",
  Validation = "validation",
}

export type ValidationCommandSpec = {
  readonly "allowedCompanionPaths": ReadonlyArray<string>
  readonly "argv": ReadonlyArray<string>
  readonly "diagnosticParserVersion"?: DiagnosticParserVersion | null
  readonly "environment": ReadonlyArray<ValidationEnvironmentVariable>
  readonly "flakyRerunLimit"?: number
  readonly "id": string
  readonly "language": ValidationCommandLanguage
  readonly "network": false
  readonly "outputLimitBytes": number
  readonly "phase": ValidationCommandPhase
  readonly "timeoutMillis": number
  readonly "workingDirectory": string
} & ({
  readonly "allowedCompanionPaths": ReadonlyArray<string>
  readonly "argv": ReadonlyArray<string>
  readonly "diagnosticParserVersion"?: null
  readonly "environment": ReadonlyArray<ValidationEnvironmentVariable>
  readonly "id": string
  readonly "network": false
  readonly "outputLimitBytes": number
  readonly "phase": ValidationCommandPhase.Formatter
  readonly "timeoutMillis": number
  readonly "workingDirectory": string
} | {
  readonly "allowedCompanionPaths": ReadonlyArray<string>
  readonly "argv": ReadonlyArray<string>
  readonly "diagnosticParserVersion"?: null
  readonly "environment": ReadonlyArray<ValidationEnvironmentVariable>
  readonly "id": string
  readonly "network": false
  readonly "outputLimitBytes": number
  readonly "phase": ValidationCommandPhase.SafeLintFix
  readonly "timeoutMillis": number
  readonly "workingDirectory": string
} | {
  readonly "allowedCompanionPaths": ReadonlyArray<string>
  readonly "argv": ReadonlyArray<string>
  readonly "diagnosticParserVersion"?: null
  readonly "environment": ReadonlyArray<ValidationEnvironmentVariable>
  readonly "id": string
  readonly "network": false
  readonly "outputLimitBytes": number
  readonly "phase": ValidationCommandPhase.Codegen
  readonly "timeoutMillis": number
  readonly "workingDirectory": string
} | {
  readonly "allowedCompanionPaths": ReadonlyArray<string>
  readonly "argv": ReadonlyArray<string>
  readonly "diagnosticParserVersion"?: null
  readonly "environment": ReadonlyArray<ValidationEnvironmentVariable>
  readonly "id": string
  readonly "network": false
  readonly "outputLimitBytes": number
  readonly "phase": ValidationCommandPhase.LockfileSync
  readonly "timeoutMillis": number
  readonly "workingDirectory": string
} | {
  readonly "allowedCompanionPaths": ReadonlyArray<unknown>
  readonly "argv": ReadonlyArray<string>
  readonly "environment": ReadonlyArray<ValidationEnvironmentVariable>
  readonly "id": string
  readonly "network": false
  readonly "outputLimitBytes": number
  readonly "phase": ValidationCommandPhase.Validation
  readonly "timeoutMillis": number
  readonly "workingDirectory": string
})

export type ValidationConfiguration = {
  readonly "commands": ReadonlyArray<ValidationCommandSpec>
  readonly "profiles": ReadonlyArray<ValidationProfile>
  readonly "schemaVersion": number
} & ({
  readonly "commands": ReadonlyArray<unknown>
  readonly "profiles": ReadonlyArray<unknown>
  readonly "schemaVersion": 1
})

export enum ValidationEnvironmentName {
  Home = "HOME",
  Lang = "LANG",
  LcAll = "LC_ALL",
  Path = "PATH",
  Tmpdir = "TMPDIR",
}

export type ValidationEnvironmentVariable = {
  readonly "name": ValidationEnvironmentName
  readonly "value": string
} & ({
  readonly "value": string
})

export type ValidationProfile = {
  readonly "commandIds": ReadonlyArray<string>
  readonly "name": ValidationProfileName
} & ({
  readonly "commandIds": ReadonlyArray<string>
})

export enum ValidationProfileName {
  Changed = "changed",
  Fast = "fast",
  Affected = "affected",
  Final = "final",
}

export type ValidationProfileSelection = {
  readonly "changedPathsDigest": Sha256Digest
  readonly "commandIds": ReadonlyArray<string>
  readonly "configurationDigest"?: Sha256Digest
  readonly "executable": boolean
  readonly "profile": ValidationProfileName
  readonly "reasonCode": ValidationProfileSelectionReasonCode
  readonly "source": ValidationSelectionSource
} & ({
  readonly "commandIds": ReadonlyArray<unknown>
  readonly "configurationDigest": Sha256Digest
  readonly "executable": true
  readonly "reasonCode": ValidationProfileSelectionReasonCode.ExplicitProfile
  readonly "source": ValidationSelectionSource.ExplicitConfiguration
} | {
  readonly "commandIds": ReadonlyArray<unknown>
  readonly "executable": false
  readonly "profile": ValidationProfileName.Affected
  readonly "reasonCode": ValidationProfileSelectionReasonCode.LockfileChanged
  readonly "source": ValidationSelectionSource.AutomaticSuggestion
} | {
  readonly "commandIds": ReadonlyArray<unknown>
  readonly "executable": false
  readonly "profile": ValidationProfileName.Affected
  readonly "reasonCode": ValidationProfileSelectionReasonCode.MixedLanguages
  readonly "source": ValidationSelectionSource.AutomaticSuggestion
} | {
  readonly "commandIds": ReadonlyArray<unknown>
  readonly "executable": false
  readonly "profile": ValidationProfileName.Changed
  readonly "reasonCode": ValidationProfileSelectionReasonCode.SingleLanguage
  readonly "source": ValidationSelectionSource.AutomaticSuggestion
} | {
  readonly "commandIds": ReadonlyArray<unknown>
  readonly "executable": false
  readonly "profile": ValidationProfileName.Changed
  readonly "reasonCode": ValidationProfileSelectionReasonCode.NoChangedPaths
  readonly "source": ValidationSelectionSource.AutomaticSuggestion
})

export enum ValidationProfileSelectionReasonCode {
  ExplicitProfile = "explicit_profile",
  LockfileChanged = "lockfile_changed",
  MixedLanguages = "mixed_languages",
  SingleLanguage = "single_language",
  NoChangedPaths = "no_changed_paths",
}

export type ValidationReceipt = {
  readonly "artifactRefs": ReadonlyArray<ArtifactReference>
  readonly "baseRevision": WorkspaceRevision
  readonly "checks": ReadonlyArray<ValidationCheckSummary>
  readonly "durationMillis": number
  readonly "profile": ValidationProfileName
  readonly "resultRevision"?: WorkspaceRevision
  readonly "status": ValidationReceiptStatus
} & ({
  readonly "checks": ReadonlyArray<unknown>
  readonly "durationMillis": 0
  readonly "resultRevision"?: WorkspaceRevision
  readonly "status": ValidationReceiptStatus.NotRun
} | {
  readonly "checks": ReadonlyArray<unknown>
  readonly "resultRevision": WorkspaceRevision
  readonly "status": ValidationReceiptStatus.Passed
} | {
  readonly "checks": ReadonlyArray<unknown>
  readonly "resultRevision": WorkspaceRevision
  readonly "status": ValidationReceiptStatus.Failed
} | {
  readonly "checks": ReadonlyArray<unknown>
  readonly "resultRevision": WorkspaceRevision
  readonly "status": ValidationReceiptStatus.InfrastructureError
} | {
  readonly "checks": ReadonlyArray<unknown>
  readonly "resultRevision": WorkspaceRevision
  readonly "status": ValidationReceiptStatus.Cancelled
})

export enum ValidationReceiptStatus {
  NotRun = "not_run",
  Passed = "passed",
  Failed = "failed",
  InfrastructureError = "infrastructure_error",
  Cancelled = "cancelled",
}

export enum ValidationSelectionSource {
  ExplicitConfiguration = "explicit_configuration",
  AutomaticSuggestion = "automatic_suggestion",
}

export type Verdict = {
  readonly "attempt": number
  readonly "candidateId": CandidateId
  readonly "contractRevision": Revision
  readonly "createdAt": Instant
  readonly "evidenceIds": ReadonlyArray<EvidenceId>
  readonly "id": VerdictId
  readonly "planRevision": Revision
  readonly "revision": Revision
  readonly "schemaVersion": SchemaVersion
  readonly "sessionIdentity": SessionIdentity
  readonly "snapshotId": SnapshotId
  readonly "status": "pass" | "fail" | "inconclusive" | "infra_error"
  readonly "verificationPlanId": VerificationPlanId
  readonly "verificationSessionId": VerificationSessionId
  readonly "workContractId": WorkContractId
  readonly "workItemId": WorkItemId
  readonly "workRunId": WorkRunId
}

export type VerdictId = string & { readonly __brand: "VerdictId" }

export type VerificationPlan = {
  readonly "candidateDigest": CandidateDigest
  readonly "commands": ReadonlyArray<string>
  readonly "contractRevision": Revision
  readonly "criterionIds": ReadonlyArray<CriterionId>
  readonly "id": VerificationPlanId
  readonly "permissionProfile": "candidate_read_only_restricted"
  readonly "planRevision": Revision
  readonly "requiredRoles": ReadonlyArray<"reviewer" | "verifier" | "adversarial-verifier">
  readonly "schemaVersion": SchemaVersion
  readonly "workContractId": WorkContractId
  readonly "workItemId": WorkItemId
  readonly "workItemRevision": Revision
  readonly "workRunId": WorkRunId
}

export type VerificationPlanId = string & { readonly __brand: "VerificationPlanId" }

export type VerificationSession = {
  readonly "attempt": number
  readonly "candidateId": CandidateId
  readonly "createdAt": Instant
  readonly "id": VerificationSessionId
  readonly "schemaVersion": SchemaVersion
  readonly "sessionIdentity": SessionIdentity
  readonly "snapshotId": SnapshotId
  readonly "verificationPlanId": VerificationPlanId
  readonly "verificationSessionId": VerificationSessionId
  readonly "workRunId": WorkRunId
}

export type VerificationSessionId = string & { readonly __brand: "VerificationSessionId" }

export type VerifierResult = {
  readonly "attempt": number
  readonly "candidateId": CandidateId
  readonly "createdAt": Instant
  readonly "evidenceIds": ReadonlyArray<EvidenceId>
  readonly "id": VerifierResultId
  readonly "schemaVersion": SchemaVersion
  readonly "sessionIdentity": SessionIdentity
  readonly "snapshotId": SnapshotId
  readonly "status": "pass" | "fail" | "inconclusive" | "infra_error"
  readonly "verificationPlanId": VerificationPlanId
  readonly "verificationSessionId": VerificationSessionId
  readonly "workRunId": WorkRunId
}

export type VerifierResultId = string & { readonly __brand: "VerifierResultId" }

export type WorkContract = {
  readonly "constraints": ReadonlyArray<string>
  readonly "createdAt": Instant
  readonly "criteria": ReadonlyArray<Criterion>
  readonly "id": WorkContractId
  readonly "objective": string
  readonly "protectedScope": ReadonlyArray<string>
  readonly "requiredHumanAuthority": "none" | "approval" | "attention"
  readonly "revision": Revision
  readonly "schemaVersion": SchemaVersion
  readonly "scope": ReadonlyArray<string>
}

export type WorkContractId = string & { readonly __brand: "WorkContractId" }

export enum WorkerCapabilityFeature {
  ArtifactStream = "artifact_stream",
  Approval = "approval",
  Git = "git",
  InteractiveInput = "interactive_input",
  Mcp = "mcp",
  ModelProxy = "model_proxy",
  Sandbox = "sandbox",
  Shell = "shell",
}

export type WorkerCapabilitySet = {
  readonly "capabilityDigest": Sha256Digest
  readonly "features": ReadonlyArray<WorkerCapabilityFeature>
  readonly "maxConcurrentJobs": number
  readonly "platform": "aarch64-apple-darwin" | "x86_64-apple-darwin" | "aarch64-unknown-linux-gnu" | "x86_64-unknown-linux-gnu"
}

export type WorkerCapacity = {
  readonly "availableSlots": number
  readonly "runningJobs": number
}

export type WorkerDrainCommand = {
  readonly "actor": Actor
  readonly "command": "worker.drain"
  readonly "expectedRevision": Revision
  readonly "payload": WorkerDrainPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type WorkerDrainCompletedResponse = {
  readonly "command": "worker.drain"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": WorkerProjection
  readonly "schemaVersion": SchemaVersion
}

export type WorkerDrainPayload = {
  readonly "reason": string
  readonly "workerId": WorkerId
}

export type WorkerEnableCommand = {
  readonly "actor": Actor
  readonly "command": "worker.enable"
  readonly "expectedRevision": Revision
  readonly "payload": WorkerEnablePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type WorkerEnableCompletedResponse = {
  readonly "command": "worker.enable"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": WorkerProjection
  readonly "schemaVersion": SchemaVersion
}

export type WorkerEnablePayload = {
  readonly "workerId": WorkerId
}

export type WorkerGetParameters = {
  readonly "workerId": WorkerId
}

export type WorkerGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": WorkerGetParameters
  readonly "query": "worker.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type WorkerGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "worker.get"
  readonly "requestId": RequestId
  readonly "result": WorkerProjection
  readonly "schemaVersion": SchemaVersion
}

export type WorkerHeartbeatAckMessage = {
  readonly "error"?: ExecutionPortError
  readonly "heartbeatSequence": ExecutionSequence
  readonly "kind": "worker.heartbeat_ack"
  readonly "messageId": ExecutionMessageId
  readonly "nextHeartbeatWithinMs": number
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "serverTime": Instant
  readonly "status": "accepted" | "duplicate" | "rejected_worker_instance"
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
}

export type WorkerHeartbeatMessage = {
  readonly "activeLeases": ReadonlyArray<ActiveLeaseSummary>
  readonly "capacity": WorkerCapacity
  readonly "heartbeatSequence": ExecutionSequence
  readonly "kind": "worker.heartbeat"
  readonly "messageId": ExecutionMessageId
  readonly "observedAt": Instant
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
}

export type WorkerId = string & { readonly __brand: "WorkerId" }

export type WorkerInstanceId = string & { readonly __brand: "WorkerInstanceId" }

export enum WorkerLaunchAckStatus {
  Accepted = "accepted",
  Duplicate = "duplicate",
  RejectedStaleFencingToken = "rejected_stale_fencing_token",
  RejectedLeaseMismatch = "rejected_lease_mismatch",
  RejectedCapacityExhausted = "rejected_capacity_exhausted",
  RejectedRepositoryUnavailable = "rejected_repository_unavailable",
  RejectedUnknownGrant = "rejected_unknown_grant",
  RejectedWrongState = "rejected_wrong_state",
}

export type WorkerLaunchGrant = {
  readonly "clientInstanceId": ClientInstanceId
  readonly "clientNodeId": ClientNodeId
  readonly "expiresAt": Instant
  readonly "occupancyFencingToken": OccupancyFencingToken
  readonly "occupancyLeaseId": ClientOccupancyLeaseId
  readonly "productSessionId": ProductSessionId
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "revision": Revision
  readonly "state": WorkerLaunchGrantState
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
  readonly "workerLaunchGrantId": WorkerLaunchGrantId
  readonly "workerSessionId": WorkerSessionId
  readonly "workRunId"?: WorkRunId | null
}

export type WorkerLaunchGrantId = string & { readonly __brand: "WorkerLaunchGrantId" }

export enum WorkerLaunchGrantState {
  Issued = "issued",
  Consumed = "consumed",
  Revoked = "revoked",
  Expired = "expired",
}

export type WorkerListParameters = {
  readonly "states": ReadonlyArray<"enabled" | "draining" | "offline">
}

export type WorkerListQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": WorkerListParameters
  readonly "query": "worker.list"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": Scope
}

export type WorkerListResultResponse = {
  readonly "page": PageInfo
  readonly "query": "worker.list"
  readonly "requestId": RequestId
  readonly "result": WorkerPage
  readonly "schemaVersion": SchemaVersion
}

export type WorkerPage = {
  readonly "items": ReadonlyArray<WorkerProjection>
  readonly "kind": "worker_page"
}

export type WorkerProjection = {
  readonly "capacity": number
  readonly "id": WorkerId
  readonly "lastHeartbeatAt": Instant | null
  readonly "revision": Revision
  readonly "state": "enabled" | "draining" | "offline"
}

export enum WorkerReconcileState {
  StillRunning = "still_running",
  Terminal = "terminal",
  Missing = "missing",
  Unknown = "unknown",
}

export type WorkerRegisterMessage = {
  readonly "capabilities": WorkerCapabilitySet
  readonly "kind": "worker.register"
  readonly "messageId": ExecutionMessageId
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "startedAt": Instant
  readonly "usageAccountingVersion": number
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
}

export type WorkerRegistrationResultMessage = {
  readonly "error"?: ExecutionPortError
  readonly "heartbeatIntervalMs": number
  readonly "kind": "worker.registration_result"
  readonly "leaseRecovery": "no_active_leases" | "reacquire_required"
  readonly "messageId": ExecutionMessageId
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "sentAt": Instant
  readonly "serverTime": Instant
  readonly "status": "accepted" | "duplicate" | "rejected"
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
}

export type WorkerSessionId = string & { readonly __brand: "WorkerSessionId" }

export type WorkGraphItemProjection = {
  readonly "blockers": ReadonlyArray<WorkItemId>
  readonly "dependencies": ReadonlyArray<WorkItemId>
  readonly "state": WorkGraphItemState
  readonly "workItemId": WorkItemId
}

export enum WorkGraphItemState {
  Ready = "ready",
  Running = "running",
  Blocked = "blocked",
  Done = "done",
}

export type WorkItem = {
  readonly "criterionIds": ReadonlyArray<CriterionId>
  readonly "dependsOn": ReadonlyArray<WorkItemId>
  readonly "goal": string
  readonly "id": WorkItemId
  readonly "revision": Revision
  readonly "schemaVersion": SchemaVersion
  readonly "state": WorkItemState
  readonly "title": string
  readonly "workContractId": WorkContractId
  readonly "workContractRevision": Revision
}

export type WorkItemCountsProjection = {
  readonly "backlog": Count
  readonly "cancelled": Count
  readonly "candidateReady": Count
  readonly "done": Count
  readonly "failed": Count
  readonly "inProgress": Count
  readonly "ready": Count
  readonly "rework": Count
  readonly "total": Count
  readonly "validating": Count
  readonly "waitingDependency": Count
  readonly "waitingHuman": Count
}

export type WorkItemId = string & { readonly __brand: "WorkItemId" }

export type WorkItemProposalProjection = {
  readonly "criterionIds": ReadonlyArray<CriterionId>
  readonly "dependsOn": ReadonlyArray<WorkItemId>
  readonly "goal": string
  readonly "id": WorkItemId
  readonly "title": string
}

export type WorkItemsCreateCommand = {
  readonly "actor": Actor
  readonly "command": "workitems.create"
  readonly "expectedRevision": Revision
  readonly "payload": WorkItemsCreatePayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type WorkItemsCreateCompletedResponse = {
  readonly "command": "workitems.create"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": WorkItemsCreateResult
  readonly "schemaVersion": SchemaVersion
}

export type WorkItemsCreateItem = {
  readonly "criterionIds": ReadonlyArray<CriterionId>
  readonly "dependsOn": ReadonlyArray<WorkItemId>
  readonly "goal": string
  readonly "id": WorkItemId
  readonly "title": string
}

export type WorkItemsCreatePayload = {
  readonly "contractRevision": Revision
  readonly "deliveryId": DeliveryId
  readonly "expectedRevision": Revision
  readonly "items": ReadonlyArray<WorkItemsCreateItem>
}

export type WorkItemsCreateResult = {
  readonly "deliveryId": DeliveryId
  readonly "deliveryRevision": Revision
  readonly "items": ReadonlyArray<WorkItem>
}

export enum WorkItemState {
  Backlog = "backlog",
  Ready = "ready",
  InProgress = "in_progress",
  WaitingDependency = "waiting_dependency",
  WaitingHuman = "waiting_human",
  CandidateReady = "candidate_ready",
  Validating = "validating",
  Rework = "rework",
  Done = "done",
  Failed = "failed",
  Cancelled = "cancelled",
}

export type WorkRun = {
  readonly "attempt": number
  readonly "candidateDigest"?: CandidateDigest | null
  readonly "codexThreadId"?: CodexThreadId | null
  readonly "contractRevision": Revision
  readonly "executionJobId": ExecutionJobId
  readonly "fencingToken": string
  readonly "id": WorkRunId
  readonly "leaseId": LeaseId
  readonly "productSessionId": ProductSessionId | null
  readonly "revision": Revision
  readonly "schemaVersion": SchemaVersion
  readonly "state": WorkRunState
  readonly "workContractId": WorkContractId
  readonly "workerId": WorkerId
  readonly "workerInstanceId": WorkerInstanceId
  readonly "workerSessionId": WorkerSessionId
  readonly "workItemId": WorkItemId
  readonly "workItemRevision": Revision
}

export type WorkRunAggregateProjection = {
  readonly "contract": WorkContract
  readonly "deviceBindings": ReadonlyArray<WorkRunDeviceBindingProjection>
  readonly "graphItems": ReadonlyArray<WorkGraphItemProjection>
  readonly "items": ReadonlyArray<WorkItem>
  readonly "managedAppRunConfigs": ReadonlyArray<ManagedAppRunControlProjection>
  readonly "readCursor": StrongFlowReadCursor
  readonly "runs": ReadonlyArray<WorkRun>
  readonly "schemaVersion": SchemaVersion
}

export type WorkRunCancelCommand = {
  readonly "actor": Actor
  readonly "command": "workrun.cancel"
  readonly "expectedRevision": Revision
  readonly "payload": WorkRunCancelPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type WorkRunCancelCompletedResponse = {
  readonly "command": "workrun.cancel"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": DeliveryProjection
  readonly "schemaVersion": SchemaVersion
}

export type WorkRunCancelPayload = {
  readonly "deliveryId": DeliveryId
  readonly "workRunId": WorkRunId
}

export type WorkRunDeviceBindingProjection = {
  readonly "clientId": PublicClientId
  readonly "repositoryBindingId": RepositoryBindingId
  readonly "workRunId": WorkRunId
}

export type WorkRunExecutionScope = {
  readonly "attempt": number
  readonly "kind": "work-run"
  readonly "productSessionId": ProductSessionId
  readonly "reworkAuthorization"?: DeliveryReworkAuthorizationScope
  readonly "workContractId": WorkContractId
  readonly "workContractRevision": Revision
  readonly "workItemId": WorkItemId
  readonly "workItemRevision": Revision
  readonly "workRunId": WorkRunId
}

export type WorkRunGetParameters = {
  readonly "atCursor": StrongFlowReadCursor | null
  readonly "deliveryId": DeliveryId
  readonly "workItemId": WorkItemId | null
  readonly "workRunId"?: WorkRunId | null
}

export type WorkRunGetQuery = {
  readonly "actor": Actor
  readonly "page": PageRequest
  readonly "parameters": WorkRunGetParameters
  readonly "query": "workrun.get"
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type WorkRunGetResultResponse = {
  readonly "page": PageInfo
  readonly "query": "workrun.get"
  readonly "requestId": RequestId
  readonly "result": WorkRunAggregateProjection
  readonly "schemaVersion": SchemaVersion
}

export type WorkRunId = string & { readonly __brand: "WorkRunId" }

export type WorkRunInput = {
  readonly "candidateRef"?: string
  readonly "deliverySpecId": string
  readonly "deliverySpecRevision": Revision
  readonly "deviceTarget"?: ExecutionDeviceTarget
  readonly "schemaVersion": SchemaVersion
  readonly "snapshotId"?: SnapshotId
  readonly "workContract": WorkContract
  readonly "workItem": WorkItem
  readonly "workPlan"?: ReadonlyArray<WorkItem>
}

export type WorkRunRuntimeProjectionGetParameters = {
  readonly "atCursor": StrongFlowReadCursor
  readonly "deliveryId": DeliveryId
  readonly "kind": "work-run"
  readonly "productSessionId": ProductSessionId
  readonly "workRunId": WorkRunId
}

export type WorkRunStartCommand = {
  readonly "actor": Actor
  readonly "command": "workrun.start"
  readonly "expectedRevision": Revision
  readonly "payload": WorkRunStartPayload
  readonly "requestId": RequestId
  readonly "schemaVersion": SchemaVersion
  readonly "scope": RepositoryScope
}

export type WorkRunStartCompletedResponse = {
  readonly "command": "workrun.start"
  readonly "currentRevision": Revision
  readonly "outcome": "completed"
  readonly "previousRevision": Revision
  readonly "requestId": RequestId
  readonly "result": DeliveryProjection
  readonly "schemaVersion": SchemaVersion
}

export type WorkRunStartPayload = {
  readonly "deliveryId": DeliveryId
  readonly "dispatchProfile": "executor" | "reviewer" | "verifier" | "adversarial-verifier" | "remediator"
  readonly "rework"?: DeliveryReworkInput
}

export enum WorkRunState {
  Queued = "queued",
  Leased = "leased",
  Running = "running",
  CandidateReady = "candidate_ready",
  Settled = "settled",
  Failed = "failed",
  Cancelled = "cancelled",
}

export type WorkspaceId = string & { readonly __brand: "WorkspaceId" }

export type WorkspaceRevision = string

export type WorkspaceScope = {
  readonly "kind": "workspace"
  readonly "organizationId": OrganizationId
  readonly "workspaceId": WorkspaceId
}
