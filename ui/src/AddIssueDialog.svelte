<script lang="ts">
  import { onMount } from 'svelte'
  import { api, type AddOutcome, type IssueHit, type ProjectChoice as Choice } from './api'
  import Dialog from './Dialog.svelte'
  import { typeClass } from './issue'
  import ProjectChoice from './ProjectChoice.svelte'

  interface Props {
    onAdded: (outcome: AddOutcome & { outcome: 'added' }) => void
    onClose: () => void
  }

  let { onAdded, onClose }: Props = $props()

  let query = $state('')
  let hits = $state<IssueHit[]>([])
  let searching = $state(false)
  let picked = $state<IssueHit | null>(null)
  let needsProject = $state<string | null>(null)
  let choice = $state<Choice | null>(null)
  let adding = $state(false)
  let error = $state<string | null>(null)
  let collapsed = $state(new Set<string>())
  let timer: ReturnType<typeof setTimeout> | undefined
  let seq = 0

  interface Node {
    hit: IssueHit
    children: Node[]
    rank: number
  }

  const tree = $derived.by(() => {
    const nodes = new Map<string, Node>(hits.map((hit, i) => [hit.id, { hit, children: [], rank: i }]))
    const roots: Node[] = []
    for (const node of nodes.values()) {
      const parent = node.hit.parent_id ? nodes.get(node.hit.parent_id) : undefined
      if (parent) parent.children.push(node)
      else roots.push(node)
    }
    const seen = new Set<string>()
    const rank = (node: Node): number => {
      if (seen.has(node.hit.id)) return node.rank
      seen.add(node.hit.id)
      node.rank = Math.min(node.rank, ...node.children.map(rank))
      node.children.sort((a, b) => a.rank - b.rank)
      return node.rank
    }
    roots.forEach(rank)
    return roots.sort((a, b) => a.rank - b.rank)
  })

  const rows = $derived.by(() => {
    const out: { node: Node; depth: number }[] = []
    const seen = new Set<string>()
    const walk = (node: Node, depth: number) => {
      if (seen.has(node.hit.id)) return
      seen.add(node.hit.id)
      out.push({ node, depth })
      if (!collapsed.has(node.hit.id)) node.children.forEach((c) => walk(c, depth + 1))
    }
    tree.forEach((n) => walk(n, 0))
    return out
  })

  function toggle(id: string) {
    const next = new Set(collapsed)
    if (!next.delete(id)) next.add(id)
    collapsed = next
    if (picked && !rows.some((r) => r.node.hit.id === picked!.id)) picked = null
  }

  onMount(search)

  async function search() {
    const mine = ++seq
    searching = true
    error = null
    try {
      const result = await api.searchIssues(query)
      if (mine !== seq) return
      hits = result
      collapsed = new Set()
      if (picked && !hits.some((h) => h.id === picked!.id)) picked = null
      const matches = hits.filter((h) => h.matched)
      if (matches.length === 1 && !matches[0].item && query.trim()) picked = matches[0]
    } catch (e) {
      if (mine === seq) error = String(e)
    } finally {
      if (mine === seq) searching = false
    }
  }

  function onInput() {
    clearTimeout(timer)
    timer = setTimeout(search, 350)
  }

  async function add() {
    if (!picked) return
    adding = true
    error = null
    try {
      const outcome = await api.addIssueItem(picked.url, needsProject && choice ? choice : { kind: 'infer' })
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
  title="Add an issue"
  subtitle={needsProject ? undefined : 'Your open assigned issues, or search GitHub for any issue you can see.'}
  busy={adding}
  wide
  {onClose}
>
  {#if needsProject}
    <ProjectChoice repo={needsProject} disabled={adding} bind:choice onError={(e) => (error = e)} />
  {:else}
    <input
      bind:value={query}
      oninput={onInput}
      placeholder="Search titles, #123, owner/repo#123, or paste an issue URL"
      aria-label="Search issues"
    />
    <div class="grid">
      <table>
        <thead>
          <tr><th>Repository</th><th>#</th><th>Type</th><th>Title</th><th>State</th><th>Assignees</th></tr>
        </thead>
        <tbody>
          {#each rows as { node, depth } (node.hit.id)}
            {@const h = node.hit}
            <tr
              class="row"
              class:picked={picked?.id === h.id}
              class:listed={!!h.item}
              class:muted={!h.matched}
              title={h.item ? `Already on the list as ${h.item}` : undefined}
              onclick={() => !h.item && (picked = h)}
              ondblclick={() => !h.item && ((picked = h), add())}
            >
              <td>{h.repo}</td>
              <td>{h.number}</td>
              <td><span class="tag {typeClass(h.issue_type)}">{h.issue_type ?? 'Issue'}</span></td>
              <td class="title">
                <span class="tree" style:padding-left="{depth * 16}px">
                  {#if node.children.length}
                    <button
                      class="toggle"
                      aria-label={collapsed.has(h.id) ? 'Expand' : 'Collapse'}
                      onclick={(e) => (e.stopPropagation(), toggle(h.id))}
                      ondblclick={(e) => e.stopPropagation()}
                    >{collapsed.has(h.id) ? '▸' : '▾'}</button>
                  {:else}
                    <span class="toggle"></span>
                  {/if}
                  <span>{h.title}</span>
                </span>
              </td>
              <td>{h.state.toLowerCase()}</td>
              <td>{h.assignees.join(', ')}</td>
            </tr>
          {:else}
            <tr>
              <td colspan="6" class="muted note">
                {searching
                  ? 'Searching…'
                  : query.trim()
                    ? 'No matching issues. A bare number is looked up only in repositories your projects know.'
                    : 'No open issues assigned to you that aren’t on the list already.'}
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
    <button class="primary" disabled={!picked || adding || (!!needsProject && !choice)} onclick={add}>
      {adding ? 'Adding…' : 'Add to list'}
    </button>
  {/snippet}
</Dialog>

<style>
  .spacer {
    flex: 1;
  }
  .tree {
    display: flex;
    align-items: baseline;
    gap: 4px;
  }
  .toggle {
    flex: none;
    width: 16px;
    padding: 0;
    border: none;
    background: none;
    color: inherit;
    cursor: pointer;
  }
</style>
