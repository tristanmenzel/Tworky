<script lang="ts">
  import { openUrl } from '@tauri-apps/plugin-opener'
  import { api, type Job, type LocalStatus, type LocalWorktree, type ServiceStatus, type Worker } from './api'
  import { activityOf, confirmCleanup, formatAge, formatBytes, jobLabel, sessionLabel } from './issue'
  import { running } from './running'
  import ActionMenu, { type MenuItem } from './ActionMenu.svelte'
  import WorkerDot from './WorkerDot.svelte'

  interface Props {
    worker: Worker
    jobs: Job[]
    local: LocalStatus | null
    localFailed: boolean
    log: string[]
    onOpenWorker: (worker: Worker) => void
    onError: (message: string) => void
  }

  let { worker, jobs, local, localFailed, log, onOpenWorker, onError }: Props = $props()

  let checking = $state(false)
  let stopping = $state<Record<string, boolean>>({})
  let logEl = $state<HTMLPreElement>()

  $effect(() => {
    void log.length
    if (logEl) logEl.scrollTop = logEl.scrollHeight
  })

  const active = $derived(running(local, jobs))
  const services = $derived(active.services)
  const serviceJobs = $derived(active.serviceJobs)
  const otherJobs = $derived(active.otherJobs)
  const containers = $derived(active.containers)
  const runningContainers = $derived(active.runningContainers)
  const anythingRunning = $derived(active.count > 0)
  const listeners = $derived(local?.listeners ?? [])

  const sessionText = $derived.by(() => {
    if (!worker.session_alive) return worker.claude_started ? 'Claude is not running' : 'No session yet'
    switch (activityOf(worker)) {
      case 'working':
        return 'Claude is working'
      case 'idle':
        return 'Claude is idle'
      case 'waiting':
        return 'Claude is waiting for you'
      case 'uat':
        return 'Ready for UAT'
      case 'account':
        return 'Claude needs an account'
      default:
        return 'Claude is running'
    }
  })

  const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`

  const stackText = $derived.by(() => {
    if (!local) return null
    if (local.stack_error) return 'stack status unknown'
    if (!local.stack) return null
    if (!containers.length) return 'stack not created'
    if (!runningContainers.length) return 'stack stopped'
    return runningContainers.length === containers.length ? 'stack up' : `stack ${runningContainers.length} of ${containers.length} up`
  })

  const sessionDetails = $derived(
    [
      services.length ? plural(services.length, 'service') : null,
      otherJobs.length ? plural(otherJobs.length, 'job') : null,
      stackText,
    ].filter(Boolean),
  )

  const worktrees = $derived<{ repo: string; branch: string; status: LocalWorktree | null }[]>(
    local
      ? local.worktrees.map((t) => ({ repo: t.repo, branch: t.branch, status: t }))
      : worker.worktrees.map((t) => ({ repo: t.repo, branch: t.branch, status: null })),
  )

  const menu = $derived<MenuItem[]>([
    { label: 'Open folder', onSelect: () => act(() => api.openDir(worker.id)) },
    {
      label: 'New conversation…',
      disabled: worker.claude_started ? false : 'No conversation has started yet',
      onSelect: () =>
        confirm('Start a new Claude conversation for this worker?') && act(() => api.newConversation(worker.id)),
    },
  ])

  async function act(fn: () => Promise<void>) {
    try {
      await fn()
    } catch (e) {
      onError(String(e))
    }
  }

  async function stop(key: string, fn: () => Promise<void>) {
    stopping[key] = true
    await act(fn)
    stopping[key] = false
  }

  async function cleanup() {
    checking = true
    const summary = await api.prs(worker.id).catch(() => null)
    checking = false
    if (confirmCleanup(worker, summary)) act(() => api.cleanup(worker.id))
  }

  function serviceAddress(s: ServiceStatus): string | null {
    return s.url?.replace(/^https?:\/\//, '').replace(/\/$/, '') ?? null
  }
</script>

<section class="local">
  <div class="head">
    <h2>Local work</h2>
    <code class="muted">{worker.id}</code>
    {#if worker.status !== 'ready' || worker.busy}
      <span class="status {worker.status}">{worker.busy ? `${worker.status}…` : worker.status}</span>
    {/if}
    <span class="spacer"></span>
    <button class="primary" disabled={worker.status !== 'ready'} onclick={() => onOpenWorker(worker)}>
      {sessionLabel(worker)}
    </button>
    {#if worker.session_alive || anythingRunning || !local}
      <button
        disabled={worker.busy}
        title="Stop Claude, everything it started and the worker's containers (kept, not removed)"
        onclick={() => act(() => api.stop(worker.id))}>Stop everything</button
      >
    {/if}
    <button class="danger" disabled={checking || worker.busy || worker.status === 'cleaning'} onclick={cleanup}>
      {checking ? 'Checking PRs…' : 'Clean up'}
    </button>
    <ActionMenu items={menu} label="More actions for {worker.id}" />
  </div>

  {#if worker.error}<div class="error">{worker.error}</div>{/if}

  <div class="panel">
    <div class="part session">
      <WorkerDot {worker} />
      <strong>{sessionText}</strong>
      {#if sessionDetails.length}<span class="muted">· {sessionDetails.join(' · ')}</span>{/if}
    </div>

    <div class="part">
      <div class="label">Worktrees</div>
      {#each worktrees as t (t.repo)}
        <div class="tree">
          <button
            class="icon"
            title="Open in VSCode"
            aria-label="Open {t.repo} in VSCode"
            onclick={() => act(() => api.openInVscode(worker.id, t.repo))}
          >
            <svg viewBox="0 0 24 24" width="14" height="14" aria-hidden="true">
              <path
                fill="#007ACC"
                d="M23.15 2.587L18.21.21a1.494 1.494 0 0 0-1.705.29l-9.46 8.63-4.12-3.128a.999.999 0 0 0-1.276.057L.327 7.261A1 1 0 0 0 .326 8.74L3.899 12 .326 15.26a1 1 0 0 0 .001 1.479L1.65 17.94a.999.999 0 0 0 1.276.057l4.12-3.128 9.46 8.63a1.492 1.492 0 0 0 1.704.29l4.942-2.377A1.5 1.5 0 0 0 24 20.06V3.939a1.5 1.5 0 0 0-.85-1.352zm-5.146 14.861L10.826 12l7.178-5.448v10.896z"
              />
            </svg>
          </button>
          <code class="repo">{t.repo}</code>
          <code class="muted branch">{t.branch}</code>
          {#if t.status?.error}
            <span class="bad">{t.status.error}</span>
          {:else if t.status && (t.status.dirty_files || t.status.unpushed)}
            {#if t.status.dirty_files}<span class="pill warn">{t.status.dirty_files} changed</span>{/if}
            {#if t.status.unpushed}<span class="pill warn">{t.status.unpushed} unpushed</span>{/if}
          {:else if t.status}
            <span class="pill ok">clean · pushed</span>
          {/if}
        </div>
      {:else}
        <div><span class="muted">None yet. Suggested branch </span><code>{worker.branch}</code></div>
      {/each}
    </div>

    <div class="part">
      <div class="label">Running</div>
      {#if anythingRunning || listeners.length || local?.stack_error}
        <div class="running">
          {#each services as s (s.name)}
            {@const job = s.pid === null ? undefined : serviceJobs.get(s.pid)}
            {@const address = serviceAddress(s)}
            <div class="what">
              <span class="dot {s.state === 'running' ? 'ready' : 'creating'}"></span>
              <strong>{s.name}</strong>
              <span class="muted">service</span>
              {#if s.url && address}<button class="link" onclick={() => openUrl(s.url!)}>{address}</button>{/if}
              {#if s.pid === null}
                <span class="muted outside">started outside Tworky</span>
              {:else if s.state === 'starting'}
                <span
                  class="warn outside"
                  title="Its process is running but nothing answers on port {s.port}: still starting, or the server under it stopped"
                  >starting or not listening</span
                >
              {/if}
            </div>
            <span class="muted">{job ? `up ${formatAge(job.age_secs)}` : ''}</span>
            <span class="muted">{job ? formatBytes(job.memory_bytes) : ''}</span>
            {#if s.pid !== null}
              <button
                class="small"
                disabled={stopping[`service:${s.name}`]}
                onclick={() => stop(`service:${s.name}`, () => api.stopService(worker.id, s.name))}>Stop</button
              >
            {:else}
              <span></span>
            {/if}
          {/each}
          {#each listeners as l (`${l.port}:${l.pid}`)}
            <div class="what">
              <span class="dot ready"></span>
              <strong>{l.name}</strong>
              <span class="muted">listening on {l.port}</span>
              {#if l.command}<code class="command" title={l.command}>{jobLabel(l.command)}</code>{/if}
            </div>
            <span class="muted">{l.pid === null ? '' : `pid ${l.pid}`}</span>
            <span></span>
            <span></span>
          {/each}
          {#each otherJobs as j (j.pid)}
            <div class="what">
              <span class="dot ready"></span>
              <code class="command" title={j.command}>{jobLabel(j.command)}</code>
              <span class="muted">job</span>
            </div>
            <span class="muted">up {formatAge(j.age_secs)}</span>
            <span class="muted">{formatBytes(j.memory_bytes)}</span>
            <button
              class="small"
              disabled={stopping[`job:${j.pid}`]}
              onclick={() => stop(`job:${j.pid}`, () => api.stopJob(worker.id, j.pid))}>Stop</button
            >
          {/each}
          {#if runningContainers.length}
            <div class="what">
              <span class="dot ready"></span>
              <strong>Stack</strong>
              <span class="muted names">{runningContainers.map((c) => c.service).join(', ')}</span>
            </div>
            <span></span>
            <span class="muted">{plural(runningContainers.length, 'container')}</span>
            <button class="small" disabled={stopping.stack} onclick={() => stop('stack', () => api.stopStack(worker.id))}
              >Stop</button
            >
          {/if}
          {#if local?.stack_error}
            <div class="what wide">
              <span class="dot failed"></span>
              <strong>Stack</strong>
              <span class="bad">{local.stack_error}</span>
            </div>
          {/if}
        </div>
      {:else if !local}
        <div class="muted">{localFailed ? "Couldn't check what's running" : 'Checking…'}</div>
      {:else}
        <div class="muted">Nothing running.</div>
      {/if}
    </div>

    <details>
      <summary class="muted"><span class="chev">▸</span> Details: folder, ports, stack</summary>
      <div class="kv">
        <span class="muted">Folder</span><code>{worker.dir}</code>
        <span class="muted">Stack</span><code>{worker.compose_project}</code>
        <span class="muted">Ports (slot {worker.slot})</span>
        <span class="ports">
          {#each Object.entries(worker.ports) as [name, port] (name)}
            <span><span class="muted">{name}</span> <code>{port}</code></span>
          {/each}
        </span>
        <span class="muted">Conversation</span><code>{worker.claude_session}</code>
      </div>
    </details>
  </div>

  {#if log.length}
    <pre class="log" bind:this={logEl}>{log.join('\n')}</pre>
  {/if}
</section>

<style>
  .local {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  h2 {
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--muted);
    margin: 0;
    font-weight: 600;
  }
  .spacer {
    flex: 1;
  }
  .panel {
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    overflow: hidden;
  }
  .part {
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
    border-top: 1px solid var(--border);
  }
  .part:first-child {
    border-top: 0;
  }
  .session {
    flex-direction: row;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }
  .label {
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--muted);
  }
  .tree {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 10px;
  }
  .repo {
    min-width: 110px;
  }
  .branch {
    overflow-wrap: anywhere;
  }
  .pill {
    display: inline-flex;
    align-items: center;
    border-radius: 999px;
    padding: 0 8px;
    font-size: 11px;
    font-weight: 600;
    border: 1px solid currentColor;
    white-space: nowrap;
  }
  .pill.ok {
    color: var(--ok);
  }
  .pill.warn {
    color: var(--warn);
  }
  .running {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto auto auto;
    gap: 6px 14px;
    align-items: center;
  }
  .what {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .what.wide {
    grid-column: 1 / -1;
  }
  .command,
  .names {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .outside {
    font-size: 12px;
    font-style: italic;
  }
  button.icon {
    display: inline-flex;
    align-items: center;
    padding: 2px 4px;
  }
  button.small {
    padding: 1px 8px;
    font-size: 12px;
  }
  details {
    border-top: 1px solid var(--border);
  }
  summary {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    cursor: pointer;
    list-style: none;
  }
  summary::-webkit-details-marker {
    display: none;
  }
  .chev {
    display: inline-block;
    transition: transform 0.15s;
  }
  details[open] .chev {
    transform: rotate(90deg);
  }
  .kv {
    display: grid;
    grid-template-columns: 110px minmax(0, 1fr);
    gap: 4px 12px;
    padding: 0 12px 12px 30px;
    font-size: 12px;
  }
  .kv code {
    overflow-wrap: anywhere;
  }
  .ports {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 14px;
  }
  .error,
  .bad {
    color: var(--bad);
  }
  .warn {
    color: var(--warn);
  }
  .ok {
    color: var(--ok);
  }
  .error {
    white-space: pre-wrap;
  }
  .log {
    margin: 0;
    max-height: 260px;
    overflow: auto;
    background: var(--term-bg);
    color: #cfd5dc;
    padding: 8px 10px;
    border-radius: 6px;
    white-space: pre-wrap;
  }
</style>
