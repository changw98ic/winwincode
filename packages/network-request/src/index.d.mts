// SPDX-License-Identifier: Apache-2.0
export type ErrorKind = 'connection_unavailable' | 'transport_interrupted' | 'timeout' | 'rate_limited' | 'server_transient' | 'authentication' | 'authorization' | 'request_invalid' | 'protocol_invalid' | 'integrity_invalid' | 'tls_invalid' | 'stream_incomplete' | 'empty_response' | 'cancelled' | 'authority_expired' | 'storage_unavailable'
export type Replay = 'retry_inference' | 'replay_exact' | 'reconcile_first'
export interface NetworkDiagnostic { readonly code: string; readonly field?: string; readonly ioKind?: string; readonly osCode?: number; readonly line?: number; readonly column?: number }
export interface NetworkAttempt { readonly attempt: number; readonly networkAttempt: number; readonly connectionWaits?: number; readonly outcome: string; readonly failure?: NetworkFailure }
export interface NetworkFailure { readonly kind: ErrorKind; readonly acceptance: 'not_sent' | 'unknown' | 'response_received'; readonly phase: string; readonly httpStatus: number | null; readonly retryAfterMs: number | null; readonly diagnostic?: NetworkDiagnostic }
export interface RequestOptions {
  readonly replay?: Replay
  readonly maxAttempts?: number
  readonly signal?: AbortSignal
  readonly canStart?: () => boolean
  readonly deadline?: number
  readonly jitter?: () => number
  readonly waitBeforeRetry?: (attempt: number, delayMs: number) => Promise<void>
  readonly beforeAttempt?: (fact: { attempt: number; networkAttempt: number; connectionAttempts: number }) => Promise<void> | void
  readonly onAttempt?: (fact: NetworkAttempt) => Promise<void> | void
}
export const policy: Readonly<{ version: string; maxAttempts: number; initialDelayMs: number; maxConnectionDelayMs: number; maxImmediateWaitMs: number; authorityCheckMs: number; jitterMs: number; transientKinds: readonly ErrorKind[] }>
export class NetworkError extends Error { readonly failure: NetworkFailure; readonly response?: unknown; readonly networkAttempts?: readonly NetworkAttempt[]; readonly networkStopReason?: string; constructor(failure: NetworkFailure, response?: unknown, options?: ErrorOptions) }
export function withResponseFailure<T extends object>(error: T, response: { readonly status: number; readonly networkError?: unknown }): T
export function retryAfter(value: string | undefined | null, now?: number): number | null
export function httpFailure(status: number, retryAfterMs?: number | null): NetworkFailure
export function classifyError(error: unknown, options?: { notSent?: boolean; phase?: string }): NetworkFailure
export function decide(failure: NetworkFailure, options?: RequestOptions & { attempt?: number; connectionAttempt?: number; jitterMs?: number }): { action: 'stop' | 'reconcile' | 'retry_after' | 'deferred_until'; delayMs?: number }
export function wait(delayMs: number, options?: RequestOptions): Promise<void>
export function executeRequest<T>(sendOnce: (context: { signal: AbortSignal; attempt: number; networkAttempt: number }) => Promise<T>, options?: RequestOptions): Promise<T>
export interface FetchResponse { readonly ok: boolean; readonly status: number; readonly headers?: { get(name: string): string | null }; readonly networkError?: NetworkError; text(): Promise<string> }
export function executeFetch<I, R extends FetchResponse>(fetcher: (input: string, init: I) => Promise<R>, input: string, init: I, options?: RequestOptions): Promise<FetchResponse>
export function requestFetch<I, R extends FetchResponse>(fetcher: (input: string, init: I) => Promise<R>, input: string, init: I, options?: RequestOptions): Promise<FetchResponse>
