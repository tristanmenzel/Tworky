<script lang="ts">
  import { onMount } from 'svelte'
  import { api, type ProjectSummary, type RepoSearch } from './api'

  interface Props {
    onCreate: (
      title: string,
      description: string,
      project: string | null,
      repos: string[],
      projectName: string | null,
    ) => Promise<void>
    onClose: () => void
  }

  let { onCreate, onClose }: Props = $props()

  let tab = $state<'projects' | 'repos'>('projects')
  let title = $state('')
  let description = $state('')
  let projects = $state<ProjectSummary[]>([])
  let project = $state<string | null>(null)
  let query = $state('')
  let search = $state<RepoSearch | null>(null)
  let searching = $state(false)
  let picked = $state<string[]>([])
  let creating = $state(false)
  let error = $state<string | null>(null)
  let dialog = $state<HTMLDivElement>()
  let searchTimer: ReturnType<typeof setTimeout> | undefined
  let searchSeq = 0

  const ready = $derived(
    title.trim().length > 0 && (tab === 'projects' ? project !== null : picked.length > 0) && !creating,
  )
  const suggestedName = $derived(picked[0]?.split('/')[1]?.toLowerCase().replace(/[^a-z0-9_-]/g, '-') ?? '')
  let projectName = $state<string | null>(null)

  onMount(async () => {
    dialog?.querySelector<HTMLInputElement>('input')?.focus()
    try {
      projects = await api.projects()
      if (projects.length === 1) project = projects[0].name
    } catch (e) {
      error = String(e)
    }
  })

  async function runSearch() {
    const seq = ++searchSeq
    searching = true
    try {
      const result = await api.searchRepos(query)
      if (seq === searchSeq) search = result
    } catch (e) {
      if (seq === searchSeq) error = String(e)
    } finally {
      if (seq === searchSeq) searching = false
    }
  }

  function showRepos() {
    tab = 'repos'
    if (!search) runSearch()
  }

  function onQuery() {
    clearTimeout(searchTimer)
    searchTimer = setTimeout(runSearch, 350)
  }

  function toggle(nwo: string) {
    picked = picked.includes(nwo) ? picked.filter((p) => p !== nwo) : [...picked, nwo]
  }

  async function create() {
    creating = true
    error = null
    try {
      await onCreate(
        title.trim(),
        description.trim(),
        tab === 'projects' ? project : null,
        tab === 'repos' ? picked : [],
        tab === 'repos' ? (projectName ?? suggestedName).trim() || null : null,
      )
    } catch (e) {
      error = String(e)
    } finally {
      creating = false
    }
  }
</script>

<div
  class="backdrop"
  role="dialog"
  aria-modal="true"
  aria-label="New item"
  tabindex="-1"
  bind:this={dialog}
  onkeydown={(e) => e.key === 'Escape' && !creating && onClose()}
  onclick={(e) => e.target === e.currentTarget && !creating && onClose()}
