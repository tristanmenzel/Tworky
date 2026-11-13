<script lang="ts">
  import { untrack } from 'svelte'
  import { openUrl } from '@tauri-apps/plugin-opener'
  import type { Item, Job, LocalStatus, Worker } from './api'
  import { ExistingWorkLoader } from './existingWork.svelte'
  import { ownsItem } from './issue'
  import LocalWork from './LocalWork.svelte'
  import Markdown from './Markdown.svelte'
  import PullRequests from './PullRequests.svelte'

  interface Props {
    item: Item
    worker: Worker | undefined
    workers: Worker[]
    exited: Record<string, boolean>
    jobs: Job[]
    local: LocalStatus | null
    localFailed: boolean
    logs: string[]
    onBegin: (name: string) => void
    onLink: (name: string, issueUrl: string) => Promise<void>
    onRemove: (name: string) => void
    onOpenWorker: (worker: Worker) => void
    onError: (message: string) => void
    issuesRefreshed: number
  }

  let {
    item,
    worker,
    workers,
    exited,
    jobs,
    local,
    localFailed,
    logs,
    onBegin,
    onLink,
    onRemove,
    onOpenWorker,
    onError,
    issuesRefreshed,
  }: Props = $props()

  let issueUrl = $state('')
  let linking = $state(false)

  const pr = $derived(item.source.kind === 'pull_request' ? item.source : null)
  const validUrl = $derived(/^https:\/\/github\.com\/[^/]+\/[^/]+\/issues\/\d+\/?$/.test(issueUrl.trim()))

  const itemName = $derived(item.name)
  const existing = new ExistingWorkLoader((message) => onError(message))

  $effect(() => {
    const name = itemName
    void issuesRefreshed
    untrack(() => existing.load({ kind: 'item', name }, false))
  })

  async function link() {
    linking = true
    try {
      await onLink(item.name, issueUrl.trim())
    } catch {
      // onLink already reported it; keep the URL so it can be corrected
    } finally {
      linking = false
    }
  }

  function remove() {
    if (confirm(`Remove item ${item.name} (“${item.title}”)?`)) onRemove(item.name)
  }
</script>

<div class="detail">
  <div class="column">
    <section class="panel">
      <header>
        <div>
          <div class="meta">
            {#if pr}
              <span class="tag type-pr">Pull request</span>
              <button class="link muted" onclick={() => openUrl(pr.url)}>{pr.repo}#{pr.number} ↗</button>
            {:else}
              <span class="tag type-item">{item.scratch ? 'Scratch item' : 'Item'}</span>
            {/if}
            <code class="muted">{item.name}</code>
            <span class="tag proj">{item.project}</span>
            <span class="muted">added {new Date(item.created_at * 1000).toLocaleString()}</span>
          </div>
          <h1>{item.title}</h1>
          {#if pr}
            <div class="muted">
              <code>{pr.head}</code> into <code>{pr.base}</code>. The worker checks out the PR's branch.
            </div>
          {:else if item.scratch}
            <div class="muted">A throwaway item for testing the project's setup. It goes once its worker is cleaned up.</div>
          {:else}
            <div class="muted">Custom work with no GitHub issue yet.</div>
          {/if}
        </div>
        {#if !worker}
          <button class="primary" onclick={() => onBegin(item.name)}>Begin session</button>
        {/if}
      </header>

      {#if item.description}
        <div class="desc">
          <Markdown source={item.description} />
        </div>
      {/if}
    </section>

    {#if worker}
      <LocalWork
        {worker}
        {jobs}
        {local}
        {localFailed}
        log={logs}
        ownsItem={ownsItem(worker, [item])}
        {onOpenWorker}
        {onError}
      />
    {/if}

    {#if item.source.kind === 'custom'}
      <section class="panel box">
        <h2>Link to an issue</h2>
        <p class="muted">
          Once an issue exists for this work, link it here.
          {worker ? `Worker ${worker.id} moves to that issue with its folder, branch and stack, and the` : 'The'} item then
          stands for the issue.
        </p>
        <form
          class="row"
          onsubmit={(e) => {
            e.preventDefault()
            if (validUrl) link()
          }}
        >
          <input
            type="url"
            placeholder="https://github.com/owner/repo/issues/123"
            bind:value={issueUrl}
            disabled={linking}
            aria-label="Issue URL"
          />
          <button type="submit" disabled={!validUrl || linking}>{linking ? 'Linking…' : 'Link'}</button>
        </form>
      </section>
    {/if}

    <section class="panel box">
      <h2>Remove</h2>
      <div class="row">
        <span class="muted">
          {worker ? `Clean up worker ${worker.id} first; then the item can be removed.` : 'Take this item off the list.'}
        </span>
        <button
          class="danger"
          disabled={!!worker}
          title={worker ? 'Clean up the worker first' : undefined}
          onclick={remove}>Remove from list</button
        >
      </div>
    </section>
  </div>

  <div class="column">
    <PullRequests
      work={existing.work}
      loading={existing.loading}
      checkedAt={existing.checkedAt}
      subject={{ kind: 'item', name: item.name }}
      {workers}
      {exited}
      onRefresh={(fetch) => existing.load({ kind: 'item', name: item.name }, fetch)}
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
  header {
    padding: 14px 16px;
    display: flex;
    justify-content: space-between;
    gap: 16px;
    align-items: flex-start;
  }
  .desc {
    padding: 12px 16px 14px;
    border-top: 1px solid var(--border);
  }
  h1 {
    font-size: 20px;
    margin: 4px 0;
    font-weight: 600;
  }
  h2 {
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--muted);
    margin: 0 0 6px;
    font-weight: 600;
  }
  .meta {
    display: flex;
    gap: 8px;
    align-items: center;
    font-size: 12px;
  }
  .type-item {
    color: var(--warn);
  }
  .type-pr {
    color: var(--ok);
  }
  .link.muted {
    color: var(--muted);
  }
  .tag.proj {
    color: var(--accent);
    border-color: var(--accent);
  }
  .box {
    padding: 10px 14px;
  }
  .box p {
    margin: 0 0 8px;
  }
  .row {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .row .muted {
    margin-right: auto;
  }
  input {
    flex: 1;
    font: inherit;
    color: var(--text);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 4px 8px;
  }
</style>
