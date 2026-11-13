<script lang="ts">
  import { openUrl } from '@tauri-apps/plugin-opener'
  import { api, type Worker } from './api'
  import { activityLabel, activityOf, confirmCleanup, sessionLabel } from './issue'
  import ActionMenu, { type MenuItem } from './ActionMenu.svelte'
  import WorkerDot from './WorkerDot.svelte'

  interface Props {
    worker: Worker
    accounts: string[]
    /** How many of the project's services have a port on this worker. */
    services: number
    removeItemDisabled: string | false
    onResume: () => void
    onTerminate: () => Promise<void>
    onOpenBrowser: (account: string) => void
    onOpenAnonymous: () => void
    onCreateProfile: () => void
    onError: (message: string) => void
  }

  let { worker, accounts, services, removeItemDisabled, onResume, onTerminate, onOpenBrowser, onOpenAnonymous, onCreateProfile, onError }: Props =
    $props()

  const noService = $derived(services ? false : 'The project has no service with a port')

  const browserItems = $derived<MenuItem[]>([
    { label: 'Anonymous', disabled: noService, onSelect: onOpenAnonymous },
    ...(accounts.length ? [{ separator: true } as const] : []),
    ...accounts.map((a) => ({ label: a, onSelect: () => onOpenBrowser(a) })),
    { separator: true },
    { label: 'Create', disabled: noService, onSelect: onCreateProfile },
  ])

  let checking = $state(false)

  async function act(fn: () => Promise<void>) {
    try {
      await fn()
    } catch (e) {
      onError(String(e))
    }
  }

  const cleanupItems = $derived<MenuItem[]>([
    { label: 'Clean up', onSelect: () => cleanup(false) },
    { label: 'Clean up and remove item', danger: true, disabled: removeItemDisabled, onSelect: () => cleanup(true) },
  ])

  async function cleanup(removeItem: boolean) {
    checking = true
    const prs = await api.prs(worker.id).catch(() => null)
    checking = false
    if (confirmCleanup(worker, prs, removeItem)) act(() => api.cleanup(worker.id, removeItem))
  }
</script>

<div class="toolbar">
  <WorkerDot {worker} />
  <strong>{worker.id}</strong>
  {#if worker.issue}
    {@const issue = worker.issue}
    <button class="link issue" title={issue.url} onclick={() => openUrl(issue.url)}>
      #{issue.number}
      {issue.title} ↗
    </button>
  {:else if worker.item}
    <span class="issue"><span class="tag">Item</span> {worker.item.title}</span>
  {/if}
  {#if activityOf(worker) && activityOf(worker) !== 'working'}
    {@const activity = activityOf(worker)!}
    <span class="attn {activity}">{activityLabel(activity)}</span>
  {/if}
  {#if worker.status !== 'ready' || worker.busy}
    <span class="status {worker.status}">{worker.busy ? `${worker.status}…` : worker.status}</span>
  {/if}
  <span class="buttons">
    {#if worker.session_alive}
      <button
        class="danger"
        disabled={worker.busy}
        title="Stop Claude, everything it started and the worker's containers (kept, not removed)"
        onclick={() => act(onTerminate)}>Terminate</button
      >
    {:else}
      <span class="muted">Claude not running</span>
      <button class="primary" disabled={worker.status !== 'ready'} onclick={onResume}>{sessionLabel(worker)}</button>
    {/if}
    {#if accounts.length || services}
      <ActionMenu label="Open browser" trigger="Open browser ▾" items={browserItems} />
    {/if}
    <button onclick={() => act(() => api.openDir(worker.id))}>Open folder</button>
    <ActionMenu
      label="Clean up"
      trigger={checking ? 'Checking PRs…' : 'Clean up ▾'}
      danger
      disabled={checking || worker.busy || worker.status === 'cleaning'}
      items={cleanupItems}
    />
  </span>
</div>

<style>
  .toolbar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 10px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
    min-width: 0;
  }
  .issue {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--muted);
  }
  .attn {
    flex: none;
    font-size: 12px;
    font-weight: 600;
    white-space: nowrap;
  }
  .attn.waiting {
    color: var(--bad);
  }
  .attn.idle {
    color: var(--warn);
  }
  .attn.uat {
    color: var(--accent);
  }
  .attn.account {
    color: var(--account);
  }
  .buttons {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-left: auto;
    flex: none;
  }
</style>
