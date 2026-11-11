<script lang="ts">
  import { onMount } from 'svelte'
  import { api, type ProjectChoice, type ProjectSummary } from './api'

  interface Props {
    repo: string
    disabled: boolean
    choice: ProjectChoice | null
    onError: (message: string) => void
  }

  let { repo, disabled, choice = $bindable(), onError }: Props = $props()

  let projects = $state<ProjectSummary[]>([])
  let kind = $state<'existing' | 'new'>('existing')
  let existing = $state('')
  let newName = $state('')

  onMount(async () => {
    newName = repo.split('/')[1]?.toLowerCase().replace(/[^a-z0-9_-]/g, '-') ?? ''
    try {
      projects = await api.projects()
      existing = projects[0]?.name ?? ''
      if (!projects.length) kind = 'new'
    } catch (e) {
      onError(String(e))
    }
  })

  $effect(() => {
    choice =
      kind === 'existing'
        ? existing
          ? { kind: 'existing', name: existing }
          : null
        : newName.trim()
          ? { kind: 'new', name: newName.trim() }
          : null
  })
</script>

<div class="choice">
  <p>
    No project covers <strong>{repo}</strong> yet. Add it to a project you have, or start a new project for it.
  </p>
  <label class="option">
    <input type="radio" value="existing" bind:group={kind} disabled={disabled || !projects.length} />
    <span>Existing project</span>
    <select bind:value={existing} disabled={disabled || kind !== 'existing' || !projects.length}>
      {#each projects as p (p.name)}
        <option value={p.name}>{p.name} ({p.repos.map((r) => r.name).join(', ') || 'no repos'})</option>
      {/each}
    </select>
  </label>
  {#if kind === 'existing'}
    <p class="muted small">
      {repo} is added to the project's config, cloned first if there's no local copy. Its worktrees then work in that
      project's workers.
    </p>
  {/if}
  <label class="option">
    <input type="radio" value="new" bind:group={kind} {disabled} />
    <span>New project</span>
    <input bind:value={newName} disabled={disabled || kind !== 'new'} aria-label="New project name" />
  </label>
  {#if kind === 'new'}
    <p class="muted small">
      Creates a minimal project for {repo} and opens a setup session under Projects, where Claude asks about the rest
      of the project and configures its ports, stack, hooks and services.
    </p>
  {/if}
</div>

<style>
  .choice {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  p {
    margin: 0;
  }
  .option {
    display: grid;
    grid-template-columns: auto 120px 1fr;
    align-items: center;
    gap: 8px;
  }
  .small {
    font-size: 12px;
    margin-left: 24px;
  }
</style>
