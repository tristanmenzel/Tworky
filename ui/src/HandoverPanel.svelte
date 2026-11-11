<script lang="ts">
  import { api, type BrowserTabView, type Handover, type Worker } from './api'

  interface Props {
    worker: Worker
    handover: Handover
    tabs: BrowserTabView[]
    onShowTab: (tab: number) => void
    onOpenWorker: (worker: Worker) => void
    onError: (message: string) => void
  }

  let { worker, handover, tabs, onShowTab, onOpenWorker, onError }: Props = $props()

  let checked = $state<boolean[]>([])

  const handoverTabs = $derived(
    handover.tabs.flatMap((id) => tabs.filter((t) => t.id === id)),
  )

  async function dismiss() {
    try {
      await api.dismissHandover(worker.id)
    } catch (e) {
      onError(String(e))
    }
  }
</script>

<section class="handover">
  <div class="head">
    <strong>Ready for UAT</strong>
    <span class="muted">{worker.id}</span>
    <span class="buttons">
      <button onclick={() => onOpenWorker(worker)} title="Reply to Claude with what you found">Reply</button>
      <button onclick={dismiss} title="Clear the handover; sending Claude a prompt clears it too">Done</button>
    </span>
  </div>
  <p>{handover.summary}</p>
  {#if handover.steps.length}
    <ol>
      {#each handover.steps as step, i (i)}
        <li>
          <label>
            <input type="checkbox" bind:checked={checked[i]} />
            <span class:done={checked[i]}>{step}</span>
          </label>
        </li>
      {/each}
    </ol>
  {/if}
  {#if handoverTabs.length}
    <div class="tabs">
      {#each handoverTabs as t (t.id)}
        <button onclick={() => onShowTab(t.id)} title={t.url}>
          <span style:color={t.color ?? undefined}>🌐</span>
          {t.account ?? 'Anonymous'}
        </button>
      {/each}
    </div>
  {:else if handover.tabs.length}
    <div class="muted">The tabs Claude left open have been closed.</div>
  {/if}
</section>

<style>
  .handover {
    padding: 12px 14px;
    margin-bottom: 12px;
    background: color-mix(in srgb, var(--accent) 8%, var(--panel));
    border: 1px solid color-mix(in srgb, var(--accent) 45%, var(--border));
    border-radius: 8px;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .buttons {
    margin-left: auto;
    display: flex;
    gap: 6px;
  }
  p {
    margin: 8px 0;
  }
  ol {
    margin: 0 0 8px;
    padding-left: 20px;
  }
  li + li {
    margin-top: 4px;
  }
  label {
    display: flex;
    gap: 8px;
    align-items: baseline;
    cursor: pointer;
  }
  .done {
    color: var(--muted);
    text-decoration: line-through;
  }
  .tabs {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
</style>
