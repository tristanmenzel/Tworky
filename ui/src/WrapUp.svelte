<script lang="ts">
  import { untrack } from 'svelte'
  import { api, type PrSummary, type Worker } from './api'
  import { ask } from './confirm.svelte'
  import { confirmCleanup, prState } from './issue'

  interface Props {
    worker: Worker | undefined
    ownsItem: boolean
    scratch?: boolean
    label: string
    title: string
    onRemove: () => void
    onError: (message: string) => void
  }

  let { worker, ownsItem, scratch = false, label, title, onRemove, onError }: Props = $props()

  let summary = $state<PrSummary | null>(null)
  let loaded = $state(false)
  let checking = $state(false)
  let request = 0

  const workerId = $derived(worker?.id)
  const worktreeKey = $derived(worker?.worktrees.map((t) => `${t.repo}:${t.branch}`).join(',') ?? '')
  const pr = $derived(prState(summary, worker?.worktrees.length ?? 0))
  const disabled = $derived(!worker || checking || worker.busy || worker.status === 'cleaning')

  const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`

  $effect(() => {
    const id = workerId
    void worktreeKey
    if (id) untrack(() => load(id))
  })

  async function load(id: string): Promise<PrSummary | null> {
    const n = ++request
    loaded = false
    const result = await api.prs(id).catch(() => null)
    if (n === request) {
      summary = result
      loaded = true
    }
    return result
  }

  async function cleanup(removeItem: boolean) {
    if (!worker) return
    const w = worker
    checking = true
    const result = await load(w.id)
    checking = false
    if (!(await confirmCleanup(w, result, { removeItem, scratch }))) return
    try {
      await api.cleanup(w.id, removeItem)
    } catch (e) {
      onError(String(e))
    }
  }

  async function remove() {
    const ok = await ask({
      title: `Remove ${label} from the list?`,
      message: title,
      confirmLabel: 'Remove',
      danger: true,
    })
    if (ok) onRemove()
  }
</script>

<section class="panel box">
  <h2>Wrap up</h2>
  {#if !worker}
    <div class="row">
      <span class="muted grow">Take this item off the list.</span>
      <button class="danger outline" onclick={remove}>Remove from list</button>
    </div>
  {:else}
    <div class="prline">
      {#if !loaded}
        <span class="muted">Checking PRs…</span>
      {:else if pr.kind === 'merged'}
        <span class="pill ok">All PRs merged</span>
      {:else if pr.kind === 'open'}
        <span class="pill warn">{plural(pr.count, 'PR')} open</span>
      {:else if pr.kind === 'unmerged'}
        <span class="pill warn">No merged PR</span>
      {:else if pr.kind === 'none'}
        <span class="muted">No worktrees yet</span>
      {:else}
        <span class="muted">PR status unknown</span>
      {/if}
    </div>
    <p class="muted">
      Clean up stops the session and removes <code>{worker.id}</code>'s stack, worktrees and the branches Tworky
      created{scratch ? ', then takes this scratch item off the list' : ''}.
    </p>
    <div class="row">
      <span class="grow"></span>
      <button class="danger outline" {disabled} onclick={() => cleanup(false)}>Clean up</button>
      {#if ownsItem && !scratch}
        <button class="danger fill" {disabled} onclick={() => cleanup(true)}>Clean up and remove item</button>
      {/if}
    </div>
  {/if}
</section>

<style>
  .panel {
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
  }
  .box {
    padding: 10px 14px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  h2 {
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--muted);
    margin: 0;
    font-weight: 600;
  }
  p {
    margin: 0;
  }
  .row {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }
  .grow {
    flex: 1;
    min-width: 0;
  }
  .prline {
    display: flex;
    gap: 6px;
    align-items: center;
    font-size: 12px;
  }
  .pill {
    border-radius: 999px;
    padding: 1px 8px;
    font-size: 11px;
    font-weight: 600;
  }
  .pill.ok {
    background: color-mix(in srgb, var(--ok) 18%, transparent);
    color: var(--ok);
  }
  .pill.warn {
    background: color-mix(in srgb, var(--warn) 18%, transparent);
    color: var(--warn);
  }
  button.outline {
    background: transparent;
    border-color: var(--bad);
    color: var(--bad);
  }
  button.fill,
  button.fill:hover:not(:disabled) {
    background: var(--bad);
    border-color: var(--bad);
    color: #fff;
  }
</style>
