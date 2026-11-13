<script lang="ts">
  import { untrack } from 'svelte'
  import { openUrl } from '@tauri-apps/plugin-opener'
  import type { Activity, Card, Issue, IssueState, Item, Job, LocalStatus, Verdict, Worker } from './api'
  import { ask } from './confirm.svelte'
  import {
    activityOf,
    boardStatusName,
    childrenOf,
    isMine,
    ownsItem,
    sessionLabel,
    shownBoards,
    typeClass,
  } from './issue'
  import { ExistingWorkLoader } from './existingWork.svelte'
  import { withoutPr } from './prs'
  import { running } from './running'
  import ActionMenu from './ActionMenu.svelte'
  import BoardSelect from './BoardSelect.svelte'
  import LocalWork from './LocalWork.svelte'
  import Markdown from './Markdown.svelte'
  import PullRequests from './PullRequests.svelte'
  import StatusSelect from './StatusSelect.svelte'
  import WorkerDot from './WorkerDot.svelte'

  interface Props {
    card: Card
    issues: Map<string, Issue>
    subtaskIds: Set<string>
    projects: Record<string, string>
    statusBoards: Record<string, number>
    viewer: string
    items: Item[]
    workers: Worker[]
    exited: Record<string, boolean>
    jobs: Record<string, Job[]>
    local: Record<string, LocalStatus>
    localFailed: Record<string, boolean>
    logs: Record<string, string[]>
    onBegin: (issueId: string) => void
    onAssign: (issueId: string) => Promise<void>
    onSetState: (issueId: string, state: IssueState) => Promise<void>
    onSetBoard: (issueId: string, projectId: string, optionId: string) => Promise<void>
    onOpenWorker: (worker: Worker) => void
    onRemove: (itemName: string) => void
    onError: (message: string) => void
  }

  let {
    card,
    issues,
    subtaskIds,
    projects,
    statusBoards,
    viewer,
    items,
    workers,
    exited,
    jobs,
    local,
    localFailed,
    logs,
    onBegin,
    onAssign,
    onSetState,
    onSetBoard,
    onOpenWorker,
    onRemove,
    onError,
  }: Props = $props()

  const DESCRIPTION_LINES = 12

  const ACTIVITY_PILLS: Record<Activity, { label: string; tone: string }> = {
    working: { label: 'Claude working', tone: 'ok' },
    idle: { label: 'Claude idle', tone: 'warn' },
    waiting: { label: 'Waiting for you', tone: 'bad' },
    uat: { label: 'Ready for UAT', tone: 'info' },
    account: { label: 'Needs account', tone: 'account' },
  }

  const VERDICT_PILLS: { verdict: Verdict; label: string; tone: string }[] = [
    { verdict: 'needs_attention', label: 'needs attention', tone: 'bad' },
    { verdict: 'ready', label: 'ready to merge', tone: 'ok' },
    { verdict: 'in_review', label: 'in review', tone: 'info' },
    { verdict: 'draft', label: 'draft', tone: 'muted' },
  ]

  let expanded = $state<Record<string, boolean>>({})
  let openParents = $state<Record<string, boolean>>({})
  let assigning = $state<string | null>(null)
  let showAll = $state(false)
  let overflowing = $state(false)
  let descEl = $state<HTMLDivElement>()
  let descInner = $state<HTMLDivElement>()
  let subtasksEl = $state<HTMLElement>()
  let localEl = $state<HTMLElement>()
  let prsEl = $state<HTMLElement>()

  const existing = new ExistingWorkLoader((message) => onError(message))

  const issue = $derived(issues.get(card.id)!)
  const ancestors = $derived(card.ancestors.map((id) => issues.get(id)).filter((i): i is Issue => !!i))
  const workersByIssue = $derived(new Map(workers.flatMap((w) => (w.issue ? [[w.issue.id, w] as const] : []))))
  const worker = $derived(workersByIssue.get(card.id))
  const project = $derived(projects[card.id])
  const subtasks = $derived(childrenOf(issues, card.id).filter((c) => subtaskIds.has(c.id)))
  const subtasksDone = $derived(subtasks.filter((s) => s.state !== 'OPEN').length)
  /** A subtask card's PRs usually hang off its siblings, so existing work is taken from the parent story. */
  const workScope = $derived(subtaskIds.has(card.id) && ancestors.length ? ancestors[0] : issue)
  const workScopeId = $derived(workScope.id)

  const activity = $derived(worker ? activityOf(worker) : null)
  const runningCount = $derived(worker ? running(local[worker.id] ?? null, jobs[worker.id] ?? []).count : 0)
  const verdictCounts = $derived.by(() => {
    const counts: Partial<Record<Verdict, number>> = {}
    for (const b of existing.work ?? []) {
      if (b.verdict && b.pull_requests.length) counts[b.verdict] = (counts[b.verdict] ?? 0) + 1
    }
    return counts
  })
  const waitingForPr = $derived((existing.work ?? []).filter((b) => withoutPr(b) === 'waiting').length)

  $effect(() => {
    const id = workScopeId
    untrack(() => existing.load({ kind: 'issue', id }, false))
  })

  $effect(() => {
    if (!descEl || !descInner) return
    const clamp = descEl
    const inner = descInner
    const observer = new ResizeObserver(() => {
      const lineHeight = parseFloat(getComputedStyle(clamp).lineHeight) || 19
      overflowing = inner.offsetHeight > lineHeight * DESCRIPTION_LINES + 1
    })
    observer.observe(inner)
    return () => observer.disconnect()
  })

  async function assign(id: string) {
    assigning = id
    try {
      await onAssign(id)
    } finally {
      assigning = null
    }
  }

  function boardsOf(i: Issue) {
    return shownBoards(i, statusBoards[projects[i.id]])
  }

  function statusOf(i: Issue): string {
    const board = boardsOf(i)
      .map(boardStatusName)
      .find((n) => n)
    if (board) return board
    if (i.state === 'OPEN') return 'Open'
    return i.state_reason === 'NOT_PLANNED' ? 'Not planned' : 'Closed'
  }

  function label(i: Issue): string {
    return `${i.issue_type ?? 'Issue'} #${i.number}`
  }

  function jump(el: HTMLElement | undefined) {
    el?.scrollIntoView({ behavior: 'smooth', block: 'start' })
  }
