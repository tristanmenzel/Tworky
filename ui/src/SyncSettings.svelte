<script lang="ts">
  import { untrack } from 'svelte'
  import { api, type ConfigRepoStatus, type ConfigSyncRecord } from './api'
  import { ask } from './confirm.svelte'
  import { formatSyncTime, syncProblem } from './configSync'

  interface Props {
    visible: boolean
    sync: ConfigSyncRecord | null
    onSync: (record: ConfigSyncRecord) => void
    onResolve: () => void
    onShowRemote: () => void
    onError: (message: string) => void
  }

  let { visible, sync, onSync, onResolve, onShowRemote, onError }: Props = $props()

  let repo = $state<ConfigRepoStatus | null>(null)
  let busy = $state<'sync' | 'abort' | 'dismiss' | null>(null)

  const syncState = $derived(sync?.state ?? null)
  const problem = $derived(syncState ? syncProblem(syncState) : null)
  const files = $derived(syncState && 'files' in syncState ? syncState.files : [])
  const incoming = $derived(sync?.incoming)
  const hasIncoming = $derived(
    !!incoming && !!(incoming.hooks_or_services.length || incoming.guidance.length || incoming.dropped.length),
  )

  $effect(() => {
    void sync
    if (visible) untrack(loadRepo)
  })

  async function loadRepo() {
    try {
      repo = await api.configRepoStatus()
    } catch (e) {
      onError(String(e))
    }
  }

  async function run(kind: 'sync' | 'abort' | 'dismiss', action: () => Promise<void>) {
    busy = kind
    try {
      await action()
    } catch (e) {
      onError(String(e))
    } finally {
      busy = null
    }
  }

  const syncNow = () => run('sync', async () => onSync(await api.syncConfigNow()))

  async function abort() {
    const ok = await ask({
      title: 'Abort this sync?',
      message:
        "Tworky stops any rebase or merge in the sync worktree, dropping what was resolved there, and pauses background syncs. This machine's config isn't changed. Sync now then starts over, discarding anything else left uncommitted in the sync worktree.",
      confirmLabel: 'Abort',
      danger: true,
    })
    if (ok) await run('abort', async () => onSync(await api.abortConfigSync()))
  }

  const dismiss = () =>
    run('dismiss', async () => {
      await api.dismissConfigIncoming()
      onSync(await api.configSyncStatus())
    })
</script>

