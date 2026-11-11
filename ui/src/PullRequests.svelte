<script lang="ts">
  import { onMount, tick } from 'svelte'
  import { openUrl } from '@tauri-apps/plugin-opener'
  import { api, type BranchWork, type PrDetail, type Verdict, type Worker, type WorkSubject } from './api'
  import { formatAge } from './issue'
  import { addressReviewPrompt, askBlocker, fixCiPrompt, openPrPrompt, withoutPr } from './prs'
  import { pasteIntoSession } from './terminals'
  import ActionMenu, { type MenuItem } from './ActionMenu.svelte'

  interface Props {
    work: BranchWork[] | null
    loading: boolean
    /** When `work` was last loaded, in ms since the epoch. */
    checkedAt: number | null
    subject: WorkSubject
    workers: Worker[]
    exited: Record<string, boolean>
    /** `fetch` is true only for the Refresh button, which fetches the repos. */
    onRefresh: (fetch: boolean) => void
    onOpenWorker: (worker: Worker) => void
    onError: (message: string) => void
  }

  let { work, loading, checkedAt, subject, workers, exited, onRefresh, onOpenWorker, onError }: Props = $props()

  let now = $state(Date.now())
  onMount(() => {
    const timer = setInterval(() => (now = Date.now()), 30_000)
    return () => clearInterval(timer)
  })

  const VERDICTS: Record<Verdict, { label: string; tone: string }> = {
    needs_attention: { label: 'Needs attention', tone: 'bad' },
    draft: { label: 'Draft', tone: 'muted' },
    ready: { label: 'Ready to merge', tone: 'ok' },
    in_review: { label: 'In review', tone: 'info' },
  }

  const workerById = $derived(new Map(workers.map((w) => [w.id, w])))
  const open = $derived((work ?? []).filter((b) => b.verdict && b.pull_requests.length))
  const withoutPrs = $derived(
    (work ?? []).flatMap((b) => {
      const state = withoutPr(b)
      return state ? [{ b, state }] : []
    }),
  )
  const waiting = $derived(withoutPrs.filter((w) => w.state === 'waiting').map((w) => w.b))
  const quietBranches = $derived(withoutPrs.filter((w) => w.state !== 'waiting'))
  const finished = $derived(
    (work ?? []).flatMap((b) => b.pull_requests.filter((pr) => pr.state !== 'OPEN').map((pr) => ({ b, pr }))),
  )
  const merged = $derived(finished.filter((f) => f.pr.state === 'MERGED').length)
  const closed = $derived(finished.length - merged)

  function ago(ms: number): string {
    return `${formatAge(Math.max(0, Math.round((now - ms) / 1000)))} ago`
  }

  /** Branches outside the project's repos are labelled owner/repo. */
  function inProject(b: BranchWork): boolean {
    return !b.repo.includes('/')
  }

  function askTarget(b: BranchWork): Worker | null {
    return (inProject(b) && b.worker && workerById.get(b.worker)) || null
  }

  async function ask(w: Worker, prompt: string) {
    onOpenWorker(w)
    await tick()
    if (!(await pasteIntoSession(w.id, prompt))) {
      onError(`${w.id}'s terminal didn't attach, so the prompt wasn't pasted. Try again once it shows Claude.`)
    }
  }

  function hasConflicts(pr: PrDetail): boolean {
    return pr.mergeable === 'CONFLICTING' || pr.merge_state === 'DIRTY'
  }

  function branchUrl(b: BranchWork): string | null {
    return b.remote && b.name_with_owner ? `https://github.com/${b.name_with_owner}/tree/${b.branch}` : null
  }

  function deleteBlocker(b: BranchWork): string | false {
    if (!inProject(b)) return 'Not in one of this project’s repos'
    if (!b.local && !b.remote) return 'The branch no longer exists'
    const openPr = b.pull_requests.find((p) => p.state === 'OPEN')
    if (openPr) return `PR #${openPr.number} is open on this branch; deleting it would close the PR`
    if (b.worker) return `Checked out in worker ${b.worker}`
    return false
  }

  async function remove(b: BranchWork) {
    const where = [b.remote ? `origin/${b.branch} on GitHub` : null, b.local ? 'the local branch' : null].filter(Boolean)
    const wasMerged = b.pull_requests.some((p) => p.state === 'MERGED')
    const risks = [
      !wasMerged && b.ahead ? `${b.ahead} commit(s) aren’t on the base branch and no merged PR contains them.` : null,
      b.unpushed ? `${b.unpushed} local commit(s) were never pushed.` : null,
      b.local && !b.remote ? 'The branch was never pushed, so this is the only copy.' : null,
    ].filter(Boolean)
    const message = [`Delete ${b.branch} in ${b.repo}?`, `This removes ${where.join(' and ')}.`, ...risks].join('\n\n')
    if (!confirm(message)) return
    try {
      await api.deleteBranch(subject, b.repo, b.branch)
      onRefresh(false)
    } catch (e) {
      onError(String(e))
    }
  }

  function actions(b: BranchWork): MenuItem[] {
    return [{ label: 'Delete branch…', danger: true, disabled: deleteBlocker(b), onSelect: () => remove(b) }]
  }