>
  <div class="panel">
    <header>
      <h1>Add custom work</h1>
      <span class="muted">Work with no GitHub issue. It gets a random name and can be linked to an issue later.</span>
    </header>

    <label>
      <span>Title</span>
      <input bind:value={title} placeholder="Fix header layout" disabled={creating} />
    </label>
    <label>
      <span>Description <span class="muted">(optional, shown to Claude)</span></span>
      <textarea bind:value={description} rows="3" disabled={creating}></textarea>
    </label>

    <nav class="tabs">
      <button class:active={tab === 'projects'} onclick={() => (tab = 'projects')}>Projects</button>
      <button class:active={tab === 'repos'} onclick={showRepos}>Repositories</button>
    </nav>

    <div class="pane">
      {#if tab === 'projects'}
        {#each projects as p (p.name)}
          <label class="choice">
            <input type="radio" name="project" value={p.name} bind:group={project} disabled={creating} />
            <strong>{p.name}</strong>
            <span class="muted">{p.repos.map((r) => r.name).join(', ')}</span>
          </label>
        {:else}
          <p class="muted">No projects configured yet. Use Repositories to start one.</p>
        {/each}
      {:else}
        <input
          class="search"
          placeholder="Search GitHub repositories…"
          bind:value={query}
          oninput={onQuery}
          disabled={creating}
          aria-label="Search repositories"
        />
        <p class="muted small">
          Only repositories without a project config. Local clones are listed first; others are cloned when you
          create the item.
        </p>
        {#if search?.search_error}<p class="warn small">GitHub search failed: {search.search_error}</p>{/if}
        {#if searching && !search}
          <p class="muted">Searching…</p>
        {:else}
          {#each search?.candidates ?? [] as c (c.name_with_owner)}
            <label class="choice">
              <input
                type="checkbox"
                checked={picked.includes(c.name_with_owner)}
                onchange={() => toggle(c.name_with_owner)}
                disabled={creating}
              />
              <strong>{c.name_with_owner}</strong>
              <span class="badge" class:local={!!c.local}>{c.local ? 'local' : 'will clone'}</span>
              <span class="muted desc">{c.description}</span>
            </label>
          {:else}
            <p class="muted">{query ? 'No matching repositories.' : 'Type to search GitHub.'}</p>
          {/each}
        {/if}
        {#if picked.length}
          <label class="small name">
            <span>New project for {picked.join(', ')}, set up afterwards by a Claude session under Projects</span>
            <input
              value={projectName ?? suggestedName}
              oninput={(e) => (projectName = e.currentTarget.value)}
              disabled={creating}
              aria-label="New project name"
            />
          </label>
        {/if}
      {/if}
    </div>

    {#if error}<pre class="error">{error}</pre>{/if}

    <footer>
      <button onclick={onClose} disabled={creating}>Cancel</button>
      <button class="primary" disabled={!ready} onclick={create}>
        {creating ? (tab === 'repos' ? 'Setting up repositories…' : 'Creating…') : 'Create item'}
      </button>
    </footer>
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 20;
    display: flex;
    align-items: flex-start;
    justify-content: center;
    padding-top: 8vh;
    background: color-mix(in srgb, #000 60%, transparent);
    outline: none;
  }
  .panel {
    width: min(640px, calc(100vw - 32px));
    max-height: 84vh;
    display: flex;
    flex-direction: column;
    gap: 10px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 16px 18px;
    box-shadow: 0 10px 40px rgb(0 0 0 / 0.4);
  }
  header {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  h1 {
    font-size: 16px;
    margin: 0;
    font-weight: 600;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  input,
  textarea {
    font: inherit;
    color: var(--text);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 5px 8px;
  }
  textarea {
    resize: vertical;
  }
  .tabs {
    display: flex;
    gap: 2px;
    border-bottom: 1px solid var(--border);
  }
  .tabs button {
    border-radius: 6px 6px 0 0;
    border: 1px solid transparent;
    border-bottom: 0;
    background: none;
  }
  .tabs button.active {
    background: var(--bg);
    border-color: var(--border);
  }
  .pane {
    flex: 1;
    min-height: 160px;
    overflow: auto;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .choice {
    flex-direction: row;
    align-items: center;
    gap: 8px;
    padding: 4px 6px;
    border-radius: 6px;
    cursor: pointer;
  }
  .choice:hover {
    background: var(--panel-2);
  }
  .choice .muted,
  .desc {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .search {
    margin-bottom: 4px;
  }
  .small {
    font-size: 12px;
    margin: 2px 0;
  }
  .warn {
    color: var(--warn);
  }
  .badge {
    flex: none;
    font-size: 10px;
    padding: 0 6px;
    border-radius: 8px;
    background: var(--panel-2);
    color: var(--warn);
  }
  .badge.local {
    color: var(--ok);
  }
  .error {
    margin: 0;
    color: var(--bad);
    white-space: pre-wrap;
    max-height: 120px;
    overflow: auto;
  }
  footer {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
</style>
