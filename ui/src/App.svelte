<script lang="ts">
  import { onMount, tick, untrack } from 'svelte'
  import { listen } from '@tauri-apps/api/event'
  import { openUrl } from '@tauri-apps/plugin-opener'
  import {
    api,
    type Issue,
    type IssueState,
    type IssuesPayload,
    type Item,
    type Activity,
    type AddOutcome,
    type BrowserTabView,
    type Handover,
    type AccountRequest,
    type Job,
    type LocalStatus,
    type ProjectSummary,
    type Worker,
  } from './api'
  import ActionMenu from './ActionMenu.svelte'
  import AddIssueDialog from './AddIssueDialog.svelte'
  import AddPullRequestDialog from './AddPullRequestDialog.svelte'
  import CardList from './CardList.svelte'
  import CardDetail from './CardDetail.svelte'
  import ItemDetail from './ItemDetail.svelte'
  import JobView from './JobView.svelte'
  import NewItemDialog from './NewItemDialog.svelte'
  import NewProjectDialog from './NewProjectDialog.svelte'
  import ProjectDetail from './ProjectDetail.svelte'
  import Terminal from './Terminal.svelte'
  import MemoryView from './MemoryView.svelte'
  import AccountsView from './AccountsView.svelte'
  import LocalWork from './LocalWork.svelte'
  import WorkerToolbar from './WorkerToolbar.svelte'
  import WorkerDot from './WorkerDot.svelte'
  import BrowserTab from './BrowserTab.svelte'
  import HandoverPanel from './HandoverPanel.svelte'
  import AccountRequestPanel from './AccountRequestPanel.svelte'
  import { overlay } from './overlay.svelte'
  import JobsMenu from './JobsMenu.svelte'
  import ServicePickerDialog from './ServicePickerDialog.svelte'
  import CreateProfileDialog from './CreateProfileDialog.svelte'
  import type { ServiceLink } from './ServiceList.svelte'
  import KeepAwakePill from './KeepAwakePill.svelte'
  import VersionBadge from './VersionBadge.svelte'
  import {
    activityLabel,
    activityOf,
    activityRank,
    formatBytes,
    jobLabel,
    serviceUrl,
    subjectLabel,
    subjectUrl,
  } from './issue'

  const POLL_MS = 5 * 60 * 1000
  const JOBS_POLL_MS = 10_000
  const ITEM_PREFIX = 'item:'

  interface JobTab {
    tab: string
    worker: Worker
    job: Job
  }

  type View = 'items' | 'workers' | 'projects'

  let view = $state<View>('items')
  let payload = $state<IssuesPayload | null>(null)
  let workers = $state<Worker[]>([])
  let selectedId = $state<string | null>(null)
  let flashId = $state<string | null>(null)
  let itemTab = $state<string>('overview')
  let workersTab = $state<string>('list')
  let projectTab = $state<'overview' | 'setup' | 'memory' | 'accounts'>('overview')
  /** Projects whose setup terminal is mounted. */
  let setupOpen = $state<string[]>([])
  let newProjectOpen = $state(false)
  let terminals = $state<string[]>([])
  let browserTabs = $state<BrowserTabView[]>([])
  let generations = $state<Record<string, number>>({})
  let exited = $state<Record<string, boolean>>({})
  let refreshKey = $state(0)
  const liveSessionCount = $derived(workers.filter((w) => w.session_alive && !exited[w.id]).length)
  let logs = $state<Record<string, string[]>>({})
  let jobs = $state<Record<string, Job[]>>({})
  let local = $state<Record<string, LocalStatus>>({})
  let localFailed = $state<Record<string, boolean>>({})
  const localRequests: Record<string, number> = {}
  const localPending = new Set<string>()
  let pollingLocal = false
  let loading = $state(false)
  let error = $state<string | null>(null)
  let items = $state<Item[]>([])
  let projects = $state<ProjectSummary[]>([])
  let anonymousFor = $state<Worker | null>(null)
  let createProfileFor = $state<{ worker: Worker; request: AccountRequest | null } | null>(null)
  let selectedProject = $state<string | null>(null)
  let addDialog = $state<'issue' | 'pull_request' | 'custom' | null>(null)
  let flashTimer: ReturnType<typeof setTimeout> | undefined

  const issues = $derived(new Map<string, Issue>((payload?.issues ?? []).map((i) => [i.id, i])))
  const cards = $derived(payload?.cards ?? [])
  const subtaskIds = $derived(new Set(payload?.subtasks ?? []))
  const workersByIssue = $derived(new Map(workers.flatMap((w) => (w.issue ? [[w.issue.id, w] as const] : []))))
  const workersByItem = $derived(new Map(workers.flatMap((w) => (w.item ? [[w.item.name, w] as const] : []))))
  const selectedCard = $derived(cards.find((c) => c.id === selectedId))
  /** Pull request and custom items; issue items show as cards. */
  const otherItems = $derived(items.filter((i) => i.source.kind !== 'issue'))
  const selectedItem = $derived(
    selectedId?.startsWith(ITEM_PREFIX) ? otherItems.find((i) => ITEM_PREFIX + i.name === selectedId) : undefined,
  )
  const project = $derived(projects.find((p) => p.name === selectedProject))

  /** The card each issue shows on: its own card, or its parent's card for a subtask listed in the overview. */
  const cardByIssue = $derived.by(() => {
    const m = new Map<string, string>()
    for (const c of cards) m.set(c.id, c.id)
    for (const c of cards) for (const i of issues.values()) if (i.parent_id === c.id && !m.has(i.id)) m.set(i.id, c.id)
    return m
  })

  function workItemOf(w: Worker): string | null {
    if (w.item) return otherItems.some((i) => i.name === w.item!.name) ? ITEM_PREFIX + w.item.name : null
    return w.issue ? (cardByIssue.get(w.issue.id) ?? null) : null
  }

  const activityByCard = $derived.by(() => {
    const m = new Map<string, Activity>()
    for (const w of workers) {
      const card = workItemOf(w)
      const activity = activityOf(w)
      if (card && activity && activityRank(activity) > activityRank(m.get(card) ?? null)) m.set(card, activity)
    }
    return m
  })
  const BADGE_ORDER: Activity[] = ['working', 'idle', 'uat', 'account', 'waiting']
  const cardActivityCounts = $derived.by(() => {
    const counts: Record<Activity, number> = { working: 0, idle: 0, uat: 0, account: 0, waiting: 0 }
    for (const w of workers) {
      const activity = activityOf(w)
      if (activity && workItemOf(w)) counts[activity]++
    }
    return counts
  })

  const hasTab = (w: Worker) => w.session_alive || terminals.includes(w.id)
  const selectedWorkerIds = $derived(
    workers.filter((w) => selectedId && workItemOf(w) === selectedId).map((w) => w.id),
  )
  const selectedWorkersKey = $derived(selectedWorkerIds.join(' '))
  const itemTerminals = $derived(workers.filter((w) => selectedId && workItemOf(w) === selectedId && hasTab(w)))
  const strayTerminals = $derived(workers.filter((w) => !workItemOf(w) && hasTab(w)))
  const jobTabs = (ws: Worker[]): JobTab[] =>
    ws.flatMap((w) => (jobs[w.id] ?? []).map((job) => ({ tab: `job:${w.id}:${job.pid}`, worker: w, job })))
  const jobEntry = (j: JobTab) => ({ tab: j.tab, command: j.job.command, memory_bytes: j.job.memory_bytes })
  const itemJobs = $derived(jobTabs(workers.filter((w) => selectedId && workItemOf(w) === selectedId)))
  const strayJobs = $derived(jobTabs(workers.filter((w) => !workItemOf(w))))
  const webTab = (t: BrowserTabView) => `web:${t.id}`
  const browserWorkItem = (t: BrowserTabView) => {
    const w = workerById(t.worker)
    return w ? workItemOf(w) : null
  }
  const itemBrowsers = $derived(browserTabs.filter((t) => selectedId && browserWorkItem(t) === selectedId))
  const strayBrowsers = $derived(browserTabs.filter((t) => !browserWorkItem(t)))
  const activeItemTab = $derived(
    itemTerminals.some((w) => w.id === itemTab) ||
      itemJobs.some((j) => j.tab === itemTab) ||
      itemBrowsers.some((t) => webTab(t) === itemTab)
      ? itemTab
      : 'overview',
  )
  const activeWorkersTab = $derived(
    strayTerminals.some((w) => w.id === workersTab) ||
      strayJobs.some((j) => j.tab === workersTab) ||
      strayBrowsers.some((t) => webTab(t) === workersTab)
      ? workersTab
      : 'list',
  )
  const shownBrowser = $derived(
    addDialog || newProjectOpen || overlay.open
      ? null
      : (browserTabs.find((t) => terminalVisible(webTab(t)) && !t.paused)?.id ?? null),
  )

  $effect(() => {
    void api.browserShow(shownBrowser).catch((e) => showError(String(e)))
  })
  const shownJob = $derived(
    view === 'items'
      ? itemJobs.find((j) => j.tab === activeItemTab)
      : view === 'workers'
        ? strayJobs.find((j) => j.tab === activeWorkersTab)
        : undefined,
  )

  function terminalVisible(id: string): boolean {
    return (
      (view === 'items' && activeItemTab === id) ||
      (view === 'workers' && activeWorkersTab === id) ||
      (view === 'projects' && projectTab === 'setup' && id === project?.setup_session)
    )
  }

  function showError(message: string) {
    error = message
  }

  async function loadIssues(refresh: boolean) {
    loading = true
    try {
      payload = await api.issues(refresh)
      if (payload.config_error) showError(`Config: ${payload.config_error}`)
      if (selectedId === null && payload.cards.length) selectedId = payload.cards[0].id
    } catch (e) {
      showError(String(e))
    } finally {
      loading = false
    }
  }

  async function loadWorkers() {
    try {
      const before = workers
      workers = await api.workers()
      const ids = new Set(workers.map((w) => w.id))
      terminals = terminals.filter((t) => ids.has(t))
      if (before.some((w) => w.issue && !ids.has(w.id))) loadIssues(false)
    } catch (e) {
      showError(String(e))
    }
  }

  async function loadJobs() {
    try {
      jobs = await api.jobs()
    } catch {
      // A failed process snapshot only leaves the job tabs as they were.
    }
  }

  async function loadLocal(ids: string[]) {
    await Promise.all(
      ids.map(async (id) => {
        const request = (localRequests[id] ?? 0) + 1
        localRequests[id] = request
        localPending.add(id)
        try {
          const status = await api.workerLocal(id)
          if (localRequests[id] === request) {
            local[id] = status
            localFailed[id] = false
          }
        } catch {
          if (localRequests[id] === request) localFailed[id] = true
        } finally {
          if (localRequests[id] === request) localPending.delete(id)
        }
      }),
    )
  }

  async function pollLocal() {
    if (pollingLocal || view !== 'items') return
    pollingLocal = true
    try {
      await loadLocal(selectedWorkerIds)
    } finally {
      pollingLocal = false
    }
  }

  $effect(() => {
    const ids = selectedWorkersKey ? selectedWorkersKey.split(' ') : []
    untrack(() => loadLocal(ids))
  })

  async function loadItems() {
    try {
      items = await api.items()
    } catch (e) {
      showError(String(e))
    }
  }

  async function loadProjects() {
    try {
      projects = await api.projects()
      if (!projects.some((p) => p.name === selectedProject)) selectedProject = projects[0]?.name ?? null
    } catch (e) {
      showError(String(e))
    }
  }

  async function createItem(
    title: string,
    description: string,
    project: string | null,
    repos: string[],
    projectName: string | null,
  ) {
    const outcome = await api.createItem(title, description, project, repos, projectName)
    if (outcome.outcome === 'added') await itemAdded(outcome)
  }

  async function itemAdded(outcome: AddOutcome & { outcome: 'added' }) {
    addDialog = null
    const { item, new_project } = outcome
    await loadItems()
    if (item.source.kind === 'issue') await loadIssues(true)
    if (new_project) {
      await loadProjects()
      openProjectSetup(new_project)
      return
    }
    view = 'items'
    goToWorkItem(item.source.kind === 'issue' ? item.source.id : ITEM_PREFIX + item.name)
  }

  /** Shows the project's setup terminal, starting or resuming its session if needed. */
  function openProjectSetup(name: string) {
    view = 'projects'
    selectedProject = name
    projectTab = 'setup'
    const id = setupSessionId(name)
    if (!setupOpen.includes(name)) setupOpen.push(name)
    else if (exited[id]) {
      exited[id] = false
      generations[id] = (generations[id] ?? 0) + 1
    }
  }

  const setupSessionId = (name: string) => projects.find((p) => p.name === name)?.setup_session ?? `setup@${name}`

  async function newSetupConversation(name: string) {
    if (!confirm(`Start a new setup conversation for ${name}? The current one ends.`)) return
    try {
      await api.newConversation(setupSessionId(name))
      exited[setupSessionId(name)] = true
      openProjectSetup(name)
    } catch (e) {
      showError(String(e))
    }
  }

  async function projectCreated(name: string) {
    newProjectOpen = false
    await loadProjects()
    openProjectSetup(name)
  }

  async function beginItem(name: string) {
    try {
      const w = await api.beginItem(name)
      logs[w.id] = []
      await loadWorkers()
      openTerminal(workerById(w.id) ?? w)
    } catch (e) {
      showError(String(e))
    }
  }

  async function linkItem(name: string, issueUrl: string) {
    try {
      await api.linkItem(name, issueUrl)
      selectedId = null
      await Promise.all([loadItems(), loadWorkers(), loadIssues(true)])
    } catch (e) {
      showError(String(e))
      throw e
    }
  }

  async function removeItem(name: string) {
    try {
      await api.removeItem(name)
      selectedId = null
      await loadItems()
      await loadIssues(false)
    } catch (e) {
      showError(String(e))
    }
  }

  function showView(v: View) {
    view = v
    if (v === 'projects') loadProjects()
  }

  const allWorkersKey = $derived(workers.map((w) => w.id).join(' '))
  let workersViewOpen = false

  $effect(() => {
    const ids = allWorkersKey ? allWorkersKey.split(' ') : []
    if (view !== 'workers') {
      workersViewOpen = false
      return
    }
    untrack(() => {
      const opening = !workersViewOpen
      workersViewOpen = true
      ensureLogs(ids)
      loadLocal(opening ? ids : ids.filter((id) => !(id in local) && !localPending.has(id)))
    })
  })

  async function ensureLogs(ids: string[]) {
    for (const id of ids) {
      if (!logs[id]) logs[id] = await api.workerLog(id)
    }
  }

  function select(id: string) {
    selectedId = id
    const live = workers.find((w) => workItemOf(w) === id && w.session_alive && !exited[w.id])
    if (live && !terminals.includes(live.id)) terminals.push(live.id)
    itemTab = live?.id ?? 'overview'
    ensureLogs(workers.filter((w) => workItemOf(w) === id).map((w) => w.id))
  }

  async function goToWorkItem(id: string) {
    view = 'items'
    if (selectedId !== id) select(id)
    clearTimeout(flashTimer)
    flashId = null
    await tick()
    flashId = id
    flashTimer = setTimeout(() => (flashId = null), 1600)
    document.querySelector(`[data-card="${CSS.escape(id)}"]`)?.scrollIntoView({ block: 'center', behavior: 'smooth' })
  }

  async function begin(issueId: string) {
    try {
      const w = await api.begin(issueId)
      logs[w.id] = []
      await loadWorkers()
      openTerminal(workerById(w.id) ?? w)
    } catch (e) {
      showError(String(e))
    }
  }

  async function assign(issueId: string) {
    try {
      payload = await api.assign(issueId)
    } catch (e) {
      showError(String(e))
    }
  }

  async function setBoardStatus(issueId: string, projectId: string, optionId: string) {
    try {
      payload = await api.setBoardStatus(issueId, projectId, optionId)
    } catch (e) {
      showError(String(e))
      throw e
    }
  }

  async function setIssueState(issueId: string, state: IssueState) {
    try {
      payload = await api.setIssueState(issueId, state)
    } catch (e) {
      showError(String(e))
      throw e
    }
  }

  function mountTerminal(w: Worker) {
    if (!terminals.includes(w.id)) terminals.push(w.id)
    if (exited[w.id] || !w.session_alive) {
      exited[w.id] = false
      generations[w.id] = (generations[w.id] ?? 0) + 1
    }
  }

  /** Starts or resumes the session if needed, then shows its tab on its work item, or under Workers if it has none. */
  function openTerminal(w: Worker) {
    mountTerminal(w)
    const workItem = workItemOf(w)
    if (workItem) {
      view = 'items'
      if (selectedId !== workItem) select(workItem)
      itemTab = w.id
    } else {
      view = 'workers'
      workersTab = w.id
    }
  }

  function showTerminal(w: Worker) {
    if (!terminals.includes(w.id)) terminals.push(w.id)
    if (view === 'items') itemTab = w.id
    else workersTab = w.id
  }

  function showProjectWorker(w: Worker) {
    const workItem = workItemOf(w)
    if (workItem) goToWorkItem(workItem)
    else {
      view = 'workers'
      workersTab = 'list'
    }
  }

  function accountsOf(w: Worker): string[] {
    return projects.find((p) => p.name === w.project)?.accounts.map((a) => a.name) ?? []
  }

  function servicesOf(w: Worker): ServiceLink[] {
    return (projects.find((p) => p.name === w.project)?.services ?? []).flatMap((s) => {
      const url = serviceUrl(s, w)
      return url ? [{ name: s.name, url }] : []
    })
  }

  function openAnonymous(w: Worker) {
    const services = servicesOf(w)
    if (services.length === 1) void openBrowser(w, null, services[0].url)
    else anonymousFor = w
  }

  async function openBrowser(w: Worker, account: string | null, url: string | null = null) {
    try {
      const id = await api.browserOpen(w.id, account, url)
      browserTabs = await api.browserTabs()
      const tab = `web:${id}`
      if (view === 'items' && workItemOf(w) === selectedId) itemTab = tab
      else if (view === 'workers') workersTab = tab
    } catch (e) {
      showError(String(e))
    }
  }

  function showBrowser(id: number) {
    const t = browserTabs.find((t) => t.id === id)
    if (!t) return
    const item = browserWorkItem(t)
    if (item) {
      view = 'items'
      if (selectedId !== item) select(item)
      itemTab = webTab(t)
    } else {
      view = 'workers'
      workersTab = webTab(t)
    }
  }

  function closeBrowser(id: number) {
    void api.browserClose(id).catch((e) => showError(String(e)))
  }

  function closeTerminal(id: string) {
    terminals = terminals.filter((t) => t !== id)
  }

  function workerById(id: string) {
    return workers.find((w) => w.id === id)
  }

  onMount(() => {
    loadIssues(false)
    loadWorkers()
    loadItems()
    loadProjects()
    loadJobs()
    api.browserTabs().then((tabs) => (browserTabs = tabs), showError)
    const timer = setInterval(() => loadIssues(true), POLL_MS)
    const jobsTimer = setInterval(() => {
      if (document.hidden) return
      loadJobs()
      pollLocal()
    }, JOBS_POLL_MS)
    const unlisten = [
      listen<string>('worker-changed', (e) => {
        loadWorkers()
        loadJobs()
        if (view === 'workers' || selectedWorkerIds.includes(e.payload)) loadLocal([e.payload])
      }),
      listen<{ id: string; line: string }>('worker-log', (e) => {
        logs[e.payload.id] ??= []
        logs[e.payload.id].push(e.payload.line)
      }),
      listen<{ id: string; activity: Activity | null; handover: Handover | null; account_requests: AccountRequest[] }>(
        'session-activity',
        (e) => {
          const w = workerById(e.payload.id)
          if (w) {
            w.activity = e.payload.activity
            w.handover = e.payload.handover
            w.account_requests = e.payload.account_requests
          }
        },
      ),
      listen<string>('session-exited', (e) => {
        exited[e.payload] = true
        if (e.payload.startsWith('setup@')) loadProjects()
      }),
      listen<BrowserTabView[]>('browser-tabs-changed', (e) => {
        browserTabs = e.payload
      }),
    ]
    return () => {
      clearInterval(timer)
      clearInterval(jobsTimer)
      unlisten.forEach((p) => p.then((f) => f()))
    }
  })