</script>

{#snippet askButton(b: BranchWork, label: string, prompt: string)}
  {@const w = askTarget(b)}
  {#if w}
    {@const blocker = askBlocker(w, !!exited[w.id])}
    <button class="small" disabled={!!blocker} title={blocker || `Paste the prompt into ${w.id}'s session`} onclick={() => ask(w, prompt)}
      >{label}</button
    >
  {/if}
{/snippet}

<section class="prs">
  <div class="head">
    <h2>Pull requests &amp; branches</h2>
    {#if checkedAt !== null}<span class="muted checked">checked {ago(checkedAt)}</span>{/if}
    <span class="spacer"></span>
    <button class="small" disabled={loading} title="Fetch the repos and refresh" onclick={() => onRefresh(true)}
      >{loading ? 'Loading…' : 'Refresh'}</button
    >
  </div>

  {#if work === null}
    <p class="muted empty-line">Looking for branches and PRs…</p>
  {:else if work.length === 0}
    <div class="panel empty">
      <strong>No pull requests or branches yet</strong>
      <div class="muted">
        {#if subject.kind === 'issue'}
          PRs linked to this issue or its subtasks, and branches named after them or checked out in their workers, show up
          here.
        {:else}
          This item's pull request and the branches in its worker's worktrees show up here.
        {/if}
        A branch Claude pushes appears as <span class="warn">Waiting for PR</span>.
      </div>
    </div>
  {:else}
    {#each open as b (b.repo + '/' + b.branch)}
      {@const pr = b.pull_requests[0]}
      {@const verdict = VERDICTS[b.verdict!]}
      <div class="panel card" class:attention={b.verdict === 'needs_attention'}>
        <div class="top">
          <div class="main">
            <div class="title-line">
              <button class="link title" onclick={() => openUrl(pr.url)}>{pr.title}</button>
              <button class="link muted" onclick={() => openUrl(pr.url)}>{b.repo}#{pr.number} ↗</button>
            </div>
            <div class="muted meta">
              <code>{pr.head}</code> → <code>{pr.base}</code>
              {#if b.behind !== null}· {b.behind ? `${b.behind} behind ${pr.base}` : 'up to date'}{/if}
              {#if pr.updated_at}· updated {ago(Date.parse(pr.updated_at))}{/if}
            </div>
          </div>
          <span class="pill {verdict.tone}">{verdict.label}</span>
        </div>
        <div class="pills">
          {#each pr.checks.failing as c (c.name)}
            {#if c.url}
              {@const url = c.url}
              <button class="pill bad link-pill" title="Open the run" onclick={() => openUrl(url)}>✕ {c.name} ↗</button>
            {:else}
              <span class="pill bad">✕ {c.name}</span>
            {/if}
          {/each}
          {#if !pr.checks.failing.length && pr.checks.pending}
            <span class="pill muted">{pr.checks.pending} check{pr.checks.pending === 1 ? '' : 's'} running</span>
          {:else if !pr.checks.failing.length && pr.checks.state === 'SUCCESS'}
            <span class="pill ok">✓ CI passing</span>
          {/if}
          {#if pr.changes_requested_by.length}
            <span class="pill bad">Changes requested · {pr.changes_requested_by.join(', ')}</span>
          {/if}
          {#if hasConflicts(pr)}<span class="pill bad">Conflicts with {pr.base}</span>{/if}
          {#if pr.merge_state === 'BEHIND'}<span class="pill warn">Behind {pr.base}</span>{/if}
          {#if pr.unresolved_threads}
            <span class="pill warn">{pr.unresolved_threads} unresolved thread{pr.unresolved_threads === 1 ? '' : 's'}</span>
          {/if}
          {#if pr.approved_by.length}<span class="pill ok">Approved · {pr.approved_by.join(', ')}</span>{/if}
          {#if pr.review_requests && !pr.approved_by.length}
            <span class="pill muted">Awaiting review</span>
          {/if}
          {#if b.unpushed}
            <span class="pill muted">{b.unpushed} local commit{b.unpushed === 1 ? '' : 's'} not pushed</span>
          {/if}
        </div>
        {#if askTarget(b) && (pr.checks.failing.length || pr.changes_requested_by.length || pr.unresolved_threads)}
          <div class="asks">
            {#if pr.checks.failing.length}{@render askButton(b, 'Ask Claude to fix CI', fixCiPrompt(b, pr))}{/if}
            {#if pr.changes_requested_by.length || pr.unresolved_threads}
              {@render askButton(b, 'Ask Claude to address review', addressReviewPrompt(b, pr))}
            {/if}
          </div>
        {/if}
      </div>
    {/each}

    {#each waiting as b (b.repo + '/' + b.branch)}
      {@const url = branchUrl(b)}
      <div class="panel card waiting">
        <div class="main">
          {#if url}
            <button class="link branch" onclick={() => openUrl(url)}>{b.branch}</button>
          {:else}
            <code class="branch">{b.branch}</code>
          {/if}
          <span class="muted">· {b.repo}</span>
          <div class="muted meta">
            {[
              b.ahead !== null ? `${b.ahead} ahead` : null,
              b.remote ? 'pushed' : 'local only (not on origin)',
              b.unpushed ? `${b.unpushed} unpushed` : null,
              b.last_commit ? `last commit ${b.last_commit}` : null,
            ]
              .filter(Boolean)
              .join(' · ')}
          </div>
        </div>
        <span class="pill warn">Waiting for PR</span>
        {@render askButton(b, 'Ask Claude to open PR', openPrPrompt(b))}
        <ActionMenu items={actions(b)} label="Actions for {b.branch}" />
      </div>
    {/each}

    {#each quietBranches as { b, state } (b.repo + '/' + b.branch)}
      <div class="panel card quiet">
        <code class="branch">{b.branch}</code>
        <span class="muted">· {b.repo}</span>
        <span class="spacer"></span>
        <span class="pill muted">{state === 'deleted' ? 'Deleted' : 'No commits yet'}</span>
        {#if state !== 'deleted'}<ActionMenu items={actions(b)} label="Actions for {b.branch}" />{/if}
      </div>
    {/each}

    {#if finished.length}
      <details class="panel">
        <summary class="muted">
          <span class="chev">▸</span>
          {[merged ? `${merged} merged` : null, closed ? `${closed} closed` : null].filter(Boolean).join(' · ')}
        </summary>
        <div class="folded">
          {#each finished as { b, pr } (pr.repo + '#' + pr.number)}
            <div class="row">
              <span class="pill muted">{pr.state === 'MERGED' ? 'Merged' : 'Closed'}</span>
              <button class="link title-small" title={pr.title} onclick={() => openUrl(pr.url)}>{pr.title}</button>
              <span class="muted"
                >{b.repo}#{pr.number}{pr.updated_at ? ` · ${ago(Date.parse(pr.updated_at))}` : ''}</span
              >
              <span class="spacer"></span>
              {#if b.local || b.remote}<ActionMenu items={actions(b)} label="Actions for {b.branch}" />{/if}
            </div>
          {/each}
        </div>
      </details>
    {/if}
  {/if}
</section>

<style>
  .prs {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  h2 {
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--muted);
    margin: 0;
    font-weight: 600;
  }
  .checked {
    font-size: 12px;
  }
  .spacer {
    flex: 1;
  }
  button.small {
    padding: 1px 8px;
    font-size: 12px;
  }
  .empty-line {
    margin: 0;
    font-size: 12px;
  }
  .panel {
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
  }
  .empty {
    border-style: dashed;
    padding: 32px 24px;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    text-align: center;
  }
  .empty .muted {
    max-width: 420px;
  }
  .warn {
    color: var(--warn);
  }
  .card {
    padding: 12px 14px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .card.attention {
    border-color: var(--bad);
  }
  .card.waiting {
    flex-direction: row;
    flex-wrap: wrap;
    align-items: center;
    border-style: dashed;
    padding: 10px 14px;
  }
  .card.quiet {
    flex-direction: row;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    border-style: dashed;
    padding: 6px 14px;
  }
  .top {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    align-items: flex-start;
  }
  .main {
    flex: 1 1 300px;
    min-width: 0;
  }
  .title-line {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    align-items: center;
  }
  .title {
    color: var(--text);
    font-weight: 600;
    font-size: 14px;
  }
  .title:hover {
    text-decoration: underline;
  }
  .meta {
    font-size: 12px;
    margin-top: 2px;
  }
  .branch {
    font-family: var(--mono);
    font-size: 12px;
    word-break: break-all;
  }
  .pills,
  .asks {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 8px;
  }
  .pill {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    border-radius: 999px;
    padding: 1px 9px;
    font-size: 12px;
    font-weight: 600;
    background: var(--panel-2);
    border: 1px solid var(--border);
    white-space: nowrap;
  }
  .pill.ok {
    color: var(--ok);
    border-color: var(--ok);
  }
  .pill.bad {
    color: var(--bad);
    border-color: var(--bad);
  }
  .pill.warn {
    color: var(--warn);
    border-color: var(--warn);
  }
  .pill.info {
    color: var(--accent);
    border-color: var(--accent);
  }
  .pill.muted {
    color: var(--muted);
  }
  button.link-pill:hover {
    text-decoration: underline;
  }
  summary {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    cursor: pointer;
    list-style: none;
  }
  summary::-webkit-details-marker {
    display: none;
  }
  .chev {
    display: inline-block;
    transition: transform 0.15s;
  }
  details[open] .chev {
    transform: rotate(90deg);
  }
  .folded {
    padding: 0 12px 10px 30px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .title-small {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
