// SPDX-License-Identifier: Apache-2.0

/**
 * Design shell sidebar 「最近对话」: a small browser-local list of chat
 * session titles, written by the Chat page when a session is opened, read by
 * the shell for the sidebar. Titles only — no business payload.
 */

export interface RecentChatEntry {
  readonly sessionKey: string
  readonly title: string
  readonly at: number
}

export const RECENT_CHATS_STORAGE_KEY = 'winwincode.recentChats.v1'
export const RECENT_CHATS_LIMIT = 5

export type RecentChatsStorage = Pick<Storage, 'getItem' | 'setItem'>

export function loadRecentChats(storage: RecentChatsStorage | null): readonly RecentChatEntry[] {
  if (storage === null) return []
  try {
    const raw = storage.getItem(RECENT_CHATS_STORAGE_KEY)
    if (raw === null) return []
    const parsed: unknown = JSON.parse(raw)
    if (!Array.isArray(parsed)) return []
    return parsed
      .filter((entry): entry is RecentChatEntry => (
        typeof entry === 'object' && entry !== null
        && typeof (entry as RecentChatEntry).sessionKey === 'string'
        && typeof (entry as RecentChatEntry).title === 'string'
        && typeof (entry as RecentChatEntry).at === 'number'
      ))
      .sort((left, right) => right.at - left.at)
      .slice(0, RECENT_CHATS_LIMIT)
  } catch {
    return []
  }
}

export function recordRecentChat(
  storage: RecentChatsStorage | null,
  entry: RecentChatEntry,
): void {
  if (storage === null) return
  const next = [
    entry,
    ...loadRecentChats(storage).filter(candidate => candidate.sessionKey !== entry.sessionKey),
  ].slice(0, RECENT_CHATS_LIMIT)
  try {
    storage.setItem(RECENT_CHATS_STORAGE_KEY, JSON.stringify(next))
    if (typeof window !== 'undefined' && typeof window.dispatchEvent === 'function') {
      window.dispatchEvent(new CustomEvent('wwc:recent-chats-changed'))
    }
  } catch {
    /* storage full or unavailable — the sidebar list is best-effort. */
  }
}