<div class="sync">
  <header>
    <h1>Sync</h1>
    <span class="spacer"></span>
    {#if syncState && syncState.kind !== 'no_remote'}
      <button class="primary" disabled={!!busy || syncState.kind === 'syncing'} onclick={syncNow}
        >{busy === 'sync' || syncState.kind === 'syncing' ? 'Syncing…' : 'Sync now'}</button
      >
    {/if}
  </header>

  {#if !syncState}
    <p class="muted">Loading…</p>
  {:else if syncState.kind === 'no_remote'}
    <p class="muted">
      Your config isn't syncing with a remote. <button class="link" onclick={onShowRemote}>Set one up</button> to share it
      between machines.
    </p>
  {:else}
    <dl>
      <dt>State</dt>
      <dd>
        {#if problem}<span class="bad">{problem}</span>
        {:else if syncState.kind === 'syncing'}Syncing…
        {:else if sync?.paused}<span class="warn">Paused: you aborted the last sync</span>
        {:else}Up to date{/if}
      </dd>
      <dt>Last sync</dt>
      <dd>{formatSyncTime(sync?.last_sync ?? null)}</dd>
      {#if repo?.ahead_behind && (repo.ahead_behind[0] || repo.ahead_behind[1])}
        <dt>This machine</dt>
        <dd>{repo.ahead_behind[0]} commits to push, {repo.ahead_behind[1]} to pull</dd>
      {/if}
    </dl>
    {#if sync?.paused}
      <p class="warn small">Background syncs are paused since the last abort. Sync now starts over.</p>
    {/if}

    {#if syncState.kind === 'conflict' || syncState.kind === 'held_back' || syncState.kind === 'held_in_sync_worktree'}
      <section class="card problem">
        {#if syncState.kind === 'conflict'}
          <h2>Conflict</h2>
          <p class="small">
            Changes from the remote and from this machine clash{sync?.operation
              ? ` while ${sync.operation === 'rebase' ? 'rebasing' : 'merging'}`
              : ''}. Your config here keeps working with this machine's version until it's resolved, and new changes
            still commit.
          </p>
        {:else if syncState.kind === 'held_back'}
          <h2>Held back</h2>
          {#if syncState.cause === 'unrecognised'}
            <p class="small">
              These project configs are valid, but not config this Tworky understands. They were probably written by a
              newer Tworky on another machine: update Tworky here, then sync. Resolve with Claude only if they hold a
              genuine mistake.
            </p>
          {:else}
            <p class="small">
              These project configs from the remote don't parse, so this machine keeps its own until they're fixed.
            </p>
          {/if}
        {:else}
          <h2>On hold</h2>
          <p class="small">
            The sync worktree has uncommitted changes, probably a fix in progress. Committing them resumes syncing. To
            give them up, abort, then Sync now, which discards them.
          </p>
        {/if}
        <ul class="files">
          {#each files as file (file)}<li><code>{file}</code></li>{/each}
        </ul>
        <div class="actions">
          <button class="danger" disabled={!!busy} onclick={abort}>{busy === 'abort' ? 'Aborting…' : 'Abort'}</button>
          <button class="primary" disabled={!!busy} onclick={onResolve}>Resolve with Claude</button>
        </div>
      </section>
    {:else if syncState.kind === 'blocked_by_local'}
      <section class="card problem">
        <h2>Blocked by local files</h2>
        <p class="small">
          These files have changes on this machine that aren't committed, and the remote changed them too. Fix them so
          they commit (a project.toml must parse), or undo the edits, then sync.
        </p>
        <ul class="files">
          {#each files as file (file)}<li><code>{file}</code></li>{/each}
        </ul>
      </section>
    {:else if syncState.kind === 'unconfirmed_remote'}
      <section class="card problem">
        <h2>Remote not connected through Tworky</h2>
        <p class="small">
          The config repo's remote, <code>{syncState.url}</code>, was added or changed outside Tworky, so nothing is
          fetched from it or pushed to it. <button class="link" onclick={onShowRemote}>Connect it or remove it</button> in
          Remote.
        </p>
      </section>
    {:else if syncState.kind === 'error'}
      <section class="card problem">
        <h2>Error</h2>
        <pre class="message">{syncState.message}</pre>
      </section>
    {/if}
  {/if}

  {#if repo?.changes.length}
    <section class="card">
      <h2>Not committed</h2>
      <p class="small muted">
        Changed on this machine but not committed yet. A project.toml that doesn't parse stays here until it's fixed.
      </p>
      <ul class="files">
        {#each repo.changes as file (file)}<li><code>{file}</code></li>{/each}
      </ul>
    </section>
  {/if}

  {#if repo?.stale_index_lock}
    <p class="warn small">
      A git command that was stopped left an <code>index.lock</code> behind in the config repo or its sync worktree. Delete
      it if no git is running there.
    </p>
  {/if}

  {#if hasIncoming && incoming}
    <section class="card">
      <header>
        <h2>Incoming changes</h2>
        <span class="spacer"></span>
        <button disabled={!!busy} onclick={dismiss}>Dismiss</button>
      </header>
      <p class="small muted">Synced config runs commands and steers sessions on this machine, so check these.</p>
      {#if incoming.hooks_or_services.length}
        <p class="small">Projects whose hooks or services changed:</p>
        <ul class="files">
          {#each incoming.hooks_or_services as project (project)}<li>{project}</li>{/each}
        </ul>
      {/if}
      {#if incoming.guidance.length}
        <p class="small">Guidance and memory that changed:</p>
        <ul class="files">
          {#each incoming.guidance as file (file)}<li><code>{file}</code></li>{/each}
        </ul>
      {/if}
      {#if incoming.dropped.length}
        <p class="small warn">Changes from this machine that the conflict resolution left out:</p>
        <ul class="files">
          {#each incoming.dropped as subject (subject)}<li>{subject}</li>{/each}
        </ul>
      {/if}
    </section>
  {/if}
</div>

<style>
  .sync {
    padding: 16px 24px 32px;
    max-width: 760px;
    display: flex;
    flex-direction: column;
    gap: 12px;
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
  h2 {
    font-size: 14px;
    font-weight: 600;
    margin: 0;
  }
  .spacer {
    flex: 1;
  }
  .small {
    font-size: 12px;
    margin: 0;
  }
  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 4px 16px;
    margin: 0;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  .card {
    display: flex;
    flex-direction: column;
    gap: 8px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 12px 16px;
  }
  .card.problem {
    border-left: 3px solid var(--warn);
  }
  .files {
    margin: 0;
    padding-left: 18px;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  .message {
    margin: 0;
    white-space: pre-wrap;
    color: var(--bad);
  }
  .warn {
    color: var(--warn);
  }
  .bad {
    color: var(--bad);
  }
</style>
