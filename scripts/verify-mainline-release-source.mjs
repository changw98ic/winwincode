#!/usr/bin/env node

import { pathToFileURL } from 'node:url'

const COMMIT_PATTERN = /^[0-9a-f]{40}$/u
const REPOSITORY_PATTERN = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/u
const BRANCH_PATTERN = /^[A-Za-z0-9._/-]+$/u
const MAINLINE_WORKFLOW_PATH = '.github/workflows/mainline.yml'

function fail(message) {
  throw new Error(`MAINLINE_SOURCE_INVALID: ${message}`)
}

function validateDefaultBranch(defaultBranch) {
  if (typeof defaultBranch !== 'string'
    || defaultBranch.length === 0
    || defaultBranch.length > 255
    || !BRANCH_PATTERN.test(defaultBranch)
    || defaultBranch.startsWith('.')
    || defaultBranch.startsWith('/')
    || defaultBranch.endsWith('.')
    || defaultBranch.endsWith('/')
    || defaultBranch.includes('..')
    || defaultBranch.includes('//')) {
    fail('default branch identity is invalid')
  }
}

export function selectSuccessfulMainlineRun({ defaultBranch, repository, sourceCommit, runs }) {
  if (!COMMIT_PATTERN.test(sourceCommit ?? '')) {
    fail('source commit must be exactly 40 lowercase hexadecimal characters')
  }
  if (!REPOSITORY_PATTERN.test(repository ?? '')) fail('repository identity is invalid')
  validateDefaultBranch(defaultBranch)
  if (!Array.isArray(runs)) fail('GitHub workflow runs response is invalid')
  const matches = runs.filter(run => (
    run?.head_sha === sourceCommit
    && run?.event === 'push'
    && run?.status === 'completed'
    && run?.conclusion === 'success'
    && run?.path === MAINLINE_WORKFLOW_PATH
    && run?.head_repository?.full_name === repository
    && run?.head_branch === defaultBranch
  ))
  if (matches.length !== 1) {
    fail(`expected one successful same-repository mainline push for ${sourceCommit}; found ${matches.length}`)
  }
  const run = matches[0]
  if (!Number.isSafeInteger(run.id) || run.id <= 0 || typeof run.html_url !== 'string') {
    fail('matching workflow run identity is invalid')
  }
  return Object.freeze({
    runId: run.id,
    runUrl: run.html_url,
    defaultBranch,
    sourceCommit,
  })
}

function parseArguments(argv) {
  if (argv.length !== 4 || argv[0] !== '--source-commit' || argv[2] !== '--default-branch') {
    fail(
      'usage: verify-mainline-release-source.mjs '
      + '--source-commit COMMIT --default-branch BRANCH',
    )
  }
  return { sourceCommit: argv[1], defaultBranch: argv[3] }
}

async function workflowRuns({ defaultBranch, repository, token }) {
  if (typeof token !== 'string' || token.length === 0) fail('GH_TOKEN is required')
  const runs = []
  for (let page = 1; ; page += 1) {
    if (page > 100) fail('GitHub workflow run pagination exceeded the bounded limit')
    const url = new URL(
      `https://api.github.com/repos/${repository}/actions/workflows/mainline.yml/runs`,
    )
    url.searchParams.set('event', 'push')
    url.searchParams.set('status', 'success')
    url.searchParams.set('branch', defaultBranch)
    url.searchParams.set('per_page', '100')
    url.searchParams.set('page', String(page))
    const response = await fetch(url, {
      headers: {
        Accept: 'application/vnd.github+json',
        Authorization: `Bearer ${token}`,
        'User-Agent': 'winwincode-release-source-verifier',
        'X-GitHub-Api-Version': '2022-11-28',
      },
    })
    if (!response.ok) fail(`GitHub workflow runs request failed with HTTP ${response.status}`)
    const body = await response.json()
    if (!Array.isArray(body?.workflow_runs)) fail('GitHub workflow runs response is invalid')
    runs.push(...body.workflow_runs)
    if (body.workflow_runs.length < 100) return runs
  }
}

async function main() {
  const { defaultBranch, sourceCommit } = parseArguments(process.argv.slice(2))
  const repository = process.env.GITHUB_REPOSITORY
  const runs = await workflowRuns({ defaultBranch, repository, token: process.env.GH_TOKEN })
  const result = selectSuccessfulMainlineRun({ defaultBranch, repository, sourceCommit, runs })
  process.stdout.write(`${JSON.stringify({ status: 'passed', ...result })}\n`)
}

const isMain = process.argv[1] !== undefined
  && pathToFileURL(process.argv[1]).href === import.meta.url
if (isMain) await main()
