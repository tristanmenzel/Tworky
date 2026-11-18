<script lang="ts">
  import { listen } from '@tauri-apps/api/event'
  import { untrack } from 'svelte'
  import { api, MEMORY_CHANGED_SINCE_SHOWN, type HunkLine, type PendingMemoryChange } from './api'

  interface Props {
    project: string
    visible: boolean
    onError: (message: string) => void
  }

  let { project, visible, onError }: Props = $props()

  const VERB = { add: 'Add to', edit: 'Edit in', remove: 'Remove from' } as const
  const SIGN = { ctx: ' ', del: '−', add: '+' } as const
  const encoder = new TextEncoder()
  const decoder = new TextDecoder()

  let changes = $state<PendingMemoryChange[] | null>(null)
  let checked = $state<Record<string, boolean>>({})
  let fresh = $state<Record<string, boolean>>({})
  let statuses: Record<string, PendingMemoryChange['preview']['status']> = {}
  let loadError = $state<string | null>(null)
  let refusedNotice = $state(false)
  let busy = $state(false)
  let loadSeq = 0
  let firstLoad = true

  const selected = $derived((changes ?? []).filter((c) => checked[c.id]))
  const selectable = $derived((changes ?? []).filter((c) => c.preview.status === 'ready' && !fresh[c.id]))
  const staleSelected = $derived(selected.some((c) => c.preview.status === 'stale'))
  const allSelected = $derived(!!selected.length && selectable.every((c) => checked[c.id]))
  const base = $derived.by(() => {
    for (const c of changes ?? []) if (c.preview.status === 'ready') return c.preview.base
    return null
  })

  async function load() {
    const seq = ++loadSeq
    try {
      const all = await api.memoryChanges()
      if (seq !== loadSeq) return
      const entry = all.find((p) => p.project === project)
      const list = entry?.changes ?? []
      const initial = firstLoad && !entry?.error
      if (initial) firstLoad = false
      const nextChecked: Record<string, boolean> = {}
      const nextFresh: Record<string, boolean> = {}
      const nextStatuses: typeof statuses = {}
      for (const c of list) {
        const status = c.preview.status
        if (!(c.id in checked)) {
          nextChecked[c.id] = initial && status === 'ready'
          if (!initial) nextFresh[c.id] = true
        } else {
          nextChecked[c.id] = statuses[c.id] === 'ready' && status === 'stale' ? false : checked[c.id]
          if (fresh[c.id]) nextFresh[c.id] = true
        }
        nextStatuses[c.id] = status
      }
      checked = nextChecked
      fresh = nextFresh
      statuses = nextStatuses
      loadError = entry?.error ?? null
      changes = list
    } catch (e) {
      onError(String(e))
    }
  }

  $effect(() => {
    if (visible) untrack(load)
  })

  $effect(() => {
    const unlisten = listen('memory-changes', () => {
      if (untrack(() => visible)) load()
    })
    return () => {
      unlisten.then((f) => f())
    }
  })

  function selectAll(on: boolean) {
    for (const c of changes ?? []) checked[c.id] = on && c.preview.status === 'ready' && !fresh[c.id]
    refusedNotice = false
  }

  function ticked(id: string) {
    delete fresh[id]
    refusedNotice = false
  }

  async function act(run: () => Promise<void>) {
    busy = true
    refusedNotice = false
    let refused = false
    try {
      await run()
    } catch (e) {
      if (String(e).includes(MEMORY_CHANGED_SINCE_SHOWN)) refused = true
      else onError(String(e))
    } finally {
      busy = false
      await load()
      if (refused) {
        for (const id in checked) checked[id] = false
        refusedNotice = true
      }
    }
  }

  function approve() {
    const ids = selected.map((c) => c.id)
    if (!base) return
    const b = base
    act(() => api.approveMemoryChanges(project, ids, b))
  }

  function reject() {
    const ids = selected.map((c) => c.id)
    act(() => api.rejectMemoryChanges(project, ids))
  }

  function section(c: PendingMemoryChange) {
    if (c.preview.status === 'ready' && c.preview.section) return c.preview.section
    return c.change.kind === 'add' ? c.change.section : null
  }

  function parts(line: HunkLine): [string, string, string] {
    if (!line.changed) return [line.text, '', '']
    const bytes = encoder.encode(line.text)
    const { start, end } = line.changed
    return [
      decoder.decode(bytes.slice(0, start)),
      decoder.decode(bytes.slice(start, end)),
      decoder.decode(bytes.slice(end)),
    ]
  }

  function staleMessage(reason: string) {
    return /[.!?]$/.test(reason) ? `${reason} It can only be rejected.` : `${reason}. It can only be rejected.`
  }
</script>

