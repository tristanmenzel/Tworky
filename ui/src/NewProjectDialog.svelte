<script lang="ts">
  import { onMount } from 'svelte'
  import { api, type RepoSearch } from './api'
  import Dialog from './Dialog.svelte'

  interface Props {
    onCreated: (name: string) => void
    onClose: () => void
  }

  let { onCreated, onClose }: Props = $props()

  let query = $state('')
  let search = $state<RepoSearch | null>(null)
  let searching = $state(false)
  let picked = $state<string[]>([])
  let name = $state<string | null>(null)
  let creating = $state(false)
  let error = $state<string | null>(null)
  let timer: ReturnType<typeof setTimeout> | undefined
  let seq = 0

  const suggested = $derived(picked[0]?.split('/')[1]?.toLowerCase().replace(/[^a-z0-9_-]/g, '-') ?? '')
  const projectName = $derived((name ?? suggested).trim())

  onMount(runSearch)

  async function runSearch() {
    const mine = ++seq
    searching = true
    try {
      const result = await api.searchRepos(query)
      if (mine === seq) search = result
    } catch (e) {
      if (mine === seq) error = String(e)
    } finally {
      if (mine === seq) searching = false
    }
  }

  function toggle(nwo: string) {
    picked = picked.includes(nwo) ? picked.filter((p) => p !== nwo) : [...picked, nwo]
  }

  async function create() {
    creating = true
    error = null
    try {
      onCreated(await api.createProject(picked, projectName || null))
    } catch (e) {
      error = String(e)
    } finally {
      creating = false
    }
  }
</script>

<Dialog
  title="New project"
  subtitle="Pick its repositories. A Claude setup session then works out the rest with you."
  busy={creating}
  {onClose}
>
  <input
    bind:value={query}
    oninput={() => {
      clearTimeout(timer)
      timer = setTimeout(runSearch, 350)
    }}
    placeholder="Search repositories without a project…"
    aria-label="Search repositories"
  />
  {#if search?.search_error}<p class="muted">GitHub search failed: {search.search_error}</p>{/if}
  <div class="grid">
    <table>
      <tbody>
        {#each search?.candidates ?? [] as c (c.name_with_owner)}
          <tr class="row" class:picked={picked.includes(c.name_with_owner)} onclick={() => toggle(c.name_with_owner)}>
            <td><input type="checkbox" checked={picked.includes(c.name_with_owner)} tabindex="-1" /></td>
            <td><strong>{c.name_with_owner}</strong></td>
            <td class="muted">{c.local ? 'local' : 'will clone'}</td>
            <td class="title muted">{c.description}</td>
          </tr>
        {:else}
          <tr><td class="muted note">{searching ? 'Searching…' : 'No repositories without a project match.'}</td></tr>
        {/each}
      </tbody>
    </table>
  </div>
  <label class="name">
    <span>Name</span>
    <input value={name ?? suggested} oninput={(e) => (name = e.currentTarget.value)} disabled={creating} />
  </label>
  {#if error}<pre class="error">{error}</pre>{/if}

  {#snippet footer()}
    <button onclick={onClose} disabled={creating}>Cancel</button>
    <button class="primary" disabled={!picked.length || !projectName || creating} onclick={create}>
      {creating ? 'Creating…' : 'Create and set up'}
    </button>
  {/snippet}
</Dialog>

<style>
  .name {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .name input {
    flex: 1;
  }
</style>
