<script lang="ts">
  import { onMount } from 'svelte'
  import { api, type AddOutcome, type PrHit, type ProjectChoice as Choice } from './api'
  import Dialog from './Dialog.svelte'
  import ProjectChoice from './ProjectChoice.svelte'

  interface Props {
    onAdded: (outcome: AddOutcome & { outcome: 'added' }) => void
    onClose: () => void
  }

  let { onAdded, onClose }: Props = $props()

  let url = $state('')
  let requests = $state<PrHit[]>([])
  let loading = $state(true)
  let needsProject = $state<string | null>(null)
  let choice = $state<Choice | null>(null)
  let adding = $state(false)
  let error = $state<string | null>(null)

  const validUrl = $derived(/^https:\/\/github\.com\/[^/]+\/[^/]+\/pull\/\d+\/?$/.test(url.trim()))

  onMount(async () => {
    try {
      requests = await api.reviewRequests()
    } catch (e) {
      error = String(e)
    } finally {
      loading = false
    }
  })

  async function add() {
    adding = true
    error = null
    try {
      const outcome = await api.addPullRequestItem(url.trim(), needsProject && choice ? choice : { kind: 'infer' })
      if (outcome.outcome === 'needs_project') needsProject = outcome.repo
      else onAdded(outcome)
    } catch (e) {
      error = String(e)
    } finally {
      adding = false
    }
  }
</script>

<Dialog
  title="Add a pull request"
  subtitle={needsProject ? undefined : 'Pick a review request, or paste the URL of any pull request.'}
  busy={adding}
  wide
  {onClose}
>
  {#if needsProject}
    <ProjectChoice repo={needsProject} disabled={adding} bind:choice onError={(e) => (error = e)} />
  {:else}
    <input
      type="url"
      bind:value={url}
      placeholder="https://github.com/owner/repo/pull/123"
      aria-label="Pull request URL"
      onkeydown={(e) => e.key === 'Enter' && validUrl && add()}
    />
    <div class="grid">
      <table>
        <thead>
          <tr><th>Repository</th><th>#</th><th>Title</th><th>Author</th><th>Branch</th></tr>
        </thead>
        <tbody>
          {#each requests as r (r.url)}
            <tr
              class="row"
              class:picked={url.trim() === r.url}
              class:listed={!!r.item}
              title={r.item ? `Already on the list as ${r.item}` : undefined}
              onclick={() => !r.item && (url = r.url)}
              ondblclick={() => !r.item && ((url = r.url), add())}
            >
              <td>{r.repo}</td>
              <td>{r.number}</td>
              <td class="title">{r.is_draft ? '[draft] ' : ''}{r.title}</td>
              <td>{r.author}</td>
              <td><code>{r.head}</code></td>
            </tr>
          {:else}
            <tr>
              <td colspan="5" class="muted note">
                {loading ? 'Loading review requests…' : 'No open pull requests are waiting on your review.'}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
  {#if error}<pre class="error">{error}</pre>{/if}

  {#snippet footer()}
    {#if needsProject}
      <button onclick={() => (needsProject = null)} disabled={adding}>Back</button>
    {/if}
    <span class="spacer"></span>
    <button onclick={onClose} disabled={adding}>Cancel</button>
    <button class="primary" disabled={!validUrl || adding || (!!needsProject && !choice)} onclick={add}>
      {adding ? 'Adding…' : 'Add to list'}
    </button>
  {/snippet}
</Dialog>

<style>
  .spacer {
    flex: 1;
  }
</style>
