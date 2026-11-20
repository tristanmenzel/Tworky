import type { Card, Issue, Item } from './api'

export type WorkListEntry = { kind: 'card'; card: Card; issue: Issue } | { kind: 'item'; item: Item }

export interface WorkListGroup {
  project: string
  multiRepo: boolean
  entries: WorkListEntry[]
}

export function workListGroups(
  cards: Card[],
  items: Item[],
  projects: Record<string, string>,
  issues: Map<string, Issue>,
): WorkListGroup[] {
  const byProject = new Map<string, WorkListEntry[]>()
  const add = (project: string, entry: WorkListEntry) => {
    const entries = byProject.get(project)
    if (entries) entries.push(entry)
    else byProject.set(project, [entry])
  }
  for (const card of cards) {
    const issue = issues.get(card.id)
    if (issue) add(projects[card.id] ?? '', { kind: 'card', card, issue })
  }
  for (const item of items) {
    if (item.source.kind !== 'issue') add(item.project, { kind: 'item', item })
  }
  return [...byProject]
    .map(([project, entries]) => ({ project, multiRepo: new Set(entries.flatMap(entryRepo)).size > 1, entries }))
    .sort((a, b) => {
      if (!a.project !== !b.project) return a.project ? -1 : 1
      return a.project.toLowerCase().localeCompare(b.project.toLowerCase()) || a.project.localeCompare(b.project)
    })
}

function entryRepo(entry: WorkListEntry): string[] {
  if (entry.kind === 'card') return [entry.issue.repo]
  return entry.item.source.kind === 'pull_request' ? [entry.item.source.repo] : []
}

export function entryId(entry: WorkListEntry): string {
  return entry.kind === 'card' ? entry.card.id : `item:${entry.item.name}`
}

export function repoLabeller(group: WorkListGroup): (entry: WorkListEntry) => string | null {
  if (!group.multiRepo) return () => null
  const repos = new Set(group.entries.flatMap(entryRepo))
  const reposNamed = new Map<string, number>()
  for (const repo of repos) reposNamed.set(shortName(repo), (reposNamed.get(shortName(repo)) ?? 0) + 1)
  return (entry) => {
    const [repo] = entryRepo(entry)
    if (repo === undefined) return null
    return reposNamed.get(shortName(repo))! > 1 ? repo : shortName(repo)
  }
}

const shortName = (repo: string) => repo.slice(repo.lastIndexOf('/') + 1)
