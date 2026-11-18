<script lang="ts">
  import { onMount, untrack } from 'svelte'
  import type { Activity, Worker, WorkSubject } from './api'
  import { ExistingWorkLoader } from './existingWork.svelte'
  import { formatAge } from './issue'
  import { hasPrContent } from './prs'
  import PullRequests from './PullRequests.svelte'

  interface Props {
    worker: Worker
    workers: Worker[]
    exited: Record<string, boolean>
    issuesRefreshed: number
    visible: boolean
    /** Width of the terminal pane, sidebar included. */
    paneWidth: number
    onOpenWorker: (worker: Worker) => void
    onError: (message: string) => void
  }

  let { worker, workers, exited, issuesRefreshed, visible, paneWidth, onOpenWorker, onError }: Props = $props()

  const SECTIONS = [{ id: 'prs', label: 'Pull requests' }] as const
  type SectionId = (typeof SECTIONS)[number]['id']

  const NARROW_PANE = 900
  const LEFT_WORKING: Activity[] = ['idle', 'uat', 'account']

  const workerId = untrack(() => worker.id)
  const collapsedKey = `tworky.sidebar.collapsed.${workerId}`
  const hadContentKey = `tworky.sidebar.hadContent.${workerId}`

  function readFlag(key: string): boolean | null {
    try {
      const value = localStorage.getItem(key)
      return value === null ? null : value === '1'
    } catch {
      return null
    }
  }

  function writeFlag(key: string, value: boolean) {
    try {
      localStorage.setItem(key, value ? '1' : '0')
    } catch {}
  }

  let collapsed = $state(readFlag(collapsedKey) ?? false)
  let hadContent = $state(readFlag(hadContentKey) ?? false)
  let openedWhileNarrow = $state(false)
  let section = $state<SectionId>('prs')
  let loadError = $state<string | null>(null)
  let stale = $state(false)

  const loader = new ExistingWorkLoader((message) => (loadError = message))

  const subject = $derived<WorkSubject | null>(
    worker.issue
      ? { kind: 'issue', id: worker.issue.id }
      : worker.item
        ? { kind: 'item', name: worker.item.name }
        : null,
  )
  const subjectKey = $derived(subject ? `${subject.kind}:${subject.kind === 'issue' ? subject.id : subject.name}` : '')

  const content = $derived<Record<SectionId, boolean>>({
    prs: loader.work ? hasPrContent(loader.work) : hadContent,
  })
  const sections = $derived(SECTIONS.filter((s) => content[s.id]))
  const active = $derived(sections.find((s) => s.id === section) ?? sections[0])
  const narrow = $derived(paneWidth > 0 && paneWidth < NARROW_PANE)
  const shownCollapsed = $derived(narrow ? !openedWhileNarrow : collapsed)

  let now = $state(Date.now())
  onMount(() => {
    const timer = setInterval(() => (now = Date.now()), 30_000)
    return () => clearInterval(timer)
  })

  function ago(ms: number): string {
    return `${formatAge(Math.max(0, Math.round((now - ms) / 1000)))} ago`
  }

  function load(fetch: boolean) {
    if (!subject) return
    stale = false
    loadError = null
    void loader.load(subject, fetch)
  }

  function requestLoad() {
    if (visible) load(false)
    else stale = true
  }

  $effect(() => {
    void subjectKey
    void issuesRefreshed
    untrack(requestLoad)
  })

  let lastVisible = untrack(() => visible)
  $effect(() => {
    const shown = visible
    if (shown && (!lastVisible || stale)) untrack(() => load(false))
    lastVisible = shown
  })

  let lastActivity = untrack(() => worker.activity)
  $effect(() => {
    const activity = worker.activity
    if (lastActivity === 'working' && activity && LEFT_WORKING.includes(activity)) untrack(requestLoad)
    lastActivity = activity
  })

  $effect(() => {
    if (!loader.work) return
    const has = hasPrContent(loader.work)
    hadContent = has
    writeFlag(hadContentKey, has)
  })

  $effect(() => {
    if (!narrow) openedWhileNarrow = false
  })

  function open(id: SectionId) {
    section = id
    if (narrow) openedWhileNarrow = true
    else setCollapsed(false)
  }

  function collapse() {
    if (narrow) openedWhileNarrow = false
    else setCollapsed(true)
  }

  function setCollapsed(value: boolean) {
    collapsed = value
    writeFlag(collapsedKey, value)
  }
</script>

{#if subject && active}
  {#if shownCollapsed}
    <aside class="bar">
      {#each sections as s (s.id)}
        <button class="bar-btn" title="Show {s.label.toLowerCase()}" onclick={() => open(s.id)}>{s.label}</button>
      {/each}
    </aside>
  {:else}
    <aside class="side">
      <div class="side-head">
        <div class="head-line">
          <h3>{active.label}</h3>
          {#if loader.checkedAt !== null}<span class="muted checked">checked {ago(loader.checkedAt)}</span>{/if}
          <span class="spacer"></span>
          <button
            class="small"
            disabled={loader.loading}
            title="Fetch the repos and refresh"
            onclick={() => load(true)}>{loader.loading ? 'Loading…' : 'Refresh'}</button
          >
          <button class="small collapse" title="Collapse" aria-label="Collapse" onclick={collapse}>−</button>
        </div>
        {#if loadError}<div class="load-error" title={loadError}>{loadError}</div>{/if}
      </div>
      <div class="side-body">
        {#if active.id === 'prs'}
          <PullRequests
            compact
            ownWorker={worker.id}
            work={loader.work}
            loading={loader.loading}
            checkedAt={loader.checkedAt}
            {subject}
            {workers}
            {exited}
            onRefresh={load}
            {onOpenWorker}
            {onError}
          />
        {/if}
      </div>
    </aside>
  {/if}
{/if}

<style>
  .side {
    width: 300px;
    flex: none;
    min-height: 0;
    display: flex;
    flex-direction: column;
    background: var(--panel);
    border-left: 1px solid var(--border);
  }
  .side-head {
    padding: 8px 12px;
    border-bottom: 1px solid var(--border);
  }
  .head-line {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  h3 {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
    margin: 0;
    font-weight: 600;
    white-space: nowrap;
  }
  .checked {
    font-size: 11.5px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .spacer {
    flex: 1;
  }
  button.small {
    padding: 1px 8px;
    font-size: 12px;
    flex: none;
  }
  button.collapse {
    font-weight: 700;
    line-height: 1;
    padding: 1px 7px;
  }
  .load-error {
    margin-top: 4px;
    font-size: 12px;
    color: var(--bad);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .side-body {
    flex: 1;
    min-height: 0;
    overflow: auto;
    padding: 10px;
  }
  .bar {
    width: 28px;
    flex: none;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    padding-top: 8px;
    background: var(--panel);
    border-left: 1px solid var(--border);
  }
  .bar-btn {
    flex: none;
    writing-mode: vertical-rl;
    transform: rotate(180deg);
    color: var(--muted);
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    padding: 10px 2px;
    border-radius: 4px;
  }
  .bar-btn:hover {
    color: var(--text);
  }
</style>