<div class="changes">
  <header>
    <h1>Memory changes</h1>
  </header>
  <p class="muted small">Sessions on {project} proposed these. Nothing changes in the memory until you approve it.</p>

  {#if loadError}
    <p class="load-error" role="alert">Couldn't read the pending changes: {loadError}</p>
  {:else if !changes}
    <p class="muted">Loading…</p>
  {:else if !changes.length}
    <p class="muted empty">No changes waiting.</p>
  {:else}
    <div class="actionbar">
      <label class="selall">
        <input
          type="checkbox"
          checked={allSelected}
          indeterminate={!!selected.length && !allSelected}
          disabled={!selected.length && !selectable.length}
          onchange={(e) => selectAll(e.currentTarget.checked)}
        />
        <span>{selected.length} of {changes.length} selected</span>
      </label>
      {#if refusedNotice}
        <span class="stale-msg" role="alert"
          >The memory changed since these diffs were shown. Review them again, then approve.</span
        >
      {:else if staleSelected}<span class="stale-msg">Untick stale changes to approve.</span>{/if}
      <span class="spacer"></span>
      <button onclick={reject} disabled={busy || !selected.length}>Reject {selected.length || ''}</button>
      <button class="approve" onclick={approve} disabled={busy || !selected.length || staleSelected || !base}
        >Approve {selected.length || ''}</button
      >
    </div>

    <div class="cards">
      {#each changes as c (c.id)}
        {@const kind = c.change.kind}
        {@const sec = section(c)}
        <article class="card" class:stale={c.preview.status === 'stale'} class:off={!checked[c.id]}>
          <label class="card-head">
            <input type="checkbox" bind:checked={checked[c.id]} onchange={() => ticked(c.id)} />
            <span class="kind {kind}">{kind}</span>
            {#if fresh[c.id]}<span class="new">New</span>{/if}
            <span>
              {#if sec}{VERB[kind]} <b>{sec}</b>{:else}{kind === 'edit' ? 'Edit' : 'Remove'}{/if}
            </span>
            <span class="spacer"></span>
            <span class="muted">{c.change.author} · {c.change.date}</span>
          </label>
          {#if c.preview.status === 'ready'}
            <div class="diff">
              {#each c.preview.hunk.lines as line, i (i)}
                {@const [before, changed, after] = parts(line)}
                <div class="ln {line.tag}">
                  <i>{SIGN[line.tag]}</i><span
                    >{before}{#if changed}<span class="hl">{changed}</span>{/if}{after}</span
                  >
                </div>
              {/each}
            </div>
          {:else}
            <div class="card-foot">
              <span class="stale-msg">{staleMessage(c.preview.reason)}</span>
            </div>
          {/if}
        </article>
      {/each}
    </div>
  {/if}
</div>

<style>
  .changes {
    padding: 16px 24px 32px;
    max-width: 980px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  header {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  h1 {
    font-size: 18px;
    font-weight: 600;
    margin: 0;
  }
  .small {
    font-size: 12px;
    margin: 0 0 4px;
  }
  .empty {
    padding: 24px 0;
  }
  .load-error {
    color: var(--bad);
  }
  .spacer {
    flex: 1;
  }
  .actionbar {
    position: sticky;
    top: 0;
    z-index: 1;
    display: flex;
    gap: 10px;
    align-items: center;
    flex-wrap: wrap;
    padding: 8px 12px;
    margin-bottom: 4px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
  }
  .selall {
    display: flex;
    gap: 8px;
    align-items: center;
    cursor: pointer;
    font-weight: 600;
  }
  input[type='checkbox'] {
    width: 15px;
    height: 15px;
    margin: 0;
    accent-color: var(--accent);
    cursor: pointer;
  }
  button.approve {
    background: var(--ok-fill);
    border-color: var(--ok-fill);
    color: var(--on-ok);
  }
  .stale-msg {
    color: var(--warn);
    flex: 1;
    min-width: 0;
  }
  .cards {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .card {
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    overflow: hidden;
  }
  .card.stale {
    border-color: var(--warn);
  }
  .card.off {
    opacity: 0.55;
  }
  .card.off .diff {
    filter: grayscale(0.6);
  }
  .card-head {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
    padding: 8px 12px;
    border-bottom: 1px solid var(--border);
    cursor: pointer;
  }
  .kind {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    padding: 1px 7px;
    border-radius: 4px;
    border: 1px solid currentColor;
  }
  .new {
    font-size: 11px;
    font-weight: 600;
    padding: 1px 6px;
    border-radius: 4px;
    background: var(--accent);
    color: var(--on-accent);
  }
  .kind.add {
    color: var(--ok);
  }
  .kind.edit {
    color: var(--accent);
  }
  .kind.remove {
    color: var(--bad);
  }
  .card-foot {
    display: flex;
    gap: 8px;
    align-items: center;
    padding: 8px 12px;
  }
  .diff {
    font-family: var(--mono);
    font-size: 12px;
    overflow-x: auto;
  }
  .ln {
    display: grid;
    grid-template-columns: 22px 1fr;
    white-space: pre-wrap;
    word-break: break-word;
    padding-right: 12px;
  }
  .ln > i {
    font-style: normal;
    text-align: center;
    color: var(--muted);
    user-select: none;
  }
  .ln.ctx {
    color: var(--muted);
  }
  .ln.add {
    background: var(--add-bg);
  }
  .ln.add > i {
    color: var(--ok);
  }
  .ln.del {
    background: var(--del-bg);
  }
  .ln.del > i {
    color: var(--bad);
  }
  .ln.del > span {
    text-decoration: line-through;
    text-decoration-color: color-mix(in srgb, var(--bad) 50%, transparent);
  }
  .ln.add .hl {
    background: color-mix(in srgb, var(--ok) 28%, transparent);
    border-radius: 2px;
  }
  .ln.del .hl {
    background: color-mix(in srgb, var(--bad) 28%, transparent);
    border-radius: 2px;
  }
</style>