</script>

{#snippet terminalTab(w: Worker, active: boolean)}
  <div class="tab" class:active>
    <button onclick={() => showTerminal(w)}>
      <WorkerDot worker={w} />
      {w.id}
    </button>
    {#if !w.session_alive}
      <button class="close" aria-label="Close tab" onclick={() => closeTerminal(w.id)}>×</button>
    {/if}
  </div>
{/snippet}

{#snippet handover(w: Worker)}
  <HandoverPanel
    worker={w}
    handover={w.handover!}
    tabs={browserTabs.filter((t) => t.worker === w.id)}
    onShowTab={showBrowser}
    onOpenWorker={openTerminal}
    onError={showError}
  />
{/snippet}

{#snippet accountRequests(w: Worker)}
  <AccountRequestPanel
    worker={w}
    onCreate={(request) => (createProfileFor = { worker: w, request })}
    onError={showError}
  />
{/snippet}

{#snippet browserTab(t: BrowserTabView, active: boolean, show: () => void)}
  <div class="tab" class:active>
    <button onclick={show} title={`${t.worker} · ${t.url}`}>
      <span class="web" style:color={t.color ?? undefined}>🌐</span>
      {t.account ?? 'Anonymous'}
    </button>
    <button class="close" aria-label="Close tab" onclick={() => closeBrowser(t.id)}>×</button>
  </div>
{/snippet}

{#snippet jobTab(j: JobTab, active: boolean, show: () => void)}
  <div class="tab" class:active>
    <button onclick={show} title={j.job.command}>
      <span class="run">▶</span>
      {jobLabel(j.job.command)}
      <span class="muted mem">{formatBytes(j.job.memory_bytes)}</span>
    </button>
  </div>
{/snippet}

<div class="app">
  <header class="topnav">
    <strong class="brand">Tworky</strong>
    <nav>
      <button class:active={view === 'items'} onclick={() => showView('items')}
        >Work Items
        {#each BADGE_ORDER as activity (activity)}
          {#if cardActivityCounts[activity]}
            <span class="count {activity}" title={`${activityLabel(activity)}: ${cardActivityCounts[activity]}`}
              >{cardActivityCounts[activity]}</span
            >
          {/if}
        {/each}
      </button>
      <button class:active={view === 'workers'} onclick={() => showView('workers')}>Workers ({workers.length})</button>
      <button class:active={view === 'projects'} onclick={() => showView('projects')}>Projects</button>
    </nav>
    <span class="right">
      <KeepAwakePill />
      <VersionBadge liveSessions={liveSessionCount} {refreshKey} />
      <span class="muted viewer">{payload?.viewer ?? ''}</span>
    </span>
  </header>

  {#if error}
    <div class="error-bar" role="alert">
      <pre>{error}</pre>
      <button onclick={() => (error = null)}>Dismiss</button>
    </div>
  {/if}

  <div class="shell" class:wide={view === 'workers'}>
    {#if view === 'items'}
      <aside>
        <div class="side-head">
          <strong>My work</strong>
          <span class="spacer"></span>
          <ActionMenu
            label="Add to the work list"
            trigger="+"
            items={[
              { label: 'From issue…', onSelect: () => (addDialog = 'issue') },
              { label: 'From pull request…', onSelect: () => (addDialog = 'pull_request') },
              { label: 'Custom…', onSelect: () => (addDialog = 'custom') },
            ]}
          />
          <button
            disabled={loading}
            onclick={() => {
              refreshKey++
              loadIssues(true)
            }}
            title="Refresh from GitHub"
            >{loading ? '…' : '↻'}</button
          >
        </div>
        <div class="list">
          {#if payload}
            <CardList
              {cards}
              {issues}
              items={otherItems}
              projects={payload.projects}
              statusBoards={payload.status_boards}
              workers={workersByIssue}
              itemWorkers={workersByItem}
              {activityByCard}
              {selectedId}
              {flashId}
              onSelect={select}
            />
          {:else if loading}
            <p class="muted pad">Loading issues…</p>
          {/if}
        </div>
      </aside>
    {:else if view === 'projects'}
      <aside>
        <div class="side-head">
          <strong>Projects</strong>
          <span class="spacer"></span>
          <button onclick={() => (newProjectOpen = true)} title="New project, set up by a Claude session">+</button>
          <button onclick={loadProjects} title="Reload project config">↻</button>
        </div>
        <ul class="projects">
          {#each projects as p (p.name)}
            {@const n = workers.filter((w) => w.project === p.name).length}
            <li>
              <button
                class="project"
                class:selected={selectedProject === p.name}
                onclick={() => (selectedProject = p.name)}
              >
                <strong>{p.name}</strong>
                {#if p.onboarding}<span class="setup-badge" title="Not set up yet">setup</span>{/if}
                <span class="muted">{p.repos.map((r) => r.name).join(', ')}</span>
                {#if n}<span class="worker-count" title="Workers">{n}</span>{/if}
              </button>
            </li>
          {:else}
            <li class="muted pad">No projects configured.</li>
          {/each}
        </ul>
      </aside>
    {/if}

    <main>
      <nav class="tabs">
        {#if view === 'items'}
          <button class:active={activeItemTab === 'overview'} onclick={() => (itemTab = 'overview')}>Overview</button>
          {#each itemTerminals as w (w.id)}
            {@render terminalTab(w, activeItemTab === w.id)}
          {/each}
          {#if itemJobs.length === 1}
            {@render jobTab(itemJobs[0], activeItemTab === itemJobs[0].tab, () => (itemTab = itemJobs[0].tab))}
          {:else if itemJobs.length > 1}
            <JobsMenu jobs={itemJobs.map(jobEntry)} current={activeItemTab} onSelect={(t) => (itemTab = t)} />
          {/if}
          {#each itemBrowsers as t (t.id)}
            {@render browserTab(t, activeItemTab === webTab(t), () => (itemTab = webTab(t)))}
          {/each}
        {:else if view === 'workers'}
          <button class:active={activeWorkersTab === 'list'} onclick={() => (workersTab = 'list')}>All workers</button>
          {#each strayTerminals as w (w.id)}
            {@render terminalTab(w, activeWorkersTab === w.id)}
          {/each}
          {#if strayJobs.length === 1}
            {@render jobTab(strayJobs[0], activeWorkersTab === strayJobs[0].tab, () => (workersTab = strayJobs[0].tab))}
          {:else if strayJobs.length > 1}
            <JobsMenu jobs={strayJobs.map(jobEntry)} current={activeWorkersTab} onSelect={(t) => (workersTab = t)} />
          {/if}
          {#each strayBrowsers as t (t.id)}
            {@render browserTab(t, activeWorkersTab === webTab(t), () => (workersTab = webTab(t)))}
          {/each}
        {:else}
          <button class:active={projectTab === 'overview'} onclick={() => (projectTab = 'overview')}>Overview</button>
          <button class:active={projectTab === 'setup'} onclick={() => (projectTab = 'setup')}>Setup</button>
          <button class:active={projectTab === 'memory'} onclick={() => (projectTab = 'memory')}>Memory</button>
          <button class:active={projectTab === 'accounts'} onclick={() => (projectTab = 'accounts')}>Accounts</button>
        {/if}
      </nav>

      <div class="content">
        <div class="overview detail" class:hidden={view !== 'items' || activeItemTab !== 'overview'}>
          {#each workers.filter((w) => w.account_requests.length && selectedId && workItemOf(w) === selectedId) as w (w.id)}
            {@render accountRequests(w)}
          {/each}
          {#each workers.filter((w) => w.handover && selectedId && workItemOf(w) === selectedId) as w (w.id)}
            {@render handover(w)}
          {/each}
          {#if selectedItem}
            {#key selectedItem.name}
              <ItemDetail
                item={selectedItem}
                worker={workersByItem.get(selectedItem.name)}
                {workers}
                {exited}
                jobs={jobs[workersByItem.get(selectedItem.name)?.id ?? ''] ?? []}
                local={local[workersByItem.get(selectedItem.name)?.id ?? ''] ?? null}
                localFailed={localFailed[workersByItem.get(selectedItem.name)?.id ?? ''] ?? false}
                logs={logs[workersByItem.get(selectedItem.name)?.id ?? ''] ?? []}
                onBegin={beginItem}
                onLink={linkItem}
                onRemove={removeItem}
                onOpenWorker={openTerminal}
                onError={showError}
              />
            {/key}
          {:else if selectedCard && issues.has(selectedCard.id)}
            {#key selectedCard.id}
              <CardDetail
                card={selectedCard}
                {issues}
                {subtaskIds}
                projects={payload?.projects ?? {}}
                statusBoards={payload?.status_boards ?? {}}
                viewer={payload?.viewer ?? ''}
                {workers}
                {exited}
                {jobs}
                {local}
                {localFailed}
                {logs}
                onBegin={begin}
                onAssign={assign}
                onSetState={setIssueState}
                onSetBoard={setBoardStatus}
                onOpenWorker={openTerminal}
                onRemove={removeItem}
                onError={showError}
              />
            {/key}
          {:else}
            <div class="empty muted">Select a work item.</div>
          {/if}
        </div>

        <div class="overview" class:hidden={view !== 'workers' || activeWorkersTab !== 'list'}>
          {#each workers.filter((w) => w.handover && !workItemOf(w)) as w (w.id)}
            {@render handover(w)}
          {/each}
          <div class="worker-list">
            {#each workers as w (w.id)}
              {@const url = subjectUrl(w)}
              {@const workItem = workItemOf(w)}
              <section>
                <div class="worker-head">
                  {#if url}
                    <button class="link muted" onclick={() => openUrl(url)}>{subjectLabel(w)} ↗</button>
                  {:else}
                    <span class="muted small-label">{subjectLabel(w)}</span>
                  {/if}
                  <span class="spacer"></span>
                  <button
                    disabled={!workItem}
                    title={workItem ? undefined : 'Not on any current work item (unassigned or closed since)'}
                    onclick={() => workItem && goToWorkItem(workItem)}>Go to work item</button
                  >
                </div>
                <LocalWork
                  worker={w}
                  jobs={jobs[w.id] ?? []}
                  local={local[w.id] ?? null}
                  localFailed={localFailed[w.id] ?? false}
                  log={logs[w.id] ?? []}
                  onOpenWorker={openTerminal}
                  onError={showError}
                />
              </section>
            {:else}
              <div class="empty muted">No workers.</div>
            {/each}
          </div>
        </div>

        <div class="overview" class:hidden={view !== 'projects' || projectTab !== 'overview'}>
          {#if project}
            {#key project.name}
              <ProjectDetail
                {project}
                workers={workers.filter((w) => w.project === project.name)}
                items={items.filter((i) => i.project === project.name)}
                onShowWorker={showProjectWorker}
                onOpenSetup={() => openProjectSetup(project.name)}
                onError={showError}
              />
            {/key}
          {:else}
            <div class="empty muted">Select a project.</div>
          {/if}
        </div>

        <div
          class="overview"
          class:hidden={view !== 'projects' || projectTab !== 'setup' || !project || setupOpen.includes(project.name)}
        >
          {#if project}
            <div class="setup-intro">
              <h2>Set up {project.name} with Claude</h2>
              <p class="muted">
                A Claude session that edits this project's config and templates. It reads the repos' docs, asks you
                about anything they leave open, and tests the result on a scratch item. Reopen it whenever the config
                needs changing.
              </p>
              <button class="primary" onclick={() => openProjectSetup(project.name)}>
                {project.setup_alive ? 'Show session' : project.setup_started ? 'Resume session' : 'Start session'}
              </button>
            </div>
          {/if}
        </div>

        <div class="overview" class:hidden={view !== 'projects' || projectTab !== 'memory'}>
          {#if project}
            {#key project.name}
              <MemoryView
                project={project.name}
                visible={view === 'projects' && projectTab === 'memory'}
                onError={showError}
              />
            {/key}
          {/if}
        </div>

        <div class="overview" class:hidden={view !== 'projects' || projectTab !== 'accounts'}>
          {#if project}
            {#key project.name}
              <AccountsView
                project={project.name}
                services={project.services}
                visible={view === 'projects' && projectTab === 'accounts'}
                onChanged={loadProjects}
                onError={showError}
              />
            {/key}
          {/if}
        </div>

        {#if shownJob}
          {#key shownJob.tab}
            <JobView worker={shownJob.worker} job={shownJob.job} onError={showError} />
          {/key}
        {/if}

        {#each browserTabs as t (t.id)}
          <BrowserTab tab={t} visible={terminalVisible(webTab(t))} onError={showError} />
        {/each}

        {#each setupOpen as name (name)}
          {@const id = setupSessionId(name)}
          <div class="term-wrap" class:hidden={!terminalVisible(id)}>
            <div class="setup-bar">
              <strong>Setup: {name}</strong>
              <span class="spacer"></span>
              {#if exited[id]}
                <button class="primary" onclick={() => openProjectSetup(name)}>Resume</button>
              {:else}
                <button onclick={() => api.stop(id).catch((e) => showError(String(e)))}>Stop</button>
              {/if}
              <button onclick={() => newSetupConversation(name)}>New conversation</button>
              <button onclick={() => api.openProjectConfig(name).catch((e) => showError(String(e)))}>Open config</button>
            </div>
            <div class="term-host">
              <Terminal workerId={id} visible={terminalVisible(id)} generation={generations[id] ?? 0} onError={showError} />
            </div>
          </div>
        {/each}

        {#each terminals as id (id)}
          {@const w = workerById(id)}
          <div class="term-wrap" class:hidden={!terminalVisible(id)}>
            {#if w}
              <WorkerToolbar
                worker={w}
                accounts={accountsOf(w)}
                services={servicesOf(w).length}
                onResume={() => mountTerminal({ ...w, session_alive: false })}
                onOpenBrowser={(account) => openBrowser(w, account)}
                onOpenAnonymous={() => openAnonymous(w)}
                onCreateProfile={() => (createProfileFor = { worker: w, request: null })}
                onError={showError}
              />
              {#if w.account_requests.length}
                <div class="term-requests">{@render accountRequests(w)}</div>
              {/if}
            {/if}
            <div class="term-host">
              <Terminal workerId={id} visible={terminalVisible(id)} generation={generations[id] ?? 0} onError={showError} />
            </div>
          </div>
        {/each}
      </div>
    </main>
  </div>

  {#if anonymousFor}
    {@const w = anonymousFor}
    <ServicePickerDialog
      services={servicesOf(w)}
      onPick={(s) => {
        anonymousFor = null
        void openBrowser(w, null, s.url)
      }}
      onClose={() => (anonymousFor = null)}
    />
  {/if}

  {#if createProfileFor}
    {@const w = createProfileFor.worker}
    {@const request = createProfileFor.request}
    <CreateProfileDialog
      project={w.project}
      services={servicesOf(w)}
      initialName={request?.name}
      initialEmail={request?.username}
      initialService={request?.service}
      onCreated={async (name) => {
        createProfileFor = null
        if (request) await api.dismissAccountRequest(w.id, request.name).catch((e) => showError(String(e)))
        await loadProjects()
        await openBrowser(w, name)
      }}
      onClose={() => (createProfileFor = null)}
    />
  {/if}

  {#if addDialog === 'custom'}
    <NewItemDialog onCreate={createItem} onClose={() => (addDialog = null)} />
  {:else if addDialog === 'issue'}
    <AddIssueDialog onAdded={itemAdded} onClose={() => (addDialog = null)} />
  {:else if addDialog === 'pull_request'}
    <AddPullRequestDialog onAdded={itemAdded} onClose={() => (addDialog = null)} />
  {/if}

  {#if newProjectOpen}
    <NewProjectDialog onCreated={projectCreated} onClose={() => (newProjectOpen = false)} />
  {/if}
</div>

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100%;
  }
  .topnav {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 0 12px;
    height: 40px;
    flex: none;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .brand {
    font-size: 14px;
  }
  .topnav nav {
    display: flex;
    gap: 2px;
    height: 100%;
  }
  .topnav nav button {
    background: none;
    border: 0;
    border-bottom: 2px solid transparent;
    border-radius: 0;
    padding: 0 12px;
    color: var(--muted);
  }
  .topnav nav button:hover {
    color: var(--text);
  }
  .topnav nav button.active {
    color: var(--text);
    border-bottom-color: var(--accent);
    font-weight: 600;
  }
  .count {
    display: inline-block;
    min-width: 16px;
    margin-left: 4px;
    padding: 0 6px;
    border-radius: 8px;
    font-size: 11px;
    font-weight: 600;
    line-height: 16px;
    text-align: center;
  }
  .count.working {
    background: var(--ok-fill);
    color: var(--on-ok);
  }
  .count.idle {
    background: var(--warn);
    color: var(--on-warn);
  }
  .count.uat {
    background: var(--accent);
    color: var(--on-accent);
  }
  .count.account {
    background: var(--account);
    color: var(--on-account);
  }
  .count.waiting {
    background: var(--bad);
    color: var(--on-bad);
  }
  .right {
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    gap: 12px;
  }
  .viewer {
    font-size: 12px;
  }
  .shell {
    display: grid;
    grid-template-columns: minmax(320px, 400px) 1fr;
    flex: 1;
    min-height: 0;
  }
  .shell.wide {
    grid-template-columns: 1fr;
  }
  .spacer {
    flex: 1;
  }
  aside {
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--border);
    background: var(--panel);
    min-height: 0;
  }
  .side-head {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 12px;
    border-bottom: 1px solid var(--border);
  }
  .list {
    flex: 1;
    overflow: auto;
  }
  .pad {
    padding: 12px;
  }
  .projects {
    list-style: none;
    margin: 0;
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    overflow: auto;
  }
  .project {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    text-align: left;
    background: var(--bg);
    border: 1px solid var(--border);
    border-left: 3px solid transparent;
    border-radius: 8px;
    padding: 8px 10px;
  }
  .project:hover {
    border-color: var(--accent);
  }
  .project.selected {
    border-color: var(--accent);
    border-left-color: var(--accent);
    background: var(--panel-2);
  }
  .project .muted {
    flex: 1;
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .setup-badge {
    flex: none;
    font-size: 10px;
    padding: 0 6px;
    border-radius: 8px;
    color: var(--warn);
    border: 1px solid color-mix(in srgb, var(--warn) 55%, transparent);
  }
  .setup-intro {
    padding: 24px;
    max-width: 640px;
  }
  .setup-intro h2 {
    margin: 0 0 8px;
    font-size: 18px;
  }
  .setup-bar {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 10px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .setup-bar .spacer {
    flex: 1;
  }
  .worker-count {
    display: inline-block;
    min-width: 16px;
    margin-left: 4px;
    font-weight: 600;
    line-height: 16px;
    text-align: center;
    flex: none;
    font-size: 11px;
    padding: 0 6px;
    border-radius: 8px;
    background: var(--panel-2);
    color: var(--accent);
  }
  main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
  }
  .tabs {
    display: flex;
    gap: 2px;
    padding: 6px 8px 0;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
    overflow-x: auto;
  }
  .tabs > button,
  .tab {
    border-radius: 6px 6px 0 0;
    border: 1px solid transparent;
    border-bottom: 0;
    background: none;
  }
  .tab {
    display: flex;
    align-items: center;
  }
  .tab button {
    background: none;
    border: 0;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .tab .run {
    color: var(--muted);
    font-size: 10px;
  }
  .tab .mem {
    font-size: 11px;
  }
  .tab .close {
    padding: 0 8px 0 0;
    color: var(--muted);
  }
  .tabs > button.active,
  .tab.active {
    background: var(--bg);
    border-color: var(--border);
  }
  .content {
    position: relative;
    flex: 1;
    min-height: 0;
  }
  .overview {
    position: absolute;
    inset: 0;
    overflow: auto;
  }
  .detail {
    container: detail / inline-size;
  }
  .term-wrap {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
  }
  .term-requests {
    padding: 8px 10px 0;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .term-host {
    position: relative;
    flex: 1;
    min-height: 0;
  }
  .worker-list {
    padding: 16px 24px 32px;
    max-width: 980px;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  .worker-list section {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .worker-head {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .worker-list .link.muted,
  .small-label {
    color: var(--muted);
    font-size: 12px;
  }
  .hidden {
    visibility: hidden;
    pointer-events: none;
  }
  .empty {
    padding: 48px;
    text-align: center;
  }
  .error-bar {
    flex: none;
    background: var(--panel);
    border-bottom: 1px solid var(--bad);
    padding: 8px 12px;
    display: flex;
    gap: 12px;
    align-items: flex-start;
  }
  .error-bar pre {
    margin: 0;
    white-space: pre-wrap;
    color: var(--bad);
    max-height: 200px;
    overflow: auto;
    flex: 1;
  }
</style>
