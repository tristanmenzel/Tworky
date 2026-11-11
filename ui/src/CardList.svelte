<script lang="ts">
  import type { Activity, Card, Issue, Item, Worker } from './api'
  import { boardStatusName, boardTone, cardComplete, shownBoards, typeClass } from './issue'
  import WorkerDot from './WorkerDot.svelte'

  interface Props {
    cards: Card[]
    issues: Map<string, Issue>
    items: Item[]
    projects: Record<string, string>
    statusBoards: Record<string, number>
    workers: Map<string, Worker>
    itemWorkers: Map<string, Worker>
    activityByCard: Map<string, Activity>
    selectedId: string | null
    flashId: string | null
    onSelect: (id: string) => void
  }

  let {
    cards,
    issues,
    items,
    projects,
    statusBoards,
    workers,
    itemWorkers,
    activityByCard,
    selectedId,
    flashId,
    onSelect,
  }: Props = $props()

  function repoName(id: string): string {
    return issues.get(id)?.repo ?? ''
  }

  const statusBoard = (i: Issue) => statusBoards[projects[i.id]]

  function statuses(i: Issue): string[] {
    return shownBoards(i, statusBoard(i)).flatMap((b) => boardStatusName(b) ?? [])
  }
</script>

{#snippet status(i: Issue)}
  {#each statuses(i) as name, n (n)}
    <span class="board {boardTone(name)}">{name}</span>
  {/each}
{/snippet}

{#snippet attention(activity: Activity | null)}
  {#if activity === 'working'}
    <span class="attn working">Working</span>
  {:else if activity === 'waiting'}
    <span class="attn waiting">Needs you</span>
  {:else if activity === 'account'}
    <span class="attn account">Needs account</span>
  {:else if activity === 'uat'}
    <span class="attn uat">Ready for UAT</span>
  {:else if activity === 'idle'}
    <span class="attn idle">Idle</span>
  {/if}
{/snippet}

<ul class="cards">
  {#if items.length}
    <li class="group muted">Pull requests and custom items</li>
    {#each items as item (item.name)}
      {@const iw = itemWorkers.get(item.name)}
      {@const activity = activityByCard.get(`item:${item.name}`) ?? null}
      <li>
        <button
          class="card {activity ?? ''}"
          class:selected={selectedId === `item:${item.name}`}
          class:flash={flashId === `item:${item.name}`}
          data-card={`item:${item.name}`}
          onclick={() => onSelect(`item:${item.name}`)}
        >
          <div class="head">
            {#if item.source.kind === 'pull_request'}
              <span class="tag type-pr">PR</span>
              <span class="num">#{item.source.number}</span>
            {:else}
              <span class="tag type-item">{item.scratch ? 'Scratch' : 'Item'}</span>
              <span class="num">{item.name}</span>
            {/if}
          </div>
          <div class="title">{item.title}</div>
          {#if iw || activity}
            <div class="foot">
              {@render attention(activity)}
              <WorkerDot worker={iw} {activity} title={iw?.id} />
            </div>
          {/if}
        </button>
      </li>
    {/each}
  {/if}
  {#each cards as card, i (card.id)}
    {@const root = issues.get(card.id)}
    {@const rootWorker = workers.get(card.id)}
    {@const complete = cardComplete(card, issues, statusBoard)}
    {@const activity = activityByCard.get(card.id) ?? null}
    {#if root}
      {#if i === 0 || repoName(cards[i - 1].id) !== root.repo}
        <li class="group muted">{root.repo}</li>
      {/if}
      <li>
        <button
          class="card {activity ?? ''}"
          class:selected={selectedId === card.id}
          class:flash={flashId === card.id}
          class:complete
          data-card={card.id}
          title={complete ? 'Closed or done: remove it from the list once its worker is cleaned up' : undefined}
          onclick={() => onSelect(card.id)}
        >
          <div class="head">
            <span class="tag {typeClass(root.issue_type)}">{root.issue_type ?? 'Issue'}</span>
            <span class="num">#{root.number}</span>
          </div>
          <div class="title">{root.title}</div>
          {#if complete || statuses(root).length || rootWorker || activity}
            <div class="foot">
              <span class="meta">
                {#if complete}
                  <span class="board done">✓ Done</span>
                {:else}
                  {@render status(root)}
                {/if}
              </span>
              {@render attention(activity)}
              {#if rootWorker || activity}
                <WorkerDot worker={rootWorker} {activity} title={rootWorker?.id} />
              {/if}
            </div>
          {/if}
        </button>
      </li>
    {/if}
  {:else}
    {#if !items.length}<li class="muted empty">Nothing on the list. Add an issue, pull request or custom item with +.</li>{/if}
  {/each}
</ul>

<style>
  .cards {
    list-style: none;
    margin: 0;
    padding: 4px 8px 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .group {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    margin: 8px 4px 0;
  }
  .empty {
    padding: 12px 4px;
  }
  .card {
    display: flex;
    flex-direction: column;
    gap: 3px;
    width: 100%;
    text-align: left;
    background: var(--bg);
    border: 1px solid var(--border);
    border-left: 3px solid transparent;
    border-radius: 8px;
    padding: 8px 10px;
  }
  .card:hover {
    border-color: var(--accent);
  }
  .type-item {
    color: var(--warn);
  }
  .type-pr {
    color: var(--ok);
  }
  .card.complete:not(.selected) {
    opacity: 0.55;
  }
  .card.complete:hover {
    opacity: 1;
  }
  .card.complete .title {
    text-decoration: line-through;
    text-decoration-color: color-mix(in srgb, var(--muted) 60%, transparent);
  }
  .card.waiting:not(.selected) {
    border-left-color: var(--bad);
    background: color-mix(in srgb, var(--bad) 7%, transparent);
  }
  .card.idle:not(.selected) {
    border-left-color: var(--warn);
  }
  .card.uat:not(.selected) {
    border-left-color: var(--accent);
  }
  .card.account:not(.selected) {
    border-left-color: var(--account);
  }
  .attn {
    flex: none;
    font-size: 11px;
    font-weight: 600;
    padding: 0 6px;
    border-radius: 8px;
    white-space: nowrap;
  }
  .attn.working {
    color: var(--ok);
    border: 1px solid color-mix(in srgb, var(--ok) 55%, transparent);
  }
  .attn.waiting {
    color: var(--on-bad);
    background: var(--bad);
  }
  .attn.idle {
    color: var(--warn);
    border: 1px solid color-mix(in srgb, var(--warn) 55%, transparent);
  }
  .attn.uat {
    color: var(--on-accent);
    background: var(--accent);
  }
  .attn.account {
    color: var(--on-account);
    background: var(--account);
  }
  .card.selected {
    border-color: var(--accent);
    border-left-color: var(--accent);
    background: var(--panel-2);
  }
  .card.flash {
    animation: flash 1.6s ease-out;
  }
  @keyframes flash {
    0%,
    30% {
      box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 55%, transparent);
    }
    100% {
      box-shadow: 0 0 0 0 transparent;
    }
  }
  .head {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .title {
    font-weight: 600;
    overflow-wrap: anywhere;
  }
  .foot {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 6px;
  }
  .meta {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .meta .board + .board {
    margin-left: 4px;
  }
  .num {
    flex: none;
    font-family: var(--mono);
    font-size: 11px;
    color: var(--muted);
    font-weight: normal;
  }
  .board {
    flex: none;
    font-size: 10px;
    font-weight: normal;
    padding: 0 6px;
    border-radius: 8px;
    background: var(--panel-2);
    color: var(--muted);
    white-space: nowrap;
  }
  .board.doing {
    color: var(--warn);
  }
  .board.done {
    color: var(--ok);
  }
</style>
