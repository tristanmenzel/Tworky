<script lang="ts">
  import { openUrl } from '@tauri-apps/plugin-opener'
  import { api, type Conversation, type Worker } from './api'
  import { ask } from './confirm.svelte'
  import { activityLabel, activityOf, confirmCleanup, sessionLabel } from './issue'
  import ActionMenu, { type MenuItem } from './ActionMenu.svelte'
  import WorkerDot from './WorkerDot.svelte'
  import { redrawTerminal } from './terminals'

  interface Props {
    worker: Worker
    accounts: string[]
    /** How many of the project's services have a port on this worker. */
    services: number
    ownsItem: boolean
    onResume: () => void
    onTerminate: () => Promise<void>
    onOpenBrowser: (account: string) => void
    onOpenAnonymous: () => void
    onCreateProfile: () => void
    onSwitchConversation: (session: string) => Promise<void>
    onNewConversation: () => Promise<void>
    onError: (message: string) => void
  }

  let {
    worker,
    accounts,
    services,
    ownsItem,
    onResume,
    onTerminate,
    onOpenBrowser,
    onOpenAnonymous,
    onCreateProfile,
    onSwitchConversation,
    onNewConversation,
    onError,
  }: Props = $props()

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

  let conversations = $state<Conversation[] | null>(null)

  async function loadConversations() {
    conversations = null
    try {
      conversations = await api.listConversations(worker.id)
    } catch (e) {
      onError(String(e))
    }
  }

  const when = (date: Date) =>
    date.toLocaleString(undefined, { day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' })

  function conversationLabel(c: Conversation) {
    const span = [c.started && when(new Date(c.started)), when(new Date(c.last_active))].filter(Boolean).join(' – ')
    return `${c.current ? '● ' : ''}${span} · ${c.prompt ?? '(no prompt)'}`
  }

  const conversationItems = $derived<MenuItem[]>([
    ...(conversations
      ? conversations.map((c): MenuItem => ({
          label: conversationLabel(c),
          disabled: c.current ? 'Current conversation' : false,
          onSelect: () => switchConversation(c.id),
        }))
      : [{ label: 'Loading…', disabled: "Reading the worker's transcripts", onSelect: () => {} }]),
    { separator: true },
    { label: 'New conversation', onSelect: newConversation },
  ])

  const conversationsBlocked = $derived(
    worker.status !== 'ready' || worker.busy || (worker.session_alive && worker.activity === 'waiting'),
  )

  async function switchConversation(session: string) {
    if (
      worker.session_alive &&
      !(await ask({
        title: 'Switch to this conversation?',
        message: 'Claude stops and resumes the chosen conversation.',
        confirmLabel: 'Switch',
      }))
    )
      return
    act(() => onSwitchConversation(session))
  }

  async function newConversation() {
    if (
      !(await ask({
        title: 'Start a new conversation?',
        message: 'The current one ends.',
        confirmLabel: 'New conversation',
      }))
    )
      return
    act(onNewConversation)
  }

  const cleanupItems = $derived<MenuItem[]>([
    { label: 'Clean up', onSelect: () => cleanup(false) },
    ...(ownsItem ? [{ label: 'Clean up and remove item', danger: true, onSelect: () => cleanup(true) }] : []),
  ])

  async function cleanup(removeItem: boolean) {
    checking = true
    const prs = await api.prs(worker.id).catch(() => null)
    checking = false
    if (await confirmCleanup(worker, prs, { removeItem })) act(() => api.cleanup(worker.id, removeItem))
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
      <button
        title="Repaint the terminal if its screen looks garbled (Ctrl+Shift+L)"
        onclick={() => redrawTerminal(worker.id)}>Redraw</button
      >
    {:else}
      <span class="muted">Claude not running</span>
      <button class="primary" disabled={worker.status !== 'ready'} onclick={onResume}>{sessionLabel(worker)}</button>
    {/if}
    {#if accounts.length || services}
      <ActionMenu label="Open browser" trigger="Open browser ▾" items={browserItems} />
    {/if}
    <ActionMenu
      label="Conversations"
      trigger="Conversations ▾"
      disabled={conversationsBlocked}
      onOpen={loadConversations}
      items={conversationItems}
    />
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