</script>

<div class="detail">
  <div class="column">
    <section class="panel card">
      <div class="head">
        {#if ancestors.length}
          <div class="crumbs">
            {#each [...ancestors].reverse() as a (a.id)}
              <button
                class="crumb"
                aria-expanded={!!openParents[a.id]}
                title={openParents[a.id] ? 'Hide details' : `Show the ${a.issue_type?.toLowerCase() ?? 'issue'}`}
                onclick={() => (openParents[a.id] = !openParents[a.id])}
              >
                <span class="chev" class:open={openParents[a.id]}>▸</span>
                <span class="tag {typeClass(a.issue_type)}">{a.issue_type ?? 'Issue'}</span>
                <span>#{a.number} {a.title}</span>
                {#if a.sub_issues_total}
                  <span class="muted">· {a.sub_issues_completed} of {a.sub_issues_total} done</span>
                {/if}
              </button>
              <span class="muted">›</span>
            {/each}
          </div>
          {#each [...ancestors].reverse().filter((a) => openParents[a.id]) as a (a.id)}
            <div class="parent">
              <div class="parent-meta">
                <span class="tag {typeClass(a.issue_type)}">{a.issue_type ?? 'Issue'}</span>
                <strong>{a.title}</strong>
                <span class="status">{statusOf(a)}</span>
                <button class="link muted" onclick={() => openUrl(a.url)}>{a.repo}#{a.number} ↗</button>
                <span class="muted">· {a.assignees.length ? `Assigned: ${a.assignees.join(', ')}` : 'Unassigned'}</span>
              </div>
              <Markdown source={a.body} />
            </div>
          {/each}
        {/if}

        <div class="title-row">
          <div class="title-main">
            <h1>{issue.title}</h1>
            <div class="meta">
              <span class="tag {typeClass(issue.issue_type)}">{issue.issue_type ?? 'Issue'}</span>
              <BoardSelect {issue} boards={boardsOf(issue)} onChange={onSetBoard} />
              <StatusSelect {issue} onChange={onSetState} />
              <button class="link muted" onclick={() => openUrl(issue.url)}>{issue.repo}#{issue.number} ↗</button>
              {#if project}<span class="tag proj">{project}</span>{/if}
              <span class="muted">
                · {issue.assignees.length ? `Assigned: ${issue.assignees.join(', ')}` : 'Unassigned'}
                {#if issue.state === 'UNKNOWN'}· GitHub no longer returns this issue (deleted, transferred or out of reach)
                {:else if !isMine(issue, viewer)}· not assigned to you{/if}
              </span>
            </div>
          </div>
          <div class="side">
            <div class="actions">
              {#if !worker}
                <button
                  class="primary"
                  disabled={!project}
                  title={project
                    ? `Start Claude in the context of this ${issue.issue_type?.toLowerCase() ?? 'issue'}`
                    : 'No project config matches this repo'}
                  onclick={() => onBegin(issue.id)}>Begin session</button
                >
              {/if}
              <ActionMenu
                items={[
                  {
                    label: 'Remove from list',
                    danger: true,
                    disabled: worker ? `Clean up worker ${worker.id} first` : false,
                    onSelect: async () => {
                      const ok = await ask({
                        title: `Take ${issue.repo}#${issue.number} off the list?`,
                        confirmLabel: 'Remove from list',
                        danger: true,
                      })
                      if (ok) onRemove(card.item)
                    },
                  },
                ]}
              />
            </div>
            <div class="glance">
              {#if activity}
                {@const pill = ACTIVITY_PILLS[activity]}
                <button class="pill {pill.tone}" onclick={() => jump(localEl)}>
                  <WorkerDot {worker} />{pill.label}
                </button>
              {/if}
              {#if runningCount}
                <button class="pill muted" onclick={() => jump(localEl)}>{runningCount} running</button>
              {/if}
              {#each VERDICT_PILLS as v (v.verdict)}
                {#if verdictCounts[v.verdict]}
                  <button class="pill {v.tone}" onclick={() => jump(prsEl)}>{verdictCounts[v.verdict]} {v.label}</button>
                {/if}
              {/each}
              {#if waitingForPr}
                <button class="pill warn" onclick={() => jump(prsEl)}>{waitingForPr} waiting for PR</button>
              {/if}
              {#if subtasks.length}
                <button class="pill {subtasksDone === subtasks.length ? 'ok' : 'muted'}" onclick={() => jump(subtasksEl)}
                  >{subtasksDone} of {subtasks.length} subtasks done</button
                >
              {/if}
            </div>
          </div>
        </div>
      </div>

      <div class="desc">
        <div class="clamp" class:clamped={!showAll} bind:this={descEl}>
          <div bind:this={descInner}><Markdown source={issue.body} /></div>
        </div>
        {#if overflowing}
          <button class="link more" onclick={() => (showAll = !showAll)}>{showAll ? 'Show less' : 'Show more'}</button>
        {/if}
      </div>

      {#if subtasks.length}
        <div class="subtasks" bind:this={subtasksEl}>
          <div class="sec-head">
            <h2>Subtasks</h2>
            <span class="muted">{subtasksDone} of {subtasks.length} done</span>
            <span class="progress"><span style:width="{(subtasksDone / subtasks.length) * 100}%"></span></span>
          </div>
          <ul>
            {#each subtasks as s (s.id)}
              {@const sw = workersByIssue.get(s.id)}
              {@const closed = s.state !== 'OPEN'}
              {@const mine = isMine(s, viewer)}
              <li class:closed>
                <div class="row">
                  <button
                    class="toggle"
                    aria-expanded={!!expanded[s.id]}
                    aria-label="Show details"
                    onclick={() => (expanded[s.id] = !expanded[s.id])}>{expanded[s.id] ? '▾' : '▸'}</button
                  >
                  <span class="tag {typeClass(s.issue_type)}">{s.issue_type ?? 'Issue'}</span>
                  <button class="link sub-title" onclick={() => (expanded[s.id] = !expanded[s.id])}
                    >#{s.number} {s.title}</button
                  >
                  <span class="muted who" class:mine>{s.assignees.length ? s.assignees.join(', ') : 'unassigned'}</span>
                  <span class="sub-actions">
                    {#if boardsOf(s).length}
                      <BoardSelect issue={s} boards={boardsOf(s)} onChange={onSetBoard} />
                    {:else}
                      <StatusSelect issue={s} onChange={onSetState} />
                    {/if}
                    {#if sw}
                      <WorkerDot worker={sw} title={sw.id} />
                      <button disabled={sw.status !== 'ready'} onclick={() => onOpenWorker(sw)}>{sessionLabel(sw)}</button>
                    {:else if mine && !closed}
                      <button
                        class="primary"
                        disabled={!projects[s.id]}
                        title={projects[s.id] ? '' : 'No project config matches this repo'}
                        onclick={() => onBegin(s.id)}>Begin session</button
                      >
                    {:else if !closed}
                      <button disabled={assigning === s.id} onclick={() => assign(s.id)}>
                        {assigning === s.id ? 'Assigning…' : 'Assign to me'}
                      </button>
                    {/if}
                  </span>
                </div>
                {#if expanded[s.id]}
                  <div class="sub-detail">
                    <div class="sub-meta">
                      <button class="link muted" onclick={() => openUrl(s.url)}>{s.repo}#{s.number} ↗</button>
                      {#if boardsOf(s).length}<StatusSelect issue={s} onChange={onSetState} />{/if}
                      {#if sw}<code class="muted">{sw.id}</code>{/if}
                    </div>
                    <Markdown source={s.body} />
                  </div>
                {/if}
              </li>
            {/each}
          </ul>
        </div>
      {/if}
    </section>

    {#if worker}
      <div bind:this={localEl}>
        <LocalWork
          {worker}
          jobs={jobs[worker.id] ?? []}
          local={local[worker.id] ?? null}
          localFailed={localFailed[worker.id] ?? false}
          log={logs[worker.id] ?? []}
          ownsItem={ownsItem(worker, items)}
          {onOpenWorker}
          {onError}
        />
      </div>
    {/if}
  </div>

  <div class="column prs" bind:this={prsEl}>
    {#if workScope.id !== issue.id}
      <div class="muted scope">Showing work for the parent {label(workScope)}, {workScope.title}</div>
    {/if}
    <PullRequests
      work={existing.work}
      loading={existing.loading}
      checkedAt={existing.checkedAt}
      subject={{ kind: 'issue', id: workScope.id }}
      {workers}
      {exited}
      onRefresh={(fetch) => existing.load({ kind: 'issue', id: workScope.id }, fetch)}
      {onOpenWorker}
      {onError}
    />
  </div>
</div>

<style>
  .detail {
    padding: 16px 24px 32px;
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 24px;
    align-items: start;
  }
  @container detail (min-width: 1100px) {
    .detail {
      grid-template-columns: minmax(0, 55fr) minmax(0, 45fr);
    }
  }
  .column {
    display: flex;
    flex-direction: column;
    gap: 24px;
    min-width: 0;
  }
  .panel {
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
  }
  .card {
    overflow: hidden;
  }
  .head {
    padding: 14px 16px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .crumbs {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px;
    font-size: 12px;
  }
  .crumb {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: none;
    border-color: transparent;
    padding: 2px 6px;
    margin-left: -6px;
    min-width: 0;
  }
  .chev {
    display: inline-block;
    color: var(--muted);
    font-size: 10px;
    transition: transform 0.15s;
  }
  .chev.open {
    transform: rotate(90deg);
  }
  .parent {
    background: var(--panel-2);
    border-radius: 6px;
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .parent-meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    font-size: 12px;
  }
  .parent-meta strong {
    font-size: 13px;
  }
  .title-row {
    display: flex;
    flex-wrap: wrap;
    gap: 12px 16px;
    align-items: flex-start;
    justify-content: space-between;
  }
  .title-main {
    flex: 1 1 360px;
    min-width: 0;
  }
  h1 {
    font-size: 20px;
    margin: 0 0 6px;
    font-weight: 600;
  }
  .meta {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    align-items: center;
    font-size: 12px;
  }
  .side {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 8px;
    max-width: 100%;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .glance {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    justify-content: flex-end;
  }
  .pill {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    border-radius: 999px;
    padding: 1px 9px;
    font-size: 12px;
    font-weight: 600;
    background: var(--panel-2);
    border: 1px solid var(--border);
    white-space: nowrap;
  }
  .pill.ok {
    color: var(--ok);
    border-color: var(--ok);
  }
  .pill.bad {
    color: var(--bad);
    border-color: var(--bad);
  }
  .pill.warn {
    color: var(--warn);
    border-color: var(--warn);
  }
  .pill.info {
    color: var(--accent);
    border-color: var(--accent);
  }
  .pill.account {
    color: var(--account);
    border-color: var(--account);
  }
  .pill.muted {
    color: var(--muted);
  }
  .link.muted {
    color: var(--muted);
  }
  .tag.proj {
    color: var(--accent);
    border-color: var(--accent);
  }
  .desc {
    padding: 12px 16px 14px;
    border-top: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .clamp.clamped {
    max-height: 12lh;
    overflow: hidden;
  }
  .more {
    align-self: flex-start;
    font-size: 12px;
  }
  h2 {
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--muted);
    margin: 0;
    font-weight: 600;
  }
  .subtasks {
    border-top: 1px solid var(--border);
    padding-top: 12px;
  }
  .sec-head {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 0 16px 8px;
    font-size: 12px;
  }
  .progress {
    flex: 1;
    max-width: 120px;
    height: 4px;
    background: var(--panel-2);
    border-radius: 2px;
    overflow: hidden;
  }
  .progress > span {
    display: block;
    height: 100%;
    background: var(--ok);
  }
  .subtasks ul {
    list-style: none;
    margin: 0;
    padding: 0 6px;
    border-top: 1px solid var(--border);
  }
  .subtasks li {
    border-top: 1px solid var(--border);
  }
  .subtasks li:first-child {
    border-top: 0;
  }
  .subtasks li.closed .row {
    opacity: 0.6;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 10px;
  }
  .toggle {
    background: none;
    border: 0;
    padding: 0;
    width: 14px;
    color: var(--muted);
    font-size: 11px;
  }
  .sub-title {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text);
  }
  .who {
    font-size: 12px;
    white-space: nowrap;
  }
  .who.mine {
    color: var(--text);
  }
  .sub-actions {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: none;
  }
  .sub-detail {
    padding: 4px 14px 12px 34px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .sub-meta {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 12px;
  }
  .column.prs {
    gap: 6px;
  }
  .scope {
    font-size: 12px;
  }
</style>
