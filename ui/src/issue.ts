import type { Activity, BoardStatus, Card, Issue, IssueState, Item, PrSummary, Service, Worker } from './api'

export function typeClass(type: string | null): string {
  const t = (type ?? '').toLowerCase()
  if (t === 'epic') return 'type-epic'
  if (t === 'story' || t === 'feature') return 'type-story'
  return t ? 'type-task' : ''
}

export function isMine(issue: Issue, viewer: string): boolean {
  return issue.assignees.some((a) => a.toLowerCase() === viewer.toLowerCase())
}

export function childrenOf(issues: Map<string, Issue>, id: string): Issue[] {
  return [...issues.values()]
    .filter((i) => i.parent_id === id)
    .sort((a, b) => a.repo.localeCompare(b.repo) || a.number - b.number)
}

export function subjectLabel(w: Worker): string {
  if (w.issue) return `${w.issue.repo}#${w.issue.number} ${w.issue.title}`
  if (w.pr) return `${w.pr.repo} PR #${w.pr.number} ${w.pr.title}`
  if (w.item) return `${w.item.name}: ${w.item.title}`
  return w.id
}

export function subjectUrl(w: Worker): string | null {
  return w.issue?.url ?? w.pr?.url ?? null
}

export function sessionLabel(w: Worker): string {
  return w.session_alive ? 'Show terminal' : w.claude_started ? 'Resume session' : 'Open session'
}

/** The project's configured board, or every board when none is configured or the issue isn't on it. */
export function shownBoards(issue: Issue, statusBoard: number | undefined): BoardStatus[] {
  const chosen = issue.boards.filter((b) => b.number === statusBoard)
  return chosen.length ? chosen : issue.boards
}

export function boardStatusName(b: BoardStatus): string | undefined {
  return b.options.find((o) => o.id === b.option_id)?.name
}

export function isDone(issue: Issue, statusBoard: number | undefined): boolean {
  if (issue.state !== 'OPEN') return true
  return shownBoards(issue, statusBoard).some((b) => boardStatusName(b)?.toLowerCase() === 'done')
}

/** The card's issue is closed or Done on its board, so it can probably come off the list. */
export function cardComplete(card: Card, issues: Map<string, Issue>, statusBoard: (i: Issue) => number | undefined) {
  const issue = issues.get(card.id)
  return !!issue && issue.state !== 'UNKNOWN' && isDone(issue, statusBoard(issue))
}

export function boardTone(name: string | undefined): string {
  const n = (name ?? '').toLowerCase()
  if (n === 'done') return 'done'
  if (n.includes('progress') || n.includes('review')) return 'doing'
  return 'todo'
}

/** `null` for a close reason the dropdown can't set, such as DUPLICATE. */
export function issueState(issue: Issue): IssueState | null {
  if (issue.state === 'OPEN') return 'open'
  if (issue.state_reason === 'NOT_PLANNED') return 'not_planned'
  if (issue.state_reason === 'COMPLETED' || !issue.state_reason) return 'completed'
  return null
}

export function confirmCleanup(w: Worker, prs: PrSummary | null, removeItem = false): boolean {
  const also = removeItem ? ' Its item is also taken off the work list.' : ''
  return confirm(
    prs?.all_merged
      ? `Clean up ${w.id}? This stops the session, removes the stack (including volumes), worktrees and branches Tworky created.${also}`
      : `${w.id} has no confirmation that all PRs are merged${prs ? '' : ' (PR status not checked)'}. Unpushed work in its worktrees will be lost.${also} Clean up anyway?`,
  )
}

export function ownsItem(w: Worker, items: Item[]): boolean {
  return items.some((i) => i.name === w.item?.name || (i.source.kind === 'issue' && i.source.id === w.issue?.id))
}

const SHELL_WORDS = new Set(['sh', 'bash', 'zsh', 'cmd', '/d', '/s', '/c', '-c'])

/** A tab-sized name for a job: the last step of the command, without its shell, paths or redirects. */
export function jobLabel(command: string): string {
  const steps = command
    .split(/&&|\|\||;/)
    .map((s) => s.trim())
    .filter((s) => s && !s.startsWith('cd '))
  const step = (steps.pop() ?? command).replace(/\s+\d?>.*$/, '')
  const words = (step.match(/"[^"]*"|\S+/g) ?? []).map((w) => {
    const bare = w.replace(/"/g, '')
    return /^\/[^/\\]*$/.test(bare) ? bare : (bare.split(/[\\/]/).pop() ?? bare)
  })
  while (words.length > 1 && SHELL_WORDS.has(words[0].toLowerCase().replace(/\.exe$/, ''))) words.shift()
  const label = words.join(' ')
  return label.length > 32 ? `${label.slice(0, 31)}…` : label
}

export function formatBytes(bytes: number): string {
  const mb = bytes / 2 ** 20
  return mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${Math.round(mb)} MB`
}

export function formatAge(secs: number): string {
  const d = Math.floor(secs / 86400)
  const h = Math.floor((secs % 86400) / 3600)
  const m = Math.floor((secs % 3600) / 60)
  if (d) return `${d}d ${h}h`
  if (h) return `${h}h ${m}m`
  if (m) return `${m}m`
  return `${secs}s`
}

export function activityOf(w: Worker): Activity | null {
  return w.session_alive ? w.activity : null
}

export function activityLabel(a: Activity): string {
  return {
    working: 'Working',
    idle: 'Idle: ready for your next prompt',
    waiting: 'Waiting for you',
    uat: 'Ready for UAT',
    account: 'Needs account',
  }[a]
}

const ACTIVITY_RANK: Record<Activity, number> = { idle: 1, working: 2, uat: 3, account: 4, waiting: 5 }

export function activityRank(a: Activity | null): number {
  return a ? ACTIVITY_RANK[a] : 0
}

export function serviceUrl(service: Service, w: Worker): string | null {
  const port = service.port === null ? undefined : w.ports[service.port]
  return port === undefined ? null : `${service.https ? 'https' : 'http'}://localhost:${port}/`
}
