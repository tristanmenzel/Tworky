import type { BranchWork, PrDetail, Worker } from './api'

const CONTROL = /[\u0000-\u001f\u007f-\u009f]/g

/** Strips control characters so a pasted prompt can't submit itself. */
export function singleLine(text: string): string {
  return text.replace(CONTROL, ' ').replace(/ {2,}/g, ' ').trim()
}

/** `exited` is true once the app has seen the session end, which `session_alive` lags behind. */
export function sessionRunning(w: Worker, exited: boolean): boolean {
  return w.session_alive && !exited
}

export function askBlocker(w: Worker, exited: boolean): string | false {
  if (!sessionRunning(w, exited)) return w.status === 'ready' ? false : `${w.id} is ${w.status}`
  if (w.activity === 'waiting') return `${w.id} is waiting for you to answer Claude`
  return false
}

export type BranchWithoutPr = 'waiting' | 'no_commits' | 'deleted'

export function withoutPr(b: BranchWork): BranchWithoutPr | null {
  if (b.pull_requests.length) return null
  if (!b.local && !b.remote) return 'deleted'
  if (b.ahead === 0) return 'no_commits'
  return 'waiting'
}

function prName(b: BranchWork, pr: PrDetail): string {
  return `PR ${b.repo}#${pr.number} "${pr.title}" (branch ${pr.head})`
}

export function fixCiPrompt(b: BranchWork, pr: PrDetail): string {
  const checks = pr.checks.failing.map((c) => c.name).join(', ')
  return singleLine(
    `Fix the failing CI checks on ${prName(b, pr)}: ${checks}. Read the failed runs with gh, fix the cause, then commit and push.`,
  )
}

export function addressReviewPrompt(b: BranchWork, pr: PrDetail): string {
  const asks = [
    pr.changes_requested_by.length ? `changes requested by ${pr.changes_requested_by.join(', ')}` : null,
    pr.unresolved_threads ? `${pr.unresolved_threads} unresolved review thread(s)` : null,
  ].filter(Boolean)
  return singleLine(
    `Address the review on ${prName(b, pr)}: ${asks.join(' and ')}. Read the review comments with gh, make the changes, then commit and push, and tell me how you handled each comment.`,
  )
}

export function openPrPrompt(b: BranchWork): string {
  return singleLine(`Open a pull request for branch ${b.branch} in ${b.repo}`)
}
