// SPDX-License-Identifier: Apache-2.0
// Adapted from DeepSeek Harness ui-workspace tree.ts / rows/Rows.tsx
// Copyright (c) 2026 DeepSeek, MIT; see THIRD_PARTY_NOTICES.md.

import { useState } from 'react'
import type { ProductSessionProjection } from '../generated/contracts.js'
import { repositoryDisplayName } from '../display-labels.js'

export interface SessionBrowserProps {
  sessions: readonly ProductSessionProjection[]
  currentId: string | null
  error: string
  loading: boolean
  href(session: ProductSessionProjection): string
  update(session: ProductSessionProjection, title: string, archived: boolean): Promise<void>
  refresh(): void
}

const projectKey = (session: ProductSessionProjection) => session.repositoryId
const projectLabel = (session: ProductSessionProjection, browser: Window | null) => repositoryDisplayName(
  session.deviceContext?.repositoryName ?? '', session.deviceContext?.repositoryBindingId, browser,
)

/** DSH title/workspace substring matching and newest-first ordering. */
export function visibleSessions(sessions: readonly ProductSessionProjection[], query: string, project: string, archived: boolean): ProductSessionProjection[] {
  const q = query.trim().toLowerCase()
  return sessions.filter(session => Boolean(session.archived) === archived
    && (project === '' || projectKey(session) === project)
    && (session.title.toLowerCase().includes(q) || projectLabel(session, typeof window === 'undefined' ? null : window).toLowerCase().includes(q)))
    .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt) || a.id.localeCompare(b.id))
}

/** DSH row actions and selection, connected to the product's durable sessions. */
export function SessionBrowser(props: SessionBrowserProps) {
  const browser = typeof window === 'undefined' ? null : window
  const [query, setQuery] = useState('')
  const [project, setProject] = useState('')
  const [archived, setArchived] = useState(false)
  const [editing, setEditing] = useState<string | null>(null)
  const [title, setTitle] = useState('')
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  const projects = new Map<string, string>(props.sessions.map(session => [projectKey(session), projectLabel(session, browser)]))
  const rows = visibleSessions(props.sessions, query, project, archived)
  const update = async (session: ProductSessionProjection, nextTitle: string, nextArchived: boolean) => {
    setBusy(session.id); setError('')
    try { await props.update(session, nextTitle.trim(), nextArchived); setEditing(null) }
    catch { setError('保存失败，请刷新会话后重试。') }
    finally { setBusy(null) }
  }
  return <section className="wwc-session-browser" aria-label="历史对话">
    <header><strong>历史对话</strong><button type="button" aria-label="刷新对话" onClick={props.refresh} disabled={props.loading}>↻</button></header>
    <input type="search" aria-label="搜索对话" placeholder="搜索对话或项目" value={query} onChange={e => setQuery(e.target.value)} />
    <details className="wwc-session-filters">
      <summary>筛选{project && ` · ${projects.get(project) ?? '所选项目'}`}{archived && ' · 已归档'}</summary>
      <select aria-label="按项目查看对话" value={project} onChange={e => setProject(e.target.value)}>
      <option value="">全部项目</option>
      {[...projects].map(([id, name]) => <option key={id} value={id}>{name}</option>)}
      </select>
      <label className="wwc-session-archived"><input type="checkbox" checked={archived} onChange={e => setArchived(e.target.checked)} />已归档</label>
    </details>
    {(error || props.error) && <p role="alert">{error || props.error}</p>}
    {props.loading && props.sessions.length === 0 && <p role="status">正在读取对话…</p>}
    {!props.loading && rows.length === 0 && <p>没有符合条件的对话</p>}
    <ul className="wwc-sidebar-recent">
      {rows.map(node => <li key={node.id} className={`wwc-sidebar-recent-item ${node.id === props.currentId ? 'wwc-sidebar-recent-item-active' : ''}`} data-session-key={node.id}>
        {editing === node.id ? <form onSubmit={e => { e.preventDefault(); void update(node, title, Boolean(node.archived)) }}>
          <input aria-label="对话名称" value={title} maxLength={500} required autoFocus onChange={e => setTitle(e.target.value)} />
          <button disabled={busy !== null || !title.trim()}>保存</button><button type="button" onClick={() => setEditing(null)}>取消</button>
        </form> : <>
          <a className="wwc-sidebar-recent-item-link" href={props.href(node)} aria-current={node.id === props.currentId ? 'page' : undefined}>
            <span className="wwc-sidebar-recent-item-text">{node.title}</span><small>{projectLabel(node, browser)}{node.state === 'running' ? ' · 运行中' : ''}</small>
          </a>
          <details className="wwc-session-actions"><summary aria-label={`${node.title}的操作`}>⋯</summary>
            <button type="button" disabled={busy !== null} onClick={() => { setEditing(node.id); setTitle(node.title) }}>重命名</button>
            <button type="button" disabled={busy !== null} onClick={() => { void update(node, node.title, !node.archived) }}>{node.archived ? '恢复对话' : '归档'}</button>
          </details>
        </>}
      </li>)}
    </ul>
  </section>
}
